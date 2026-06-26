use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agent_surface_eval::{
    EvaluationReport, Fixture, TrialReport, catalog_measurement, fixtures, scoring::LoggedCall,
    validate_catalog_contract,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use wait_timeout::ChildExt;

const EVIDENCE_DIR: &str = "docs/rfcs/evidence/00005-agent-interaction-surface";

#[derive(Debug)]
pub struct AgentEvalArgs {
    auth_source: PathBuf,
    codex: PathBuf,
    model: String,
    reasoning_effort: String,
    fixture: Option<String>,
    resume: Option<PathBuf>,
}

impl AgentEvalArgs {
    pub fn parse(args: Vec<String>) -> Result<Self> {
        let mut auth_source = None;
        let mut codex = PathBuf::from("codex");
        let mut model = "gpt-5.5".to_string();
        let mut reasoning_effort = "medium".to_string();
        let mut fixture = None;
        let mut resume = None;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--auth-source" => {
                    index += 1;
                    auth_source = Some(PathBuf::from(
                        args.get(index)
                            .context("missing value after --auth-source")?,
                    ));
                }
                "--codex" => {
                    index += 1;
                    codex = PathBuf::from(args.get(index).context("missing value after --codex")?);
                }
                "--model" => {
                    index += 1;
                    model = args
                        .get(index)
                        .context("missing value after --model")?
                        .clone();
                }
                "--reasoning-effort" => {
                    index += 1;
                    reasoning_effort = args
                        .get(index)
                        .context("missing value after --reasoning-effort")?
                        .clone();
                }
                "--fixture" => {
                    index += 1;
                    fixture = Some(
                        args.get(index)
                            .context("missing value after --fixture")?
                            .clone(),
                    );
                }
                "--resume" => {
                    index += 1;
                    resume = Some(PathBuf::from(
                        args.get(index).context("missing value after --resume")?,
                    ));
                }
                argument => bail!("unknown agent-eval argument `{argument}`"),
            }
            index += 1;
        }
        let auth_source = auth_source.context("agent-eval requires --auth-source <path>")?;
        if !matches!(
            reasoning_effort.as_str(),
            "minimal" | "low" | "medium" | "high" | "xhigh"
        ) {
            bail!("unsupported reasoning effort `{reasoning_effort}`");
        }
        Ok(Self {
            auth_source,
            codex,
            model,
            reasoning_effort,
            fixture,
            resume,
        })
    }
}

pub fn validate_catalog() -> Result<()> {
    validate_catalog_contract()
}

pub fn catalog_measurement_command(root: &Path) -> Result<()> {
    let measurement = catalog_measurement()?;
    let evidence = root.join(EVIDENCE_DIR).join("catalog-measurement.json");
    fs::create_dir_all(
        evidence
            .parent()
            .context("catalog evidence path omitted parent")?,
    )?;
    fs::write(&evidence, serde_json::to_vec_pretty(&measurement)?)?;
    println!(
        "hybrid={} baseline={} tokens={}/{} ratio={:.4} passes={}",
        measurement.hybrid_tools,
        measurement.baseline_tools,
        measurement.hybrid_tokens,
        measurement.baseline_tokens,
        measurement.ratio,
        measurement.passes
    );
    println!("wrote {}", evidence.display());
    if !measurement.passes {
        bail!("catalog measurement failed its 0.60 acceptance threshold");
    }
    Ok(())
}

pub fn agent_eval_command(root: &Path, args: AgentEvalArgs) -> Result<()> {
    let auth_source = resolve_auth_source(&args.auth_source)?;
    let selected = select_fixtures(args.fixture.as_deref())?;
    build_evaluation_server(root)?;
    let run_root = match args.resume.as_deref() {
        Some(path) => path
            .canonicalize()
            .with_context(|| format!("failed to resolve evaluation run `{}`", path.display()))?,
        None => root.join("target/agent-surface-evaluation").join(format!(
            "{}-{}",
            unix_seconds()?,
            std::process::id()
        )),
    };
    fs::create_dir_all(&run_root)?;
    let mut reports = Vec::new();
    for (index, fixture) in selected.iter().enumerate() {
        println!("trial {}/{}: {}", index + 1, selected.len(), fixture.id);
        let fixture_root = run_root.join(&fixture.id);
        let report = match load_existing_trial(&fixture_root, fixture, &args)? {
            Some(report) => {
                println!("  resumed scored trial");
                report
            }
            None => {
                run_with_infrastructure_retry(root, &fixture_root, fixture, &args, &auth_source)?
            }
        };
        fs::create_dir_all(&fixture_root)?;
        fs::write(
            fixture_root.join("trial-report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        println!(
            "  success={} first_selection={} tools={}",
            report.success,
            report.correct_first_selection,
            report.tool_sequence.join(" -> ")
        );
        reports.push(report);
    }
    let report = EvaluationReport::from_trials(&args.model, &args.reasoning_effort, reports);
    fs::write(
        run_root.join("summary.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    fs::write(run_root.join("summary.md"), report.markdown())?;
    if args.fixture.is_none() {
        let evidence = root.join(EVIDENCE_DIR);
        fs::create_dir_all(&evidence)?;
        fs::write(
            evidence.join("agent-evaluation-summary.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        fs::write(
            evidence.join("agent-evaluation-summary.md"),
            report.markdown(),
        )?;
    }
    println!(
        "evaluation: success={}/{} first={}/{} fallback_violations={} foreign_backend_actions={} passes={}",
        report.successful_trials,
        report.total_trials,
        report.correct_first_selections,
        report.total_trials,
        report.semantic_fallback_violations,
        report.unowned_backend_actions,
        report.passes
    );
    println!("raw evaluation artifacts: {}", run_root.display());
    if args.fixture.is_none() && !report.passes {
        bail!("agent evaluation did not meet the Stage 2 acceptance thresholds");
    }
    if args.fixture.is_some() && report.successful_trials != report.total_trials {
        bail!("selected fixture failed");
    }
    Ok(())
}

fn select_fixtures(selected: Option<&str>) -> Result<Vec<Fixture>> {
    let fixtures = fixtures();
    match selected {
        Some(id) => fixtures
            .into_iter()
            .find(|fixture| fixture.id == id)
            .map(|fixture| vec![fixture])
            .with_context(|| format!("unknown fixture `{id}`")),
        None => Ok(fixtures),
    }
}

fn build_evaluation_server(root: &Path) -> Result<()> {
    let status = Command::new("cargo")
        .args([
            "build",
            "-p",
            "agent-surface-eval",
            "--bin",
            "visible-browser-lab-eval-mcp",
        ])
        .current_dir(root)
        .status()
        .context("failed to build evaluation MCP server")?;
    if !status.success() {
        bail!("evaluation MCP server build failed");
    }
    Ok(())
}

fn run_with_infrastructure_retry(
    root: &Path,
    fixture_root: &Path,
    fixture: &Fixture,
    args: &AgentEvalArgs,
    auth_source: &Path,
) -> Result<TrialReport> {
    let mut last_error = None;
    let first_attempt = next_attempt_number(fixture_root)?;
    for attempt in first_attempt..first_attempt + 2 {
        let trial_root = fixture_root.join(format!("attempt-{attempt}"));
        match run_trial(root, &trial_root, fixture, args, auth_source) {
            Ok(report) => return Ok(report),
            Err(error) => {
                eprintln!("  infrastructure attempt {attempt} failed: {error:#}");
                last_error = Some(error);
            }
        }
    }
    Err(last_error.context("trial failed without an infrastructure error")?)
}

fn run_trial(
    root: &Path,
    trial_root: &Path,
    fixture: &Fixture,
    args: &AgentEvalArgs,
    auth_source: &Path,
) -> Result<TrialReport> {
    fs::create_dir_all(trial_root)?;
    let runtime = tempfile::Builder::new()
        .prefix(&format!("vbl-agent-eval-{}-", fixture.id))
        .tempdir()
        .context("failed to create isolated evaluation runtime")?;
    let home = runtime.path().join("home");
    let codex_home = runtime.path().join("codex-home");
    let sqlite_home = runtime.path().join("codex-sqlite-home");
    let workspace = runtime.path().join("workspace");
    let marketplace = runtime.path().join("marketplace");
    let plugin = marketplace.join("plugin");
    for directory in [&home, &codex_home, &sqlite_home, &workspace, &plugin] {
        fs::create_dir_all(directory)?;
    }
    fs::copy(auth_source, codex_home.join("auth.json"))?;
    for cache_name in ["cloud-requirements-cache.json", "models_cache.json"] {
        let source = auth_source
            .parent()
            .context("auth source omitted parent")?
            .join(cache_name);
        if source.is_file() {
            fs::copy(source, codex_home.join(cache_name))?;
        }
    }
    copy_tree(&root.join("agent-surface-eval/plugin"), &plugin)?;
    let binary_name = if cfg!(windows) {
        "visible-browser-lab-eval-mcp.exe"
    } else {
        "visible-browser-lab-eval-mcp"
    };
    fs::create_dir_all(plugin.join("bin"))?;
    fs::copy(
        root.join("target/debug").join(binary_name),
        plugin.join("bin").join(binary_name),
    )?;
    write_marketplace(&marketplace)?;
    let environment = EvaluationEnvironment {
        home,
        codex_home,
        sqlite_home,
        workspace,
    };
    fs::write(
        environment.workspace.join("AGENTS.md"),
        fs::read_to_string(
            root.join("agent-surface-eval/plugin/skills/visible-browser-lab/SKILL.md"),
        )?,
    )?;
    run_checked(
        isolated_codex(&args.codex, &environment)
            .args(["plugin", "marketplace", "add"])
            .arg(&marketplace),
        Duration::from_secs(60),
        "add evaluation marketplace",
        trial_root,
    )?;
    run_checked(
        isolated_codex(&args.codex, &environment).args([
            "plugin",
            "add",
            "visible-browser-lab@visible-browser-lab-evaluation",
        ]),
        Duration::from_secs(60),
        "install evaluation plugin",
        trial_root,
    )?;
    let output_schema = trial_root.join("output-schema.json");
    fs::write(
        &output_schema,
        serde_json::to_vec_pretty(&json!({
            "type":"object",
            "properties":{
                "fixture_id":{"type":"string","const":fixture.id},
                "outcome":{"type":"string","enum":["completed","blocked"]},
                "observations":{"type":"object","properties":{"result":{"type":"string"}},"required":["result"],"additionalProperties":false}
            },
            "required":["fixture_id","outcome","observations"],
            "additionalProperties":false
        }))?,
    )?;
    let final_output = trial_root.join("final.json");
    let events = trial_root.join("codex-events.jsonl");
    let stderr = trial_root.join("codex-stderr.log");
    let calls = trial_root.join("mcp-calls.jsonl");
    let reasoning = format!("model_reasoning_effort=\"{}\"", args.reasoning_effort);
    let mut command = isolated_codex(&args.codex, &environment);
    command
        .args([
            "exec",
            "--ephemeral",
            "--json",
            "--skip-git-repo-check",
            "--ignore-rules",
            "--dangerously-bypass-approvals-and-sandbox",
            "--model",
            &args.model,
            "--config",
            &reasoning,
            "--output-schema",
        ])
        .arg(&output_schema)
        .arg("--output-last-message")
        .arg(&final_output)
        .arg("-C")
        .arg(&environment.workspace)
        .arg(fixture.prompt())
        .env("VISIBLE_BROWSER_LAB_EVAL_FIXTURE", &fixture.id)
        .env("VISIBLE_BROWSER_LAB_EVAL_LOG", &calls);
    run_to_files(
        &mut command,
        &events,
        &stderr,
        Duration::from_secs(600),
        "run Codex evaluation",
    )?;
    let event_text = fs::read_to_string(&events)?;
    let policy_failure = validate_events(&event_text)
        .err()
        .map(|error| error.to_string());
    let calls = read_calls(&calls)?;
    let final_output: Value = serde_json::from_slice(
        &fs::read(&final_output).context("Codex omitted structured final output")?,
    )
    .context("Codex final output was not valid JSON")?;
    let mut report = agent_surface_eval::score_trial(
        fixture,
        &calls,
        &final_output,
        &args.model,
        &args.reasoning_effort,
    );
    if let Some(failure) = policy_failure {
        report.success = false;
        report.failure = Some(failure);
    }
    let _ = fs::remove_file(environment.codex_home.join("auth.json"));
    Ok(report)
}

struct EvaluationEnvironment {
    home: PathBuf,
    codex_home: PathBuf,
    sqlite_home: PathBuf,
    workspace: PathBuf,
}

fn isolated_codex<'a>(codex: &'a Path, environment: &'a EvaluationEnvironment) -> Command {
    let mut command = Command::new(codex);
    command
        .current_dir(&environment.workspace)
        .env("HOME", &environment.home)
        .env("USERPROFILE", &environment.home)
        .env("XDG_CONFIG_HOME", environment.home.join(".config"))
        .env("XDG_CACHE_HOME", environment.home.join(".cache"))
        .env("LOCALAPPDATA", environment.home.join("AppData/Local"))
        .env("APPDATA", environment.home.join("AppData/Roaming"))
        .env("CODEX_HOME", &environment.codex_home)
        .env("CODEX_SQLITE_HOME", &environment.sqlite_home)
        .env_remove("CODEX_MANAGED_CONFIG_PATH");
    command
}

fn run_checked(
    command: &mut Command,
    timeout: Duration,
    operation: &str,
    trial_root: &Path,
) -> Result<()> {
    let stem = operation.replace(' ', "-");
    run_to_files(
        command,
        &trial_root.join(format!("{stem}.stdout")),
        &trial_root.join(format!("{stem}.stderr")),
        timeout,
        operation,
    )
}

fn run_to_files(
    command: &mut Command,
    stdout_path: &Path,
    stderr_path: &Path,
    timeout: Duration,
    operation: &str,
) -> Result<()> {
    let stdout = File::create(stdout_path)?;
    let stderr = File::create(stderr_path)?;
    let mut child = command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| format!("failed to {operation}"))?;
    match child.wait_timeout(timeout)? {
        Some(status) if status.success() => Ok(()),
        Some(status) => bail!(
            "{operation} exited with {status}; stderr={}",
            fs::read_to_string(stderr_path).unwrap_or_default().trim()
        ),
        None => {
            child.kill()?;
            let _ = child.wait();
            bail!("{operation} exceeded {} seconds", timeout.as_secs());
        }
    }
}

fn validate_events(events: &str) -> Result<()> {
    for line in events.lines().filter(|line| !line.trim().is_empty()) {
        let event: Value = serde_json::from_str(line).context("Codex emitted invalid JSONL")?;
        let Some(item) = event.get("item") else {
            continue;
        };
        if item.get("type").and_then(Value::as_str) == Some("command_execution") {
            bail!("Codex used command execution");
        }
        if item.get("type").and_then(Value::as_str) == Some("mcp_tool_call")
            && item.get("server").and_then(Value::as_str) != Some("visible-browser-lab")
        {
            bail!("Codex called a non-evaluation MCP server");
        }
    }
    Ok(())
}

fn read_calls(path: &Path) -> Result<Vec<LoggedCall>> {
    let contents =
        fs::read_to_string(path).context("evaluation MCP server did not produce a call log")?;
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("invalid evaluation call log entry"))
        .collect()
}

fn load_existing_trial(
    fixture_root: &Path,
    fixture: &Fixture,
    args: &AgentEvalArgs,
) -> Result<Option<TrialReport>> {
    let report_path = fixture_root.join("trial-report.json");
    let checkpoint = report_path
        .is_file()
        .then(|| fs::read(&report_path))
        .transpose()?
        .map(|bytes| serde_json::from_slice::<TrialReport>(&bytes))
        .transpose()?;
    if !fixture_root.is_dir() {
        return Ok(None);
    }
    let mut attempts = fs::read_dir(fixture_root)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("attempt-")?
                .parse::<usize>()
                .ok()
                .map(|number| (number, entry.path()))
        })
        .collect::<Vec<_>>();
    attempts.sort_by_key(|(number, _)| std::cmp::Reverse(*number));
    for (_, attempt) in attempts {
        let final_path = attempt.join("final.json");
        let calls_path = attempt.join("mcp-calls.jsonl");
        let events_path = attempt.join("codex-events.jsonl");
        if !(final_path.is_file() && calls_path.is_file() && events_path.is_file()) {
            continue;
        }
        let final_output: Value = serde_json::from_slice(&fs::read(final_path)?)?;
        let calls = read_calls(&calls_path)?;
        let mut report = agent_surface_eval::score_trial(
            fixture,
            &calls,
            &final_output,
            &args.model,
            &args.reasoning_effort,
        );
        if let Err(error) = validate_events(&fs::read_to_string(events_path)?) {
            report.success = false;
            report.failure = Some(error.to_string());
        }
        return Ok(Some(report));
    }
    Ok(checkpoint)
}

fn next_attempt_number(fixture_root: &Path) -> Result<usize> {
    if !fixture_root.is_dir() {
        return Ok(1);
    }
    let maximum = fs::read_dir(fixture_root)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("attempt-")?
                .parse::<usize>()
                .ok()
        })
        .max()
        .unwrap_or(0);
    Ok(maximum + 1)
}

fn write_marketplace(root: &Path) -> Result<()> {
    let path = root.join(".agents/plugins/marketplace.json");
    fs::create_dir_all(
        path.parent()
            .context("marketplace manifest omitted parent")?,
    )?;
    fs::write(
        path,
        serde_json::to_vec_pretty(&json!({
            "name":"visible-browser-lab-evaluation",
            "interface":{"displayName":"Visible Browser Lab Evaluation"},
            "plugins":[{"name":"visible-browser-lab","source":{"source":"local","path":"./plugin"},"policy":{"installation":"AVAILABLE","authentication":"ON_INSTALL"},"category":"Developer Tools"}]
        }))?,
    )?;
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let output = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&output)?;
            copy_tree(&entry.path(), &output)?;
        } else {
            fs::copy(entry.path(), output)?;
        }
    }
    Ok(())
}

fn resolve_auth_source(path: &Path) -> Result<PathBuf> {
    let path = if path.is_dir() {
        path.join("auth.json")
    } else {
        path.to_path_buf()
    };
    if !path.is_file() {
        bail!("auth source does not contain `{}`", path.display());
    }
    path.canonicalize().context("failed to resolve auth source")
}

fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_agent_eval_arguments() {
        let args = AgentEvalArgs::parse(vec![
            "--auth-source".into(),
            "/tmp/auth.json".into(),
            "--model".into(),
            "gpt-5.5".into(),
            "--reasoning-effort".into(),
            "medium".into(),
        ])
        .unwrap();
        assert_eq!(args.model, "gpt-5.5");
        assert_eq!(args.reasoning_effort, "medium");
        assert!(args.resume.is_none());
    }
}
