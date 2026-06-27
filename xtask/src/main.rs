use std::{
    env,
    fs::{self, File},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use semver::Version;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zip::{
    CompressionMethod, ZipArchive, ZipWriter,
    write::{ExtendedFileOptions, FileOptions},
};

mod agent_eval;

const BINARY_NAME: &str = "visible-browser-lab-mcp";
const DEFAULT_OUT_DIR: &str = "out/packages";
const RELEASE_VERSION_ENV: &str = "VISIBLE_BROWSER_LAB_RELEASE_VERSION";
const RUNTIME_ENV_VARS: &[&str] = &[
    "VISIBLE_BROWSER_LAB_STATE_DIR",
    "VISIBLE_BROWSER_LAB_CHROME_PATH",
    "VISIBLE_BROWSER_CDP_ENDPOINT",
    "VISIBLE_BROWSER_CDP_PORT",
];
const SUPPORTED_TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

#[derive(Clone, Copy)]
struct AgentHost {
    id: &'static str,
    display_name: &'static str,
    manifest_path: &'static str,
    plugin_format: PluginFormat,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PluginFormat {
    Codex,
    Claude,
}

const AGENT_HOSTS: &[AgentHost] = &[
    AgentHost {
        id: "codex",
        display_name: "Codex",
        manifest_path: ".codex-plugin/plugin.json",
        plugin_format: PluginFormat::Codex,
    },
    AgentHost {
        id: "claude-code",
        display_name: "Claude Code",
        manifest_path: ".claude-plugin/plugin.json",
        plugin_format: PluginFormat::Claude,
    },
    AgentHost {
        id: "vscode",
        display_name: "VS Code",
        manifest_path: ".claude-plugin/plugin.json",
        plugin_format: PluginFormat::Claude,
    },
];

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        print_usage();
        return Ok(());
    };

    match command.as_str() {
        "validate" => validate(),
        "package" => package(PackageArgs::parse(args.collect())?),
        "checksums" => checksums(ChecksumsArgs::parse(args.collect())?),
        "live-smoke" => live_smoke(LiveSmokeArgs::parse(args.collect())?),
        "install-smoke" => install_smoke(InstallSmokeArgs::parse(args.collect())?),
        "catalog-measurement" => agent_eval::catalog_measurement_command(&repo_root()?),
        "agent-eval" => agent_eval::agent_eval_command(
            &repo_root()?,
            agent_eval::AgentEvalArgs::parse(args.collect())?,
        ),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        command => bail!("unknown xtask command `{command}`"),
    }
}

fn print_usage() {
    eprintln!(
        "\
usage:
  cargo xtask validate
  cargo xtask package [--target <target>] [--binary <path>] [--out-dir <dir>] [--version <semver>]
  cargo xtask checksums [--dir <dir>]
  cargo xtask live-smoke [--cdp-endpoint <url>] [--binary <path>] [--state-dir <dir>] [--allow-focus]
      Omitting --cdp-endpoint exercises managed Chrome mode.
      Omitting --allow-focus keeps native input checks on the focus_required path.
  cargo xtask install-smoke [--archive <path>] [--codex <path>] [--chrome-path <path>] [--invoke-codex] [--auth-source <path>] [--keep-temp]
  cargo xtask catalog-measurement
  cargo xtask agent-eval --auth-source <path> [--codex <path>] [--model <model>] [--reasoning-effort <effort>] [--fixture <id>] [--resume <run-dir>]
"
    );
}

#[derive(Debug)]
struct PackageArgs {
    target: String,
    binary: Option<PathBuf>,
    out_dir: PathBuf,
    version: Option<String>,
}

impl PackageArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut target = None;
        let mut binary = None;
        let mut out_dir = PathBuf::from(DEFAULT_OUT_DIR);
        let mut version = None;
        let mut index = 0;

        while index < args.len() {
            match args[index].as_str() {
                "--target" => {
                    index += 1;
                    target = Some(
                        args.get(index)
                            .context("missing value after --target")?
                            .to_string(),
                    );
                }
                "--binary" => {
                    index += 1;
                    binary = Some(PathBuf::from(
                        args.get(index).context("missing value after --binary")?,
                    ));
                }
                "--out-dir" => {
                    index += 1;
                    out_dir =
                        PathBuf::from(args.get(index).context("missing value after --out-dir")?);
                }
                "--version" => {
                    index += 1;
                    version = Some(
                        args.get(index)
                            .context("missing value after --version")?
                            .to_string(),
                    );
                }
                arg => bail!("unknown package argument `{arg}`"),
            }

            index += 1;
        }

        let target = target.unwrap_or(host_target()?);
        ensure_supported_target(&target)?;

        Ok(Self {
            target,
            binary,
            out_dir,
            version,
        })
    }
}

#[derive(Debug)]
struct ChecksumsArgs {
    dir: PathBuf,
}

impl ChecksumsArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut dir = PathBuf::from(DEFAULT_OUT_DIR);
        let mut index = 0;

        while index < args.len() {
            match args[index].as_str() {
                "--dir" => {
                    index += 1;
                    dir = PathBuf::from(args.get(index).context("missing value after --dir")?);
                }
                arg => bail!("unknown checksums argument `{arg}`"),
            }

            index += 1;
        }

        Ok(Self { dir })
    }
}

#[derive(Debug)]
struct LiveSmokeArgs {
    cdp_endpoint: Option<String>,
    binary: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    allow_focus: bool,
}

impl LiveSmokeArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut cdp_endpoint = None;
        let mut binary = None;
        let mut state_dir = None;
        let mut allow_focus = false;
        let mut index = 0;

        while index < args.len() {
            match args[index].as_str() {
                "--cdp-endpoint" => {
                    index += 1;
                    cdp_endpoint = Some(
                        args.get(index)
                            .context("missing value after --cdp-endpoint")?
                            .to_string(),
                    );
                }
                "--binary" => {
                    index += 1;
                    binary = Some(PathBuf::from(
                        args.get(index).context("missing value after --binary")?,
                    ));
                }
                "--state-dir" => {
                    index += 1;
                    state_dir = Some(PathBuf::from(
                        args.get(index).context("missing value after --state-dir")?,
                    ));
                }
                "--allow-focus" => {
                    allow_focus = true;
                }
                arg => bail!("unknown live-smoke argument `{arg}`"),
            }

            index += 1;
        }

        Ok(Self {
            cdp_endpoint,
            binary,
            state_dir,
            allow_focus,
        })
    }
}

#[derive(Debug)]
struct InstallSmokeArgs {
    archive: Option<PathBuf>,
    codex: PathBuf,
    chrome_path: Option<PathBuf>,
    invoke_codex: bool,
    auth_source: Option<PathBuf>,
    keep_temp: bool,
}

impl InstallSmokeArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut archive = None;
        let mut codex = PathBuf::from("codex");
        let mut chrome_path = None;
        let mut invoke_codex = false;
        let mut auth_source = None;
        let mut keep_temp = false;
        let mut index = 0;

        while index < args.len() {
            match args[index].as_str() {
                "--archive" => {
                    index += 1;
                    archive = Some(PathBuf::from(
                        args.get(index).context("missing value after --archive")?,
                    ));
                }
                "--codex" => {
                    index += 1;
                    codex = PathBuf::from(args.get(index).context("missing value after --codex")?);
                }
                "--chrome-path" => {
                    index += 1;
                    chrome_path = Some(PathBuf::from(
                        args.get(index)
                            .context("missing value after --chrome-path")?,
                    ));
                }
                "--invoke-codex" => invoke_codex = true,
                "--auth-source" => {
                    index += 1;
                    auth_source = Some(PathBuf::from(
                        args.get(index)
                            .context("missing value after --auth-source")?,
                    ));
                }
                "--keep-temp" => keep_temp = true,
                arg => bail!("unknown install-smoke argument `{arg}`"),
            }
            index += 1;
        }

        if auth_source.is_some() && !invoke_codex {
            bail!("--auth-source requires --invoke-codex");
        }

        Ok(Self {
            archive,
            codex,
            chrome_path,
            invoke_codex,
            auth_source,
            keep_temp,
        })
    }
}

fn validate() -> Result<()> {
    let root = repo_root()?;

    for path in [
        ".codex-plugin/plugin.json",
        "skills/visible-browser-lab/SKILL.md",
        "Cargo.toml",
        "Cargo.lock",
    ] {
        let path = root.join(path);
        if !path.is_file() {
            bail!("required source file is missing: {}", path.display());
        }
    }
    validate_source_package_contract(&root)?;
    agent_eval::validate_catalog()?;

    for forbidden in [
        "package.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "package-lock.json",
    ] {
        let path = root.join(forbidden);
        if path.exists() {
            bail!("Node packaging file is not allowed for trusted binary releases: {forbidden}");
        }
    }

    let gitignore =
        fs::read_to_string(root.join(".gitignore")).context("failed to read .gitignore")?;
    for required in [".DS_Store", ".exo/runtime/", "target/", "out/"] {
        if !gitignore.lines().any(|line| line.trim() == required) {
            bail!(".gitignore must ignore `{required}`");
        }
    }

    let package_dir = root.join(DEFAULT_OUT_DIR);
    if package_dir.is_dir() {
        validate_archives(&package_dir)?;
    }

    println!("validated visible-browser-lab release inputs");
    Ok(())
}

fn package(args: PackageArgs) -> Result<()> {
    let root = repo_root()?;
    let version = release_version(&root, args.version.as_deref())?;
    let binary = binary_path(&root, &args.target, args.binary.as_deref())?;
    let out_dir = root.join(args.out_dir);
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create output directory `{}`", out_dir.display()))?;

    let mut archives = Vec::new();
    for host in AGENT_HOSTS {
        let archive = out_dir.join(format!(
            "visible-browser-lab-{}-{}-{}.zip",
            host.id, version, args.target
        ));
        write_plugin_archive(&root, host, &args.target, &version, &binary, &archive)?;
        archives.push(archive);
    }

    let binary_archive = out_dir.join(format!(
        "visible-browser-lab-mcp-{}-{}.zip",
        version, args.target
    ));
    write_binary_archive(&args.target, &version, &binary, &binary_archive)?;
    archives.push(binary_archive);

    for archive in &archives {
        println!("wrote {}", archive.display());
    }

    validate_archives(&out_dir)?;
    Ok(())
}

fn checksums(args: ChecksumsArgs) -> Result<()> {
    let root = repo_root()?;
    let dir = root.join(args.dir);
    let mut files = archive_files(&dir)?;

    if files.is_empty() {
        bail!("no release assets found in `{}`", dir.display());
    }

    files.sort();
    let mut output = String::new();

    for path in &files {
        if path.file_name().is_some_and(|name| name == "SHA256SUMS") {
            continue;
        }

        let digest = sha256_file(path)?;
        let relative = path
            .strip_prefix(&dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        output.push_str(&format!("{digest}  {relative}\n"));
    }

    let sums_path = dir.join("SHA256SUMS");
    fs::write(&sums_path, output)
        .with_context(|| format!("failed to write `{}`", sums_path.display()))?;
    println!("wrote {}", sums_path.display());
    Ok(())
}

fn live_smoke(args: LiveSmokeArgs) -> Result<()> {
    let root = repo_root()?;
    if args.binary.is_none() {
        build_live_smoke_binary(&root)?;
    }
    let binary = live_smoke_binary(&root, args.binary.as_deref())?;
    let (state_dir, remove_state_dir) = match args.state_dir {
        Some(state_dir) => (state_dir, false),
        None => {
            // Unix socket paths are length-limited; keep the smoke state path short.
            let state_root = if cfg!(windows) {
                env::temp_dir()
            } else {
                PathBuf::from("/tmp")
            };
            let state_dir = state_root.join(format!(
                "visible-browser-lab-live-smoke-{}-{}",
                std::process::id(),
                unix_millis()?
            ));
            fs::create_dir_all(&state_dir)
                .with_context(|| format!("failed to create `{}`", state_dir.display()))?;
            (state_dir, true)
        }
    };

    let mut client = match args.cdp_endpoint.as_deref() {
        Some(cdp_endpoint) => visible_browser_lab_test_support::McpClient::spawn(
            &binary,
            cdp_endpoint,
            &state_dir,
            &root,
        )?,
        None => visible_browser_lab_test_support::McpClient::spawn_with_state(
            &binary, &state_dir, &root,
        )?,
    };
    let mut open_tabs = Vec::new();
    let smoke_result = visible_browser_lab_test_support::run_live_smoke(
        &mut client,
        &mut open_tabs,
        args.cdp_endpoint.as_deref(),
        Some(&state_dir),
        args.allow_focus,
    );
    visible_browser_lab_test_support::cleanup_open_tabs(&mut client, &mut open_tabs);
    client.shutdown();
    visible_browser_lab_test_support::stop_broker(&state_dir);
    if args.cdp_endpoint.is_none()
        && remove_state_dir
        && let Ok(endpoint) = visible_browser_lab_test_support::managed_endpoint(&state_dir)
    {
        let _ = visible_browser_lab_test_support::close_browser_via_cdp(&endpoint);
    }

    if remove_state_dir {
        let _ = fs::remove_dir_all(&state_dir);
    }

    let summary = smoke_result?;
    println!(
        "live smoke passed: tools={}, screenshot_bytes={}, global_groups={}",
        summary.tool_count, summary.screenshot_bytes, summary.global_groups
    );
    Ok(())
}

fn install_smoke(args: InstallSmokeArgs) -> Result<()> {
    let root = repo_root()?;
    let codex = resolve_command(&args.codex)?;
    let temp_root = if cfg!(windows) {
        env::temp_dir()
    } else {
        PathBuf::from("/tmp")
    };
    let temp = tempfile::Builder::new()
        .prefix("vbl-is-")
        .tempdir_in(temp_root)
        .context("failed to create disposable install-smoke root")?;
    let result = (|| -> Result<()> {
        let smoke_root = temp.path();
        let home = smoke_root.join("home");
        let codex_home = smoke_root.join("codex-home");
        let codex_sqlite_home = smoke_root.join("codex-sqlite-home");
        let marketplace = smoke_root.join("marketplace");
        let marketplace_plugin = marketplace.join("plugin");
        let workspace = smoke_root.join("workspace");
        let state_dir = smoke_root.join("state");
        for directory in [
            &home,
            &codex_home,
            &codex_sqlite_home,
            &marketplace_plugin,
            &workspace,
            &state_dir,
        ] {
            fs::create_dir_all(directory)
                .with_context(|| format!("failed to create `{}`", directory.display()))?;
        }

        if let Some(auth_source) = args.auth_source.as_deref() {
            copy_codex_auth(auth_source, &codex_home)?;
        }

        let archive = match args.archive.as_deref() {
            Some(path) => path
                .canonicalize()
                .with_context(|| format!("failed to resolve Codex package `{}`", path.display()))?,
            None => build_disposable_codex_package(&root, smoke_root)?,
        };
        validate_plugin_archive(&archive)?;
        let package_manifest = read_zip_json(&archive, "package-manifest.json")?;
        if package_manifest["host"].as_str() != Some("codex") {
            bail!("install-smoke requires a Codex host package");
        }
        let version = package_manifest["version"]
            .as_str()
            .context("package manifest omitted version")?
            .to_string();
        let target = package_manifest["target"]
            .as_str()
            .context("package manifest omitted target")?
            .to_string();
        extract_zip(&archive, &marketplace_plugin)?;
        write_isolated_marketplace(&marketplace)?;

        let environment = IsolatedCodexEnvironment {
            home,
            codex_home,
            codex_sqlite_home,
            workspace,
        };
        let host_default_state_dir = environment.default_visible_browser_state_dir();
        let marketplace_output = run_checked(
            isolated_codex_command(&codex, ["plugin", "marketplace", "add"], &environment)
                .arg(&marketplace),
            "add disposable Codex marketplace",
        )?;
        let marketplace_stdout = String::from_utf8(marketplace_output.stdout)
            .context("Codex marketplace output was not UTF-8")?;
        if !marketplace_stdout.contains("visible-browser-lab-isolated") {
            bail!("Codex did not report the disposable marketplace: {marketplace_stdout}");
        }

        let add_output = run_checked(
            &mut isolated_codex_command(
                &codex,
                [
                    "plugin",
                    "add",
                    "visible-browser-lab@visible-browser-lab-isolated",
                ],
                &environment,
            ),
            "install visible-browser-lab into disposable Codex home",
        )?;
        let add_stdout =
            String::from_utf8(add_output.stdout).context("Codex plugin output was not UTF-8")?;
        let installed_root = installed_plugin_root(&add_stdout)?;
        let installed_root = installed_root.canonicalize().with_context(|| {
            format!(
                "failed to resolve installed plugin root `{}`",
                installed_root.display()
            )
        })?;
        let canonical_codex_home = environment.codex_home.canonicalize()?;
        if !installed_root.starts_with(canonical_codex_home.join("plugins/cache")) {
            bail!(
                "Codex installed the plugin outside the disposable cache: {}",
                installed_root.display()
            );
        }

        let list_output = run_checked(
            &mut isolated_codex_command(
                &codex,
                [
                    "plugin",
                    "list",
                    "--marketplace",
                    "visible-browser-lab-isolated",
                ],
                &environment,
            ),
            "list the disposable Codex plugin installation",
        )?;
        let list_stdout =
            String::from_utf8(list_output.stdout).context("Codex plugin list was not UTF-8")?;
        if !list_stdout.contains("visible-browser-lab")
            || !list_stdout.contains("installed, enabled")
            || !list_stdout.contains(&version)
        {
            bail!("Codex plugin list did not report the installed package: {list_stdout}");
        }

        let (installed_binary, installed_cwd) =
            validate_installed_codex_package(&installed_root, &version, &target)?;
        let version_output = run_checked(
            Command::new(&installed_binary).arg("--version"),
            "run the installed visible-browser-lab binary",
        )?;
        let reported_version = String::from_utf8(version_output.stdout)
            .context("installed binary version output was not UTF-8")?;
        let expected_version = format!("{BINARY_NAME} {version}");
        if reported_version.trim() != expected_version {
            bail!(
                "installed binary version mismatch: expected `{expected_version}`, got `{}`",
                reported_version.trim()
            );
        }

        let chrome_path = match args.chrome_path.as_deref() {
            Some(path) => path.canonicalize().with_context(|| {
                format!("failed to resolve Chrome executable `{}`", path.display())
            })?,
            None => visible_browser_lab_test_support::chrome_for_testing_executable()?
                .canonicalize()
                .context("failed to resolve Chrome for Testing executable")?,
        };
        let _cleanup = InstalledSmokeCleanup {
            state_dirs: vec![state_dir.clone(), host_default_state_dir.clone()],
        };
        let title = run_installed_facade_lifecycle(
            &installed_binary,
            &installed_cwd,
            &state_dir,
            &chrome_path,
        )?;

        if args.invoke_codex {
            run_model_invocation(&codex, &environment, &state_dir, &chrome_path)?;
            if host_default_state_dir.exists() {
                bail!(
                    "Codex did not pass the isolated runtime environment to the installed MCP server: {}",
                    host_default_state_dir.display()
                );
            }
        }

        let active_port = state_dir.join("chrome-profile/DevToolsActivePort");
        if !active_port.is_file() {
            bail!(
                "managed Chrome did not use the disposable profile at `{}`",
                active_port.display()
            );
        }
        if !state_dir.join("broker-v3.pid").is_file() {
            bail!(
                "broker did not use the disposable state directory `{}`",
                state_dir.display()
            );
        }

        println!(
            "install smoke passed: version={version}, target={target}, title={title}, cache={}",
            installed_root.display()
        );
        Ok(())
    })();

    if args.auth_source.is_some() {
        let _ = fs::remove_file(temp.path().join("codex-home/auth.json"));
    }
    if args.keep_temp {
        let retained = temp.keep();
        println!(
            "retained disposable install-smoke root at {}",
            retained.display()
        );
    }
    result
}

fn build_disposable_codex_package(root: &Path, smoke_root: &Path) -> Result<PathBuf> {
    let status = Command::new("cargo")
        .args(["build", "--release", "--bin", BINARY_NAME])
        .current_dir(root)
        .status()
        .context("failed to build release binary for install smoke")?;
    if !status.success() {
        bail!("cargo build --release --bin {BINARY_NAME} failed");
    }

    let target = host_target()?;
    ensure_supported_target(&target)?;
    let version = release_version(root, None)?;
    let binary = binary_path(root, &target, None)?;
    let archive = smoke_root.join(format!("visible-browser-lab-codex-{version}-{target}.zip"));
    write_plugin_archive(root, &AGENT_HOSTS[0], &target, &version, &binary, &archive)?;
    Ok(archive)
}

fn resolve_command(command: &Path) -> Result<PathBuf> {
    if command.components().count() == 1 && !command.is_file() {
        return Ok(command.to_path_buf());
    }
    command
        .canonicalize()
        .with_context(|| format!("failed to resolve executable `{}`", command.display()))
}

fn read_zip_json(archive_path: &Path, entry_name: &str) -> Result<Value> {
    let mut archive = open_zip(archive_path)?;
    let mut entry = archive.by_name(entry_name).with_context(|| {
        format!(
            "archive `{}` is missing `{entry_name}`",
            archive_path.display()
        )
    })?;
    let mut contents = String::new();
    entry.read_to_string(&mut contents)?;
    serde_json::from_str(&contents).with_context(|| {
        format!(
            "archive `{}` has invalid JSON in `{entry_name}`",
            archive_path.display()
        )
    })
}

fn extract_zip(archive_path: &Path, destination: &Path) -> Result<()> {
    let mut archive = open_zip(archive_path)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let relative = entry.enclosed_name().with_context(|| {
            format!(
                "archive `{}` contains unsafe path `{}`",
                archive_path.display(),
                entry.name()
            )
        })?;
        let output = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = File::create(&output)?;
        std::io::copy(&mut entry, &mut file)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&output, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

fn write_isolated_marketplace(root: &Path) -> Result<()> {
    let manifest_path = root.join(".agents/plugins/marketplace.json");
    fs::create_dir_all(
        manifest_path
            .parent()
            .context("marketplace manifest omitted parent")?,
    )?;
    let manifest = json!({
        "name": "visible-browser-lab-isolated",
        "interface": { "displayName": "Visible Browser Lab Isolated" },
        "plugins": [{
            "name": "visible-browser-lab",
            "source": { "source": "local", "path": "./plugin" },
            "policy": {
                "installation": "AVAILABLE",
                "authentication": "ON_INSTALL"
            },
            "category": "Developer Tools"
        }]
    });
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

struct IsolatedCodexEnvironment {
    home: PathBuf,
    codex_home: PathBuf,
    codex_sqlite_home: PathBuf,
    workspace: PathBuf,
}

impl IsolatedCodexEnvironment {
    fn default_visible_browser_state_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Caches").join("visible-browser-lab")
        } else if cfg!(windows) {
            self.home.join("AppData/Local").join("visible-browser-lab")
        } else {
            self.home.join(".cache").join("visible-browser-lab")
        }
    }
}

fn isolated_codex_command<I, S>(
    codex: &Path,
    args: I,
    environment: &IsolatedCodexEnvironment,
) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(codex);
    command
        .args(args)
        .current_dir(&environment.workspace)
        .env("HOME", &environment.home)
        .env("USERPROFILE", &environment.home)
        .env("XDG_CONFIG_HOME", environment.home.join(".config"))
        .env("XDG_CACHE_HOME", environment.home.join(".cache"))
        .env("LOCALAPPDATA", environment.home.join("AppData/Local"))
        .env("APPDATA", environment.home.join("AppData/Roaming"))
        .env("CODEX_HOME", &environment.codex_home)
        .env("CODEX_SQLITE_HOME", &environment.codex_sqlite_home)
        .env_remove("CODEX_MANAGED_CONFIG_PATH");
    command
}

fn run_checked(command: &mut Command, operation: &str) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("failed to {operation}"))?;
    if !output.status.success() {
        bail!(
            "failed to {operation}: status={}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

fn installed_plugin_root(stdout: &str) -> Result<PathBuf> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("Installed plugin root: "))
        .map(PathBuf::from)
        .context("Codex plugin add output omitted the installed plugin root")
}

fn validate_installed_codex_package(
    installed_root: &Path,
    version: &str,
    target: &str,
) -> Result<(PathBuf, PathBuf)> {
    let manifest: Value =
        serde_json::from_slice(&fs::read(installed_root.join(".codex-plugin/plugin.json"))?)?;
    if manifest["version"].as_str() != Some(version) {
        bail!("installed Codex manifest version does not match `{version}`");
    }
    let package_manifest: Value =
        serde_json::from_slice(&fs::read(installed_root.join("package-manifest.json"))?)?;
    if package_manifest["version"].as_str() != Some(version)
        || package_manifest["target"].as_str() != Some(target)
        || package_manifest["host"].as_str() != Some("codex")
    {
        bail!("installed package manifest does not match the selected Codex archive");
    }

    let mcp: Value = serde_json::from_slice(&fs::read(installed_root.join(".mcp.json"))?)?;
    let server = &mcp["mcpServers"]["visible-browser-lab"];
    let command = server["command"]
        .as_str()
        .context("installed MCP config omitted command")?;
    let cwd = server["cwd"]
        .as_str()
        .context("installed MCP config omitted cwd")?;
    if Path::new(command).is_absolute() || Path::new(cwd).is_absolute() {
        bail!("installed Codex MCP config must resolve from the plugin root");
    }
    if server["env_vars"] != json!(RUNTIME_ENV_VARS) {
        bail!("installed Codex MCP config omitted runtime environment overrides");
    }
    let installed_root = installed_root.canonicalize()?;
    let resolved_cwd = installed_root.join(cwd).canonicalize()?;
    if !resolved_cwd.starts_with(&installed_root) {
        bail!("installed MCP cwd escaped the plugin root");
    }
    let binary = resolved_cwd
        .join(command.strip_prefix("./").unwrap_or(command))
        .canonicalize()?;
    if !binary.starts_with(&installed_root) || !binary.is_file() {
        bail!("installed MCP command did not resolve to the packaged binary");
    }
    Ok((binary, resolved_cwd))
}

fn run_installed_facade_lifecycle(
    binary: &Path,
    installed_cwd: &Path,
    state_dir: &Path,
    chrome_path: &Path,
) -> Result<String> {
    use visible_browser_lab_test_support::{McpClient, OpenTab, field_str};

    let expected_title = "Visible Browser Lab Installed Smoke";
    let mut client =
        McpClient::spawn_managed_from_environment(binary, state_dir, installed_cwd, chrome_path)?;
    client.initialize("visible-browser-lab-install-smoke")?;
    let session = client.call_tool(
        "start_session",
        json!({
            "label": "installed-package-smoke",
            "start_url": visible_browser_lab_test_support::data_url(expected_title, expected_title),
            "focus": false
        }),
        Duration::from_secs(45),
        false,
    )?;
    let session_id = field_str(&session, "agent_session_id")?;
    let tab = OpenTab::from_summary(
        &session_id,
        session.get("tab").context("start_session omitted tab")?,
    )?;
    let owned = client.call_tool(
        "list_tabs",
        json!({ "agent_session_id": session_id }),
        Duration::from_secs(20),
        false,
    )?;
    let tabs = owned
        .get("tabs")
        .and_then(Value::as_array)
        .context("list_tabs omitted tabs")?;
    if !tabs.iter().any(|candidate| {
        candidate.get("tab_id").and_then(Value::as_str) == Some(tab.tab_id.as_str())
    }) {
        bail!("default list_tabs omitted the installed smoke tab");
    }
    let evaluation = client.call_tool(
        "evaluate",
        json!({
            "agent_session_id": session_id,
            "tab_id": tab.tab_id,
            "source": "document.title"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let title = evaluation["value"]
        .as_str()
        .context("evaluate omitted document.title")?
        .to_string();
    if title != expected_title {
        bail!("installed facade returned unexpected title `{title}`");
    }
    client.call_tool(
        "close_tab",
        json!({ "agent_session_id": session_id, "tab_id": tab.tab_id }),
        Duration::from_secs(20),
        false,
    )?;
    client.shutdown();
    Ok(title)
}

fn copy_codex_auth(source: &Path, codex_home: &Path) -> Result<()> {
    let source = if source.is_dir() {
        source.join("auth.json")
    } else {
        source.to_path_buf()
    };
    if !source.is_file() {
        bail!("Codex auth source does not contain `{}`", source.display());
    }
    fs::copy(&source, codex_home.join("auth.json"))
        .with_context(|| format!("failed to copy Codex auth from `{}`", source.display()))?;
    Ok(())
}

fn run_model_invocation(
    codex: &Path,
    environment: &IsolatedCodexEnvironment,
    state_dir: &Path,
    chrome_path: &Path,
) -> Result<()> {
    let prompt = "Use only the visible-browser-lab MCP tools. Call start_session with focus false and a data: page whose title is Visible Browser Lab Codex Smoke. Call default list_tabs with the returned agent_session_id, evaluate document.title with the returned tab_id, then close_tab. Return the observed title and confirm the tab was closed. Do not use shell commands or browser fallbacks.";
    let mut command = isolated_codex_command(
        codex,
        [
            "exec",
            "--ephemeral",
            "--json",
            "--skip-git-repo-check",
            "--dangerously-bypass-approvals-and-sandbox",
            "-C",
        ],
        environment,
    );
    command
        .arg(&environment.workspace)
        .arg(prompt)
        .env("VISIBLE_BROWSER_LAB_STATE_DIR", state_dir)
        .env("VISIBLE_BROWSER_LAB_CHROME_PATH", chrome_path);
    let output = run_checked(&mut command, "run isolated Codex MCP invocation")?;
    let events = String::from_utf8(output.stdout).context("Codex JSONL output was not UTF-8")?;
    let events_path = environment.workspace.join("codex-exec.jsonl");
    fs::write(&events_path, &events)?;
    validate_codex_invocation_events(&events)
        .with_context(|| format!("Codex events: {}", events_path.display()))?;
    if !events.contains("Visible Browser Lab Codex Smoke") {
        bail!("Codex invocation did not report the expected page title");
    }
    Ok(())
}

fn validate_codex_invocation_events(events: &str) -> Result<()> {
    let mut completed_tools = Vec::new();
    for line in events.lines().filter(|line| !line.trim().is_empty()) {
        let event: Value = serde_json::from_str(line).context("Codex emitted invalid JSONL")?;
        let Some(item) = event.get("item") else {
            continue;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        if item_type == "command_execution" {
            bail!("Codex used command execution during the MCP-only invocation");
        }
        if item_type != "mcp_tool_call" || event["type"].as_str() != Some("item.completed") {
            continue;
        }
        if item["server"].as_str() != Some("visible-browser-lab") {
            bail!("Codex called a non-facade MCP server: {item}");
        }
        if !item["error"].is_null() || item["status"].as_str() != Some("completed") {
            bail!("Codex facade tool call did not complete: {item}");
        }
        completed_tools.push(
            item["tool"]
                .as_str()
                .context("Codex MCP event omitted tool name")?
                .to_string(),
        );
    }

    let expected = ["start_session", "list_tabs", "evaluate", "close_tab"];
    if completed_tools != expected {
        bail!("Codex completed facade tool sequence {completed_tools:?}; expected {expected:?}");
    }
    Ok(())
}

fn managed_endpoint(state_dir: &Path) -> Result<String> {
    let active_port = fs::read_to_string(state_dir.join("chrome-profile/DevToolsActivePort"))?;
    let port = active_port
        .lines()
        .next()
        .context("DevToolsActivePort omitted port")?
        .trim()
        .parse::<u16>()?;
    Ok(format!("http://127.0.0.1:{port}"))
}

struct InstalledSmokeCleanup {
    state_dirs: Vec<PathBuf>,
}

impl Drop for InstalledSmokeCleanup {
    fn drop(&mut self) {
        for state_dir in &self.state_dirs {
            visible_browser_lab_test_support::stop_broker(state_dir);
            if let Ok(endpoint) = managed_endpoint(state_dir) {
                let _ = visible_browser_lab_test_support::close_browser_via_cdp(&endpoint);
            }
        }
    }
}

fn unix_millis() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_millis())
}

fn live_smoke_binary(root: &Path, override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        bail!(
            "live-smoke binary override does not exist: {}",
            path.display()
        );
    }

    let binary = root
        .join("target")
        .join("debug")
        .join(binary_file_name(&host_target()?));
    if binary.is_file() {
        return Ok(binary);
    }

    bail!(
        "debug binary not found at `{}`. Run `cargo build --bin {BINARY_NAME}` first.",
        binary.display()
    )
}

fn build_live_smoke_binary(root: &Path) -> Result<()> {
    let status = Command::new("cargo")
        .args(["build", "--bin", BINARY_NAME])
        .current_dir(root)
        .status()
        .context("failed to run cargo build for live smoke binary")?;
    if !status.success() {
        bail!("cargo build --bin {BINARY_NAME} failed");
    }
    Ok(())
}

fn write_plugin_archive(
    root: &Path,
    host: &AgentHost,
    target: &str,
    version: &str,
    binary: &Path,
    archive: &Path,
) -> Result<()> {
    let file = File::create(archive)
        .with_context(|| format!("failed to create archive `{}`", archive.display()))?;
    let mut zip = ZipWriter::new(file);
    let binary_name = binary_file_name(target);
    let mcp_config = mcp_config_bytes(host, &binary_name)?;
    let manifest = host_manifest_bytes(root, host, target, version, &binary_name)?;
    let package_manifest = package_manifest_bytes(host, target, version, &binary_name)?;

    add_bytes(&mut zip, host.manifest_path, &manifest, 0o644)?;
    add_bytes(&mut zip, ".mcp.json", &mcp_config, 0o644)?;
    add_file(
        &mut zip,
        "skills/visible-browser-lab/SKILL.md",
        &root.join("skills/visible-browser-lab/SKILL.md"),
        0o644,
    )?;
    add_file(
        &mut zip,
        &format!("bin/{binary_name}"),
        binary,
        executable_mode(target),
    )?;
    add_bytes(&mut zip, "package-manifest.json", &package_manifest, 0o644)?;

    zip.finish()
        .with_context(|| format!("failed to finish archive `{}`", archive.display()))?;
    validate_plugin_archive(archive)?;
    Ok(())
}

fn write_binary_archive(target: &str, version: &str, binary: &Path, archive: &Path) -> Result<()> {
    let file = File::create(archive)
        .with_context(|| format!("failed to create archive `{}`", archive.display()))?;
    let mut zip = ZipWriter::new(file);
    let binary_name = binary_file_name(target);
    let readme = format!(
        "\
visible-browser-lab MCP broker

target: {target}
version: {version}
binary: {binary_name}

This archive is for debugging or manual installation. Plugin hosts should use
the host-specific visible-browser-lab package archives from the same release.
"
    );

    add_file(&mut zip, &binary_name, binary, executable_mode(target))?;
    add_bytes(&mut zip, "README.txt", readme.as_bytes(), 0o644)?;
    zip.finish()
        .with_context(|| format!("failed to finish archive `{}`", archive.display()))?;
    validate_binary_archive(archive)?;
    Ok(())
}

fn add_file<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    source: &Path,
    mode: u32,
) -> Result<()> {
    let bytes =
        fs::read(source).with_context(|| format!("failed to read `{}`", source.display()))?;
    add_bytes(zip, name, &bytes, mode)
}

fn add_bytes<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<()> {
    if name.starts_with('/')
        || name.contains("..")
        || name.contains('\\')
        || forbidden_archive_path(name)
    {
        bail!("refusing to add unsafe archive path `{name}`");
    }

    let options: FileOptions<'_, ExtendedFileOptions> = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(mode);
    zip.start_file(name, options)
        .with_context(|| format!("failed to start archive entry `{name}`"))?;
    zip.write_all(bytes)
        .with_context(|| format!("failed to write archive entry `{name}`"))?;
    Ok(())
}

fn host_manifest_bytes(
    root: &Path,
    host: &AgentHost,
    target: &str,
    version: &str,
    binary_name: &str,
) -> Result<Vec<u8>> {
    if host.plugin_format == PluginFormat::Codex {
        let source = fs::read_to_string(root.join(".codex-plugin/plugin.json"))
            .context("failed to read Codex plugin manifest")?;
        let mut manifest: Value =
            serde_json::from_str(&source).context("invalid Codex manifest JSON")?;
        manifest["version"] = Value::String(version.to_string());
        manifest["mcpServers"] = Value::String("./.mcp.json".to_string());
        manifest["packaging"] = json!({
            "kind": "trusted-binary-release",
            "target": target,
            "binary": format!("bin/{binary_name}"),
        });
        return Ok(serde_json::to_vec_pretty(&manifest)?);
    }

    let manifest = json!({
        "name": "visible-browser-lab",
        "version": version,
        "displayName": format!("Visible Browser Lab ({})", host.display_name),
        "description": "Visible Chrome automation through a shared CDP endpoint",
        "skills": "./skills/",
        "mcpServers": "./.mcp.json",
        "packaging": {
            "kind": "trusted-binary-release",
            "target": target,
            "binary": format!("bin/{binary_name}"),
        },
    });
    Ok(serde_json::to_vec_pretty(&manifest)?)
}

fn mcp_config_bytes(host: &AgentHost, binary_name: &str) -> Result<Vec<u8>> {
    let (command, cwd) = match host.plugin_format {
        PluginFormat::Codex => (format!("./bin/{binary_name}"), ".".to_string()),
        PluginFormat::Claude => (
            format!("${{CLAUDE_PLUGIN_ROOT}}/bin/{binary_name}"),
            "${CLAUDE_PLUGIN_ROOT}".to_string(),
        ),
    };
    let mut server = json!({
        "command": command,
        "args": [],
        "cwd": cwd,
    });
    if host.plugin_format == PluginFormat::Codex {
        server["env_vars"] = json!(RUNTIME_ENV_VARS);
    }
    let config = json!({
        "mcpServers": {
            "visible-browser-lab": server
        }
    });
    Ok(serde_json::to_vec_pretty(&config)?)
}

fn package_manifest_bytes(
    host: &AgentHost,
    target: &str,
    version: &str,
    binary_name: &str,
) -> Result<Vec<u8>> {
    let manifest = json!({
        "name": "visible-browser-lab",
        "version": version,
        "host": host.id,
        "target": target,
        "binary": format!("bin/{binary_name}"),
        "mcp_server": "visible-browser-lab",
        "source_commit": git_head().ok(),
    });
    Ok(serde_json::to_vec_pretty(&manifest)?)
}

fn release_version(root: &Path, explicit: Option<&str>) -> Result<String> {
    let configured = explicit
        .map(ToOwned::to_owned)
        .or_else(|| env::var(RELEASE_VERSION_ENV).ok())
        .map(|version| version.trim_start_matches('v').to_string());
    let candidate = match configured {
        Some(version) => version,
        None => cargo_package_version(root)?,
    };
    Version::parse(&candidate)
        .with_context(|| format!("release version `{candidate}` is not valid semantic version"))?;
    Ok(candidate)
}

fn validate_source_package_contract(root: &Path) -> Result<()> {
    let package_version = cargo_package_version(root)?;
    let manifest: Value = serde_json::from_slice(
        &fs::read(root.join(".codex-plugin/plugin.json"))
            .context("failed to read Codex plugin manifest")?,
    )
    .context("invalid Codex plugin manifest JSON")?;
    if manifest["version"].as_str() != Some(&package_version) {
        bail!("Codex source manifest version must match Cargo package version `{package_version}`");
    }

    let mcp: Value = serde_json::from_slice(
        &fs::read(root.join(".mcp.json")).context("failed to read source MCP config")?,
    )
    .context("invalid source MCP config JSON")?;
    let server = &mcp["mcpServers"]["visible-browser-lab"];
    if server["command"].as_str() != Some("./scripts/visible-browser-lab-mcp.sh")
        || server["cwd"].as_str() != Some(".")
        || server["env_vars"] != json!(RUNTIME_ENV_VARS)
    {
        bail!("source MCP config must preserve plugin-root and runtime environment contracts");
    }
    Ok(())
}

fn cargo_package_version(root: &Path) -> Result<String> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .context("failed to run cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let metadata: Value =
        serde_json::from_slice(&output.stdout).context("cargo metadata returned invalid JSON")?;
    metadata["packages"]
        .as_array()
        .context("cargo metadata omitted packages")?
        .iter()
        .find(|package| package["name"] == "visible-browser-lab")
        .and_then(|package| package["version"].as_str())
        .map(ToOwned::to_owned)
        .context("cargo metadata omitted the visible-browser-lab package version")
}

fn validate_archives(dir: &Path) -> Result<()> {
    for path in archive_files(dir)? {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name == "SHA256SUMS" {
            continue;
        }

        if name.starts_with("visible-browser-lab-mcp-") {
            validate_binary_archive(&path)?;
        } else if name.starts_with("visible-browser-lab-") {
            validate_plugin_archive(&path)?;
        }
    }

    Ok(())
}

fn validate_plugin_archive(path: &Path) -> Result<()> {
    let mut archive = open_zip(path)?;
    let mut names = Vec::new();
    let mut mcp_config: Option<Value> = None;
    let mut package_manifest: Option<Value> = None;
    let mut host_manifests = Vec::new();

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let name = file.name().to_string();
        if forbidden_archive_path(&name) {
            bail!(
                "archive `{}` contains forbidden path `{name}`",
                path.display()
            );
        }

        if name == ".mcp.json"
            || name == "package-manifest.json"
            || AGENT_HOSTS.iter().any(|host| host.manifest_path == name)
        {
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            let json: Value = serde_json::from_str(&contents).with_context(|| {
                format!("archive `{}` has invalid JSON in `{name}`", path.display())
            })?;
            match name.as_str() {
                ".mcp.json" => mcp_config = Some(json),
                "package-manifest.json" => package_manifest = Some(json),
                _ => host_manifests.push((name.clone(), json)),
            }
        }

        names.push(name);
    }

    let binary_count = names
        .iter()
        .filter(|name| name.starts_with("bin/visible-browser-lab-mcp"))
        .count();
    if binary_count != 1 {
        bail!(
            "archive `{}` must contain exactly one packaged binary, found {binary_count}",
            path.display()
        );
    }

    for required in [
        ".mcp.json",
        "skills/visible-browser-lab/SKILL.md",
        "package-manifest.json",
    ] {
        if !names.iter().any(|name| name == required) {
            bail!("archive `{}` is missing `{required}`", path.display());
        }
    }

    if host_manifests.len() != 1 {
        bail!(
            "archive `{}` must contain exactly one host plugin manifest, found {}",
            path.display(),
            host_manifests.len()
        );
    }

    let package_manifest = package_manifest.with_context(|| {
        format!(
            "archive `{}` is missing package-manifest.json",
            path.display()
        )
    })?;
    let host_id = package_manifest["host"]
        .as_str()
        .context("package manifest omitted host")?;
    let host = AGENT_HOSTS
        .iter()
        .find(|host| host.id == host_id)
        .with_context(|| format!("package manifest has unknown host `{host_id}`"))?;
    let version = package_manifest["version"]
        .as_str()
        .context("package manifest omitted version")?;
    Version::parse(version).context("package manifest version is not semantic version")?;
    let target = package_manifest["target"]
        .as_str()
        .context("package manifest omitted target")?;
    let binary_name = binary_file_name(target);
    let expected_binary = format!("bin/{binary_name}");
    if package_manifest["binary"].as_str() != Some(&expected_binary) {
        bail!("package manifest binary does not match target `{target}`");
    }
    let archive_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let expected_archive = format!("visible-browser-lab-{host_id}-{version}-{target}.zip");
    if archive_name != expected_archive {
        bail!("archive `{archive_name}` does not match package identity `{expected_archive}`");
    }

    let (manifest_path, host_manifest) = &host_manifests[0];
    if manifest_path != host.manifest_path {
        bail!(
            "archive `{}` uses `{manifest_path}` for host `{host_id}`; expected `{}`",
            path.display(),
            host.manifest_path
        );
    }
    if host_manifest["version"].as_str() != Some(version) {
        bail!("host manifest version does not match package version `{version}`");
    }

    let mcp_config =
        mcp_config.with_context(|| format!("archive `{}` is missing .mcp.json", path.display()))?;
    let server = &mcp_config["mcpServers"]["visible-browser-lab"];
    let (expected_command, expected_cwd) = match host.plugin_format {
        PluginFormat::Codex => (format!("./bin/{binary_name}"), ".".to_string()),
        PluginFormat::Claude => (
            format!("${{CLAUDE_PLUGIN_ROOT}}/bin/{binary_name}"),
            "${CLAUDE_PLUGIN_ROOT}".to_string(),
        ),
    };
    if server["command"].as_str() != Some(&expected_command)
        || server["cwd"].as_str() != Some(&expected_cwd)
        || server["args"]
            .as_array()
            .is_none_or(|args| !args.is_empty())
    {
        bail!(
            "archive `{}` does not resolve its MCP binary from the installed plugin root",
            path.display(),
        );
    }
    match host.plugin_format {
        PluginFormat::Codex if server["env_vars"] != json!(RUNTIME_ENV_VARS) => {
            bail!("Codex archive does not pass through runtime environment overrides");
        }
        PluginFormat::Claude if server.get("env_vars").is_some() => {
            bail!("Claude-format archive contains unsupported Codex env_vars");
        }
        _ => {}
    }

    Ok(())
}

fn validate_binary_archive(path: &Path) -> Result<()> {
    let mut archive = open_zip(path)?;
    let mut binary_count = 0;
    let mut readme = None;

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let name = file.name().to_string();
        if forbidden_archive_path(&name) {
            bail!(
                "archive `{}` contains forbidden path `{name}`",
                path.display()
            );
        }

        if name == BINARY_NAME || name == format!("{BINARY_NAME}.exe") {
            binary_count += 1;
        }
        if name == "README.txt" {
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            readme = Some(contents);
        }
    }

    if binary_count != 1 {
        bail!(
            "archive `{}` must contain exactly one binary, found {binary_count}",
            path.display()
        );
    }

    let readme =
        readme.with_context(|| format!("archive `{}` is missing README.txt", path.display()))?;
    let target = readme
        .lines()
        .find_map(|line| line.strip_prefix("target: "))
        .context("binary archive README omitted target")?;
    let version = readme
        .lines()
        .find_map(|line| line.strip_prefix("version: "))
        .context("binary archive README omitted version")?;
    Version::parse(version).context("binary archive version is not semantic version")?;
    let expected_archive = format!("visible-browser-lab-mcp-{version}-{target}.zip");
    let archive_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if archive_name != expected_archive {
        bail!("archive `{archive_name}` does not match binary identity `{expected_archive}`");
    }

    Ok(())
}

fn open_zip(path: &Path) -> Result<ZipArchive<File>> {
    let file =
        File::open(path).with_context(|| format!("failed to open archive `{}`", path.display()))?;
    ZipArchive::new(file).with_context(|| format!("invalid zip archive `{}`", path.display()))
}

fn archive_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_files(dir, &mut files)?;
    files.retain(|path| {
        path.extension().and_then(|ext| ext.to_str()) == Some("zip")
            || path.file_name().is_some_and(|name| name == "SHA256SUMS")
    });
    Ok(files)
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read `{}`", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }

    Ok(())
}

fn forbidden_archive_path(name: &str) -> bool {
    name.starts_with(".git/")
        || name.starts_with(".exo/runtime/")
        || name.starts_with("target/")
        || name.contains("/.git/")
        || name.contains("/.exo/runtime/")
        || name.contains("/target/")
        || name.contains(".DS_Store")
        || name.contains("/logs/")
        || name.starts_with("logs/")
        || name.contains("/cache/")
        || name.starts_with("cache/")
}

fn binary_path(root: &Path, target: &str, override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }

        bail!("binary override does not exist: {}", path.display());
    }

    let binary_name = binary_file_name(target);
    let mut candidates = vec![
        root.join("target")
            .join(target)
            .join("release")
            .join(&binary_name),
    ];
    if host_target()? == target {
        candidates.push(root.join("target").join("release").join(&binary_name));
    }

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }

    let candidates = candidates
        .iter()
        .map(|path| format!("  - {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    bail!("release binary for `{target}` not found. Checked:\n{candidates}");
}

fn binary_file_name(target: &str) -> String {
    if target.contains("windows") {
        format!("{BINARY_NAME}.exe")
    } else {
        BINARY_NAME.to_string()
    }
}

fn executable_mode(target: &str) -> u32 {
    if target.contains("windows") {
        0o644
    } else {
        0o755
    }
}

fn ensure_supported_target(target: &str) -> Result<()> {
    if SUPPORTED_TARGETS.contains(&target) {
        Ok(())
    } else {
        bail!(
            "unsupported target `{target}`. Supported targets: {}",
            SUPPORTED_TARGETS.join(", ")
        )
    }
}

fn host_target() -> Result<String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("failed to run rustc -vV")?;
    if !output.status.success() {
        bail!("rustc -vV failed");
    }

    let stdout = String::from_utf8(output.stdout).context("rustc -vV output was not UTF-8")?;
    for line in stdout.lines() {
        if let Some(host) = line.strip_prefix("host: ") {
            return Ok(host.to_string());
        }
    }

    bail!("rustc -vV output did not include host target")
}

fn repo_root() -> Result<PathBuf> {
    let mut dir = env::current_dir().context("failed to read current directory")?;

    loop {
        if dir.join(".codex-plugin/plugin.json").is_file() && dir.join("Cargo.toml").is_file() {
            return Ok(dir);
        }

        if !dir.pop() {
            bail!("could not find visible-browser-lab repository root");
        }
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open `{}`", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];

    loop {
        let bytes = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read `{}`", path.display()))?;
        if bytes == 0 {
            break;
        }
        hasher.update(&buffer[..bytes]);
    }

    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn git_head() -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .context("failed to run git rev-parse")?;
    if !output.status.success() {
        bail!("git rev-parse failed");
    }

    Ok(String::from_utf8(output.stdout)
        .context("git rev-parse output was not UTF-8")?
        .trim()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_version_normalizes_tag_prefix() {
        let root = repo_root().unwrap();

        assert_eq!(release_version(&root, Some("v1.2.3")).unwrap(), "1.2.3");
        assert!(release_version(&root, Some("release-1")).is_err());
    }

    #[test]
    fn host_mcp_configs_preserve_plugin_root_contracts() {
        let codex: Value = serde_json::from_slice(
            &mcp_config_bytes(&AGENT_HOSTS[0], "visible-browser-lab-mcp").unwrap(),
        )
        .unwrap();
        let codex_server = &codex["mcpServers"]["visible-browser-lab"];
        assert_eq!(codex_server["command"], "./bin/visible-browser-lab-mcp");
        assert_eq!(codex_server["cwd"], ".");
        assert_eq!(codex_server["env_vars"], json!(RUNTIME_ENV_VARS));

        for host in &AGENT_HOSTS[1..] {
            let config: Value =
                serde_json::from_slice(&mcp_config_bytes(host, "visible-browser-lab-mcp").unwrap())
                    .unwrap();
            let server = &config["mcpServers"]["visible-browser-lab"];
            assert_eq!(
                server["command"],
                "${CLAUDE_PLUGIN_ROOT}/bin/visible-browser-lab-mcp"
            );
            assert_eq!(server["cwd"], "${CLAUDE_PLUGIN_ROOT}");
        }
    }

    #[test]
    fn generated_archives_validate_host_root_and_version_identity() {
        let root = repo_root().unwrap();
        let output = tempfile::tempdir().unwrap();
        let binary = output.path().join(BINARY_NAME);
        fs::write(&binary, b"test binary").unwrap();
        let version = "1.2.3";
        let target = "aarch64-apple-darwin";

        for host in AGENT_HOSTS {
            let archive = output.path().join(format!(
                "visible-browser-lab-{}-{version}-{target}.zip",
                host.id
            ));
            write_plugin_archive(&root, host, target, version, &binary, &archive).unwrap();
        }

        let binary_archive = output
            .path()
            .join(format!("visible-browser-lab-mcp-{version}-{target}.zip"));
        write_binary_archive(target, version, &binary, &binary_archive).unwrap();
        validate_archives(output.path()).unwrap();
    }

    #[test]
    fn install_smoke_auth_requires_model_invocation() {
        let error = InstallSmokeArgs::parse(vec![
            "--auth-source".to_string(),
            "/tmp/codex-auth".to_string(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("requires --invoke-codex"));

        let args = InstallSmokeArgs::parse(vec![
            "--invoke-codex".to_string(),
            "--auth-source".to_string(),
            "/tmp/codex-auth".to_string(),
            "--keep-temp".to_string(),
        ])
        .unwrap();
        assert!(args.invoke_codex);
        assert!(args.keep_temp);
        assert_eq!(args.auth_source, Some(PathBuf::from("/tmp/codex-auth")));
    }

    #[test]
    fn codex_invocation_requires_only_the_expected_facade_sequence() {
        let events = ["start_session", "list_tabs", "evaluate", "close_tab"]
            .map(|tool| {
                json!({
                    "type": "item.completed",
                    "item": {
                        "type": "mcp_tool_call",
                        "server": "visible-browser-lab",
                        "tool": tool,
                        "status": "completed",
                        "error": null
                    }
                })
                .to_string()
            })
            .join("\n");
        validate_codex_invocation_events(&events).unwrap();

        let command = json!({
            "type": "item.completed",
            "item": { "type": "command_execution", "status": "completed" }
        });
        assert!(validate_codex_invocation_events(&format!("{events}\n{command}")).is_err());
    }
}
