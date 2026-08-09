//! Finding a Chromium on this machine. We never download one.
//!
//! Downloading a browser binary is the single most common way an automation tool
//! turns into a supply-chain surface, so `brow` refuses to do it and fails with an
//! actionable message instead.

use std::path::{Path, PathBuf};

/// Env var that overrides discovery entirely.
pub const CHROME_ENV: &str = "BROW_CHROME";

/// A Chromium-family browser found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub path: PathBuf,
    pub flavor: Flavor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Chrome,
    Chromium,
    Edge,
    Brave,
    Unknown,
}

impl Flavor {
    fn label(self) -> &'static str {
        match self {
            Flavor::Chrome => "Google Chrome",
            Flavor::Chromium => "Chromium",
            Flavor::Edge => "Microsoft Edge",
            Flavor::Brave => "Brave",
            Flavor::Unknown => "Chromium-family browser",
        }
    }
}

impl std::fmt::Display for Installed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at {}", self.flavor.label(), self.path.display())
    }
}

/// Resolves the browser to drive, honouring `BROW_CHROME` first.
pub fn find() -> anyhow::Result<Installed> {
    if let Some(explicit) = std::env::var_os(CHROME_ENV) {
        let path = PathBuf::from(explicit);
        anyhow::ensure!(
            is_executable(&path),
            "{CHROME_ENV} points at {}, which is not an executable file",
            path.display()
        );
        return Ok(Installed {
            flavor: classify(&path),
            path,
        });
    }

    for candidate in candidates() {
        if is_executable(&candidate) {
            return Ok(Installed {
                flavor: classify(&candidate),
                path: candidate,
            });
        }
    }

    anyhow::bail!(
        "no Chromium-family browser found.\n\
         brow never downloads a browser. Install Google Chrome or Chromium, or point \
         brow at an existing binary:\n    {CHROME_ENV}=/path/to/chrome brow open <url>"
    )
}

fn candidates() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut out: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "macos")]
    {
        let apps = [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta",
            "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
            "Chromium.app/Contents/MacOS/Chromium",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "Brave Browser.app/Contents/MacOS/Brave Browser",
        ];
        for app in apps {
            out.push(PathBuf::from("/Applications").join(app));
            if let Some(home) = &home {
                out.push(home.join("Applications").join(app));
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        for name in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "brave-browser",
        ] {
            for dir in [
                "/usr/bin",
                "/usr/local/bin",
                "/snap/bin",
                "/opt/google/chrome",
            ] {
                out.push(PathBuf::from(dir).join(name));
            }
        }
    }

    // PATH lookup last: an explicitly installed app bundle beats whatever shim
    // happens to be on PATH.
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in ["google-chrome", "chromium", "chrome"] {
                out.push(dir.join(name));
            }
        }
    }

    out
}

fn classify(path: &Path) -> Flavor {
    let hay = path.to_string_lossy().to_lowercase();
    if hay.contains("brave") {
        Flavor::Brave
    } else if hay.contains("edge") {
        Flavor::Edge
    } else if hay.contains("chromium") {
        Flavor::Chromium
    } else if hay.contains("chrome") {
        Flavor::Chrome
    } else {
        Flavor::Unknown
    }
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_path() {
        assert_eq!(
            classify(Path::new(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
            )),
            Flavor::Chrome
        );
        assert_eq!(
            classify(Path::new("/usr/bin/chromium-browser")),
            Flavor::Chromium
        );
        assert_eq!(classify(Path::new("/usr/bin/brave-browser")), Flavor::Brave);
        // Edge's path also contains "chrome"-ish words on some platforms; edge wins.
        assert_eq!(
            classify(Path::new(
                "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"
            )),
            Flavor::Edge
        );
    }

    #[test]
    fn env_override_rejects_a_non_executable() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("not-a-browser");
        std::fs::write(&bogus, b"").unwrap();
        // Guard the process-global env var for the duration of this test only.
        let err = {
            std::env::set_var(CHROME_ENV, &bogus);
            let r = find();
            std::env::remove_var(CHROME_ENV);
            r.unwrap_err()
        };
        assert!(err.to_string().contains("not an executable"), "{err}");
    }
}
