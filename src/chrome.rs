use std::path::{Path, PathBuf};

#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::env;
#[cfg(any(test, target_os = "macos"))]
use std::fs;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeFamily {
    GoogleChrome,
    ChromeForTesting,
    Chromium,
    MicrosoftEdge,
    Brave,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromeInstallation {
    pub family: ChromeFamily,
    pub executable: PathBuf,
    pub application_bundle: Option<PathBuf>,
}

pub fn discover_chrome(explicit_path: Option<&Path>) -> Result<ChromeInstallation> {
    if let Some(path) = explicit_path {
        return installation_from_explicit_path(path).with_context(|| {
            format!(
                "VISIBLE_BROWSER_LAB_CHROME_PATH `{}` does not identify a supported browser executable",
                path.display()
            )
        });
    }

    let candidates = platform_candidates();
    if let Some(candidate) = first_existing(&candidates) {
        return Ok(candidate);
    }

    let attempted = candidates
        .iter()
        .map(|candidate| format!("`{}`", candidate.executable.display()))
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "no supported Chrome-family browser was found; set VISIBLE_BROWSER_LAB_CHROME_PATH. Attempted: {attempted}"
    )
}

fn first_existing(candidates: &[ChromeInstallation]) -> Option<ChromeInstallation> {
    candidates
        .iter()
        .find(|candidate| candidate.executable.is_file())
        .cloned()
}

fn installation_from_explicit_path(path: &Path) -> Result<ChromeInstallation> {
    let path = path
        .canonicalize()
        .with_context(|| format!("failed to resolve `{}`", path.display()))?;

    #[cfg(target_os = "macos")]
    if path.extension().and_then(|extension| extension.to_str()) == Some("app") {
        let executable_dir = path.join("Contents/MacOS");
        let executable = first_file(&executable_dir)?.ok_or_else(|| {
            anyhow::anyhow!("`{}` contains no application executable", path.display())
        })?;
        return Ok(ChromeInstallation {
            family: infer_family(&path),
            executable,
            application_bundle: Some(path),
        });
    }

    if !path.is_file() {
        bail!("`{}` is not a file", path.display());
    }

    Ok(ChromeInstallation {
        family: infer_family(&path),
        application_bundle: application_bundle_for_executable(&path),
        executable: path,
    })
}

#[cfg(target_os = "macos")]
fn first_file(directory: &Path) -> Result<Option<PathBuf>> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("failed to read `{}`", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries
        .into_iter()
        .map(|entry| entry.path())
        .find(|path| path.is_file()))
}

fn infer_family(path: &Path) -> ChromeFamily {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    if lower.contains("chrome for testing") || lower.contains("chrome-for-testing") {
        ChromeFamily::ChromeForTesting
    } else if lower.contains("chromium") {
        ChromeFamily::Chromium
    } else if lower.contains("edge") {
        ChromeFamily::MicrosoftEdge
    } else if lower.contains("brave") {
        ChromeFamily::Brave
    } else {
        ChromeFamily::GoogleChrome
    }
}

#[cfg(target_os = "macos")]
fn application_bundle_for_executable(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| {
            ancestor
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("app")
        })
        .map(Path::to_path_buf)
}

#[cfg(not(target_os = "macos"))]
fn application_bundle_for_executable(_path: &Path) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn platform_candidates() -> Vec<ChromeInstallation> {
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    let applications = Path::new("/Applications");
    let definitions = [
        (
            ChromeFamily::GoogleChrome,
            "Google Chrome.app",
            "Google Chrome",
        ),
        (
            ChromeFamily::ChromeForTesting,
            "Google Chrome for Testing.app",
            "Google Chrome for Testing",
        ),
        (ChromeFamily::Chromium, "Chromium.app", "Chromium"),
        (
            ChromeFamily::MicrosoftEdge,
            "Microsoft Edge.app",
            "Microsoft Edge",
        ),
        (ChromeFamily::Brave, "Brave Browser.app", "Brave Browser"),
    ];
    let mut candidates = Vec::new();
    for (family, bundle_name, executable_name) in definitions {
        for root in [
            Some(applications.to_path_buf()),
            home.as_ref().map(|home| home.join("Applications")),
        ]
        .into_iter()
        .flatten()
        {
            let bundle = root.join(bundle_name);
            candidates.push(ChromeInstallation {
                family,
                executable: bundle.join("Contents/MacOS").join(executable_name),
                application_bundle: Some(bundle),
            });
        }
    }
    candidates
}

#[cfg(target_os = "linux")]
fn platform_candidates() -> Vec<ChromeInstallation> {
    let definitions = [
        (
            ChromeFamily::GoogleChrome,
            ["google-chrome-stable", "google-chrome"].as_slice(),
        ),
        (
            ChromeFamily::ChromeForTesting,
            ["chrome-for-testing"].as_slice(),
        ),
        (
            ChromeFamily::Chromium,
            ["chromium", "chromium-browser"].as_slice(),
        ),
        (
            ChromeFamily::MicrosoftEdge,
            ["microsoft-edge-stable", "microsoft-edge"].as_slice(),
        ),
        (
            ChromeFamily::Brave,
            ["brave-browser", "brave-browser-stable"].as_slice(),
        ),
    ];
    definitions
        .into_iter()
        .flat_map(|(family, names)| {
            names.iter().flat_map(move |name| {
                executable_paths(name).map(move |executable| ChromeInstallation {
                    family,
                    executable,
                    application_bundle: None,
                })
            })
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn platform_candidates() -> Vec<ChromeInstallation> {
    let roots = [
        env::var_os("PROGRAMFILES").map(PathBuf::from),
        env::var_os("PROGRAMFILES(X86)").map(PathBuf::from),
        env::var_os("LOCALAPPDATA").map(PathBuf::from),
    ];
    let definitions = [
        (
            ChromeFamily::GoogleChrome,
            "Google/Chrome/Application/chrome.exe",
        ),
        (
            ChromeFamily::ChromeForTesting,
            "Google/Chrome for Testing/Application/chrome.exe",
        ),
        (ChromeFamily::Chromium, "Chromium/Application/chrome.exe"),
        (
            ChromeFamily::MicrosoftEdge,
            "Microsoft/Edge/Application/msedge.exe",
        ),
        (
            ChromeFamily::Brave,
            "BraveSoftware/Brave-Browser/Application/brave.exe",
        ),
    ];
    definitions
        .into_iter()
        .flat_map(|(family, relative)| {
            roots.iter().flatten().map(move |root| ChromeInstallation {
                family,
                executable: root.join(relative),
                application_bundle: None,
            })
        })
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn platform_candidates() -> Vec<ChromeInstallation> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn executable_paths(name: &str) -> impl Iterator<Item = PathBuf> + '_ {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(move |directory| directory.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_executable_is_resolved_and_classified() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("Microsoft Edge");
        fs::write(&executable, b"test").unwrap();

        let installation = discover_chrome(Some(&executable)).unwrap();

        assert_eq!(installation.family, ChromeFamily::MicrosoftEdge);
        assert_eq!(installation.executable, executable.canonicalize().unwrap());
    }

    #[test]
    fn explicit_missing_path_reports_the_override() {
        let error = discover_chrome(Some(Path::new("/missing/visible-browser-lab-chrome")))
            .unwrap_err()
            .to_string();

        assert!(error.contains("VISIBLE_BROWSER_LAB_CHROME_PATH"));
        assert!(error.contains("visible-browser-lab-chrome"));
    }

    #[test]
    fn family_inference_distinguishes_supported_browsers() {
        assert_eq!(
            infer_family(Path::new("/tmp/Google Chrome for Testing")),
            ChromeFamily::ChromeForTesting
        );
        assert_eq!(
            infer_family(Path::new("/tmp/Chromium")),
            ChromeFamily::Chromium
        );
        assert_eq!(
            infer_family(Path::new("/tmp/Brave Browser")),
            ChromeFamily::Brave
        );
    }

    #[test]
    fn discovery_preserves_candidate_precedence() {
        let directory = tempfile::tempdir().unwrap();
        let chrome = directory.path().join("Google Chrome");
        let edge = directory.path().join("Microsoft Edge");
        fs::write(&chrome, b"chrome").unwrap();
        fs::write(&edge, b"edge").unwrap();
        let candidates = vec![
            ChromeInstallation {
                family: ChromeFamily::GoogleChrome,
                executable: chrome.clone(),
                application_bundle: None,
            },
            ChromeInstallation {
                family: ChromeFamily::MicrosoftEdge,
                executable: edge,
                application_bundle: None,
            },
        ];

        let selected = first_existing(&candidates).unwrap();

        assert_eq!(selected.family, ChromeFamily::GoogleChrome);
        assert_eq!(selected.executable, chrome);
    }
}
