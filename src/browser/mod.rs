//! Browser discovery, launch and process ownership.

pub mod discover;
pub mod launch;

pub use discover::{find, Flavor, Installed};
pub use launch::{launch, Headless, LaunchOptions, Launched};

/// Cross-process Chromium concurrency reservation for crate unit tests.
/// Integration tests use the same lock-file pool from `tests/common`.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) struct TestBrowserSlot(std::fs::File);

#[cfg(test)]
pub(crate) fn test_browser_slot() -> TestBrowserSlot {
    use std::os::unix::io::AsRawFd;

    let dir = std::path::Path::new("/tmp/brow-test-slots");
    let _ = std::fs::create_dir_all(dir);
    loop {
        // Match the integration-test pool protocol: an exclusive reservation
        // holds this gate while draining every slot, so unit tests must take a
        // shared gate while selecting one.
        let gate = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join("pool.gate"))
            .expect("open browser test pool gate");
        // SAFETY: `gate` remains open for the complete shared-lock lifetime.
        let result = unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_SH) };
        assert_eq!(result, 0, "lock shared browser test pool gate");
        for index in 0..4 {
            let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(dir.join(format!("{index}.lock")))
            else {
                continue;
            };
            // SAFETY: `file` stays open for the lifetime of the returned guard.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return TestBrowserSlot(file);
            }
        }
        drop(gate);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
