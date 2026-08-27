//! Where `brow` keeps its socket, profiles, artifacts and logs.
//!
//! Everything lives under one root so that uninstalling is `rm -rf` of a single
//! directory, and so a user can see exactly what the tool has stored.

use std::path::{Path, PathBuf};

const MAX_SESSION_COMPONENT_BYTES: usize = 240;

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

/// Creates a directory tree and durably publishes every newly-created
/// directory entry before returning.
///
/// `create_dir_all` alone is not a host-crash durability boundary: a new child
/// can disappear unless its parent directory is synced. Walking the missing
/// chain deepest-first commits each child before the parent that names it.
pub(crate) fn create_dir_all_durable(path: &Path) -> std::io::Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut missing = Vec::new();
    let mut cursor = absolute.as_path();
    while !cursor.exists() {
        missing.push(cursor.to_path_buf());
        let Some(parent) = cursor.parent() else {
            break;
        };
        cursor = parent;
    }
    std::fs::create_dir_all(&absolute)?;
    #[cfg(unix)]
    for directory in &missing {
        if let Some(parent) = directory.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

/// Encodes one explicit artifact name without allowing traversal or aliases.
pub(crate) fn artifact_component(name: &str) -> std::io::Result<String> {
    if name.is_empty() || name.len() > 80 || name.chars().any(char::is_control) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "artifact name must be 1..=80 bytes and contain no control characters",
        ));
    }
    let encoded = sanitize(name);
    if encoded.len() > MAX_SESSION_COMPONENT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "encoded artifact name is too long",
        ));
    }
    Ok(encoded)
}

/// Refuses an ambiguous upgrade from the historical lossy session mapping.
///
/// Old versions mapped `/`, punctuation and non-ASCII to `_`, so several names
/// could share one profile. Automatically moving that directory would silently
/// assign another session's login state. Lowercase-safe historical names already
/// have the same path and require no migration; changed names must be inspected
/// and moved explicitly by the user.
pub fn ensure_session_storage_compatible(session: &str) -> std::io::Result<()> {
    ensure_session_storage_compatible_under(&root(), session)
}

fn ensure_session_storage_compatible_under(
    state_root: &std::path::Path,
    session: &str,
) -> std::io::Result<()> {
    let encoded = sanitize(session);
    if encoded.len() > MAX_SESSION_COMPONENT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "session name encodes to {} bytes; maximum is {MAX_SESSION_COMPONENT_BYTES}",
                encoded.len()
            ),
        ));
    }
    let legacy = legacy_sanitize(session);
    if legacy == encoded {
        return Ok(());
    }
    for kind in ["profiles", "artifacts"] {
        let legacy_path = state_root.join(kind).join(&legacy);
        if legacy_path.exists() {
            let encoded_path = state_root.join(kind).join(&encoded);
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "legacy {kind} path {} may belong to a colliding historical session; refusing automatic migration for {session:?}. Inspect it, then move it explicitly to {} or choose a new session name",
                    legacy_path.display(),
                    encoded_path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Root of all job directories.
pub fn jobs_root() -> PathBuf {
    root().join("jobs")
}

/// One job's artifacts and manifest.
pub fn job(id: &str) -> PathBuf {
    jobs_root().join(sanitize(id))
}

/// Creates the state tree with private permissions.
///
/// The socket lives inside a `0700` directory; anything that can open it can
/// drive a browser holding the user's logged-in sessions, so the directory mode
/// is a real access control, not hygiene.
pub fn ensure_layout() -> std::io::Result<()> {
    let root = root();
    // These directories are part of the daemon's persisted contract. Plain
    // `create_dir_all` can report success while the parent directory entry is
    // still only in the kernel cache, so a host crash could make a subsequently
    // fsynced manifest unreachable after restart.
    create_dir_all_durable(&root.join("run"))?;
    create_dir_all_durable(&logs())?;
    create_dir_all_durable(&root.join("profiles"))?;
    create_dir_all_durable(&root.join("artifacts"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in [root.clone(), root.join("run")] {
            let mut perms = std::fs::metadata(&dir)?.permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&dir, perms)?;
            // Persist the access-control metadata too. On a fresh layout the
            // directory may initially have been created under a permissive
            // umask; a crash must not resurrect that pre-chmod mode.
            sync_directory(&dir)?;
        }
    }
    Ok(())
}

/// Keeps a session name from escaping its directory.
fn sanitize(name: &str) -> String {
    // Percent-encoding bytes makes this mapping injective even on the default
    // case-insensitive macOS filesystem. Uppercase ASCII is encoded as well, so
    // sessions `A` and `a` cannot share a Chromium profile. Historical names made
    // only of lowercase ASCII, digits, `-`, and `_` remain byte-identical.
    if name.is_empty() {
        // A non-empty input containing these characters cannot collide with the
        // sentinel because `%` itself is encoded below.
        return "%EMPTY".to_string();
    }
    let mut cleaned = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_' {
            cleaned.push(char::from(byte));
        } else {
            cleaned.push_str(&format!("%{byte:02X}"));
        }
    }
    cleaned
}

fn legacy_sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
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
        assert_eq!(
            sanitize("../../etc/passwd"),
            "%2E%2E%2F%2E%2E%2Fetc%2Fpasswd"
        );
        assert_eq!(sanitize("ok-name_1"), "ok-name_1");
        assert_eq!(sanitize("default"), "default");
        assert_eq!(sanitize(""), "%EMPTY");
        assert_ne!(sanitize(""), sanitize("\0"));
        assert_eq!(sanitize("/"), "%2F");
        assert_ne!(sanitize("a/b"), sanitize("a_b"));
        assert_eq!(sanitize("A"), "%41");
        assert_ne!(sanitize("A"), sanitize("a"));
        assert_ne!(sanitize(""), sanitize("default"));
    }

    #[test]
    fn profile_paths_stay_inside_the_root() {
        let p = profile("../escape");
        assert!(p.starts_with(root().join("profiles")), "{}", p.display());
        assert!(!p.to_string_lossy().contains(".."));
    }

    #[test]
    fn ambiguous_legacy_session_storage_is_never_claimed_silently() {
        let scratch = std::env::temp_dir().join(format!(
            "brow-path-migration-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(scratch.join("profiles").join("a_b")).unwrap();

        let error = ensure_session_storage_compatible_under(&scratch, "a/b")
            .expect_err("lossy legacy path is ambiguous");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("refusing automatic migration"));
        assert!(ensure_session_storage_compatible_under(&scratch, "safe-name").is_ok());
        assert!(ensure_session_storage_compatible_under(&scratch, "fresh/name").is_ok());

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn oversized_session_names_fail_before_reaching_the_filesystem() {
        let error = ensure_session_storage_compatible_under(
            std::path::Path::new("/unused"),
            &"A".repeat(MAX_SESSION_COMPONENT_BYTES),
        )
        .expect_err("uppercase bytes expand to percent escapes");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn durable_directory_creation_builds_nested_parents() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("jobs/job_1/checkpoints");
        create_dir_all_durable(&nested).expect("create and sync nested directory chain");
        assert!(nested.is_dir());
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
