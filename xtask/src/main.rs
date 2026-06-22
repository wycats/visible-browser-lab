use std::{
    env,
    fs::{self, File},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zip::{
    CompressionMethod, ZipArchive, ZipWriter,
    write::{ExtendedFileOptions, FileOptions},
};

const BINARY_NAME: &str = "visible-browser-lab-mcp";
const DEFAULT_OUT_DIR: &str = "out/packages";
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
}

const AGENT_HOSTS: &[AgentHost] = &[
    AgentHost {
        id: "codex",
        display_name: "Codex",
        manifest_path: ".codex-plugin/plugin.json",
    },
    AgentHost {
        id: "claude-code",
        display_name: "Claude Code",
        manifest_path: ".claude-plugin/plugin.json",
    },
    AgentHost {
        id: "vscode",
        display_name: "VS Code",
        manifest_path: ".vscode-plugin/plugin.json",
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
  cargo xtask package [--target <target>] [--binary <path>] [--out-dir <dir>]
  cargo xtask checksums [--dir <dir>]
"
    );
}

#[derive(Debug)]
struct PackageArgs {
    target: String,
    binary: Option<PathBuf>,
    out_dir: PathBuf,
}

impl PackageArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut target = None;
        let mut binary = None;
        let mut out_dir = PathBuf::from(DEFAULT_OUT_DIR);
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
    let binary = binary_path(&root, &args.target, args.binary.as_deref())?;
    let out_dir = root.join(args.out_dir);
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create output directory `{}`", out_dir.display()))?;

    let mut archives = Vec::new();
    for host in AGENT_HOSTS {
        let archive = out_dir.join(format!(
            "visible-browser-lab-{}-{}.zip",
            host.id, args.target
        ));
        write_plugin_archive(&root, host, &args.target, &binary, &archive)?;
        archives.push(archive);
    }

    let binary_archive = out_dir.join(format!("visible-browser-lab-mcp-{}.zip", args.target));
    write_binary_archive(&args.target, &binary, &binary_archive)?;
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

fn write_plugin_archive(
    root: &Path,
    host: &AgentHost,
    target: &str,
    binary: &Path,
    archive: &Path,
) -> Result<()> {
    let file = File::create(archive)
        .with_context(|| format!("failed to create archive `{}`", archive.display()))?;
    let mut zip = ZipWriter::new(file);
    let binary_name = binary_file_name(target);
    let mcp_config = mcp_config_bytes(&binary_name)?;
    let manifest = host_manifest_bytes(root, host, target, &binary_name)?;
    let package_manifest = package_manifest_bytes(host, target, &binary_name)?;

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

fn write_binary_archive(target: &str, binary: &Path, archive: &Path) -> Result<()> {
    let file = File::create(archive)
        .with_context(|| format!("failed to create archive `{}`", archive.display()))?;
    let mut zip = ZipWriter::new(file);
    let binary_name = binary_file_name(target);
    let readme = format!(
        "\
visible-browser-lab MCP broker

target: {target}
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
    binary_name: &str,
) -> Result<Vec<u8>> {
    if host.id == "codex" {
        let source = fs::read_to_string(root.join(".codex-plugin/plugin.json"))
            .context("failed to read Codex plugin manifest")?;
        let mut manifest: Value =
            serde_json::from_str(&source).context("invalid Codex manifest JSON")?;
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
        "version": plugin_version(root)?,
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

fn mcp_config_bytes(binary_name: &str) -> Result<Vec<u8>> {
    let config = json!({
        "mcpServers": {
            "visible-browser-lab": {
                "command": format!("./bin/{binary_name}"),
                "args": []
            }
        }
    });
    Ok(serde_json::to_vec_pretty(&config)?)
}

fn package_manifest_bytes(host: &AgentHost, target: &str, binary_name: &str) -> Result<Vec<u8>> {
    let manifest = json!({
        "name": "visible-browser-lab",
        "host": host.id,
        "target": target,
        "binary": format!("bin/{binary_name}"),
        "mcp_server": "visible-browser-lab",
        "source_commit": git_head().ok(),
    });
    Ok(serde_json::to_vec_pretty(&manifest)?)
}

fn plugin_version(root: &Path) -> Result<String> {
    let source = fs::read_to_string(root.join(".codex-plugin/plugin.json"))
        .context("failed to read Codex plugin manifest")?;
    let manifest: Value = serde_json::from_str(&source).context("invalid Codex manifest JSON")?;
    manifest
        .get("version")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .context("Codex plugin manifest is missing string `version`")
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
    let mut mcp_config = None;

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let name = file.name().to_string();
        if forbidden_archive_path(&name) {
            bail!(
                "archive `{}` contains forbidden path `{name}`",
                path.display()
            );
        }

        if name == ".mcp.json" {
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            mcp_config = Some(contents);
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

    let has_host_manifest = AGENT_HOSTS
        .iter()
        .any(|host| names.iter().any(|name| name == host.manifest_path));
    if !has_host_manifest {
        bail!(
            "archive `{}` is missing a host plugin manifest",
            path.display()
        );
    }

    let mcp_config =
        mcp_config.with_context(|| format!("archive `{}` is missing .mcp.json", path.display()))?;
    if mcp_config.contains("npx")
        || mcp_config.contains("node")
        || mcp_config.contains("cargo")
        || !mcp_config.contains("./bin/visible-browser-lab-mcp")
    {
        bail!(
            "archive `{}` has an invalid generated MCP config",
            path.display()
        );
    }

    Ok(())
}

fn validate_binary_archive(path: &Path) -> Result<()> {
    let mut archive = open_zip(path)?;
    let mut binary_count = 0;

    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        let name = file.name();
        if forbidden_archive_path(name) {
            bail!(
                "archive `{}` contains forbidden path `{name}`",
                path.display()
            );
        }

        if name == BINARY_NAME || name == format!("{BINARY_NAME}.exe") {
            binary_count += 1;
        }
    }

    if binary_count != 1 {
        bail!(
            "archive `{}` must contain exactly one binary, found {binary_count}",
            path.display()
        );
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
