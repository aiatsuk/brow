//! Where `brow` keeps its socket, profiles, artifacts and logs.
//!
//! Everything lives under one root so that uninstalling is `rm -rf` of a single
//! directory, and so a user can see exactly what the tool has stored.

use std::path::PathBuf;

/// Overrides the state root entirely (used by tests and by per-project setups).
pub const HOME_ENV: &str = "BROW_HOME";

/// `~/.brow`, or `$BROW_HOME`.
pub fn root() -> PathBuf {
    if let Some(explicit) = std::env::var_os(HOME_ENV) {
        return PathBuf::from(explicit);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".brow")
}

/// The control socket.
///
/// Deliberately *not* under `$XDG_RUNTIME_DIR`: `sun_path` is 104 bytes on macOS
/// and some runtime dirs are long enough to overflow it, which fails at bind time
/// with a confusing error. One predictable location is worth more here than
/// spec-purity.
pub fn socket() -> PathBuf {
    root().join("run").join("brow.sock")
}

pub fn logs() -> PathBuf {
    root().join("logs")
}

pub fn daemon_log() -> PathBuf {
    logs().join("browd.log")
}

pub fn pid_file() -> PathBuf {
    root().join("run").join("browd.pid")
}

/// Per-session browser profile.
pub fn profile(session: &str) -> PathBuf {
    root().join("profiles").join(sanitize(session))
}

/// Where screenshots and other outputs land when no path is given.
pub fn artifacts(session: &str) -> PathBuf {
    root().join("artifacts").join(sanitize(session))
}

/// Creates the state tree with private permissions.
///
/// The socket lives inside a `0700` directory; anything that can open it can
/// drive a browser holding the user's logged-in sessions, so the directory mode
/// is a real access control, not hygiene.
pub fn ensure_layout() -> std::io::Result<()> {
    let root = root();
    std::fs::create_dir_all(root.join("run"))?;
    std::fs::create_dir_all(logs())?;
    std::fs::create_dir_all(root.join("profiles"))?;
    std::fs::create_dir_all(root.join("artifacts"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in [root.clone(), root.join("run")] {
            let mut perms = std::fs::metadata(&dir)?.permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&dir, perms)?;
        }
    }
    Ok(())
}

/// Keeps a session name from escaping its directory.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_cannot_traverse_directories() {
        assert_eq!(sanitize("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize("ok-name_1"), "ok-name_1");
        assert_eq!(sanitize(""), "default");
        assert_eq!(sanitize("/"), "_");
    }

    #[test]
    fn profile_paths_stay_inside_the_root() {
        let p = profile("../escape");
        assert!(p.starts_with(root().join("profiles")), "{}", p.display());
        assert!(!p.to_string_lossy().contains(".."));
    }

    #[test]
    fn socket_path_fits_in_sun_path() {
        // 104 bytes on macOS, 108 on Linux; bind() truncates or fails otherwise.
        let s = socket();
        assert!(
            s.as_os_str().len() < 100,
            "socket path {} is {} bytes, too long for sun_path",
            s.display(),
            s.as_os_str().len()
        );
    }
}
