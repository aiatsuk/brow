//! Generation-coherent, atomically published evidence bundles.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::ipc::WaitPolicy;
use crate::page::{
    ActionReceipt, Capture, ConsoleEntry, CoverageGap, ImageFormat, NetworkEntry, Page,
    ScreenshotTarget, Snapshot,
};
use crate::{paths, redact};

const DIAGNOSTIC_LIMIT: usize = 200;
const CAPTURE_ATTEMPTS: usize = 3;
static PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Page-scoped same-document mutation used to force one incoherent capture.
/// Matching both target and session prevents parallel checkpoint tests from
/// consuming the hook or receiving its fixture-specific JavaScript.
#[cfg(test)]
mod coherent_capture_test_support {
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    pub(super) struct Hook {
        target_id: String,
        session_id: String,
        phase: AtomicU8,
        attempts: AtomicU64,
    }

    fn slot() -> &'static Mutex<Option<Arc<Hook>>> {
        static SLOT: OnceLock<Mutex<Option<Arc<Hook>>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    pub(super) struct InterleaveHold {
        hook: Arc<Hook>,
    }

    impl InterleaveHold {
        pub(super) fn attempts(&self) -> u64 {
            self.hook.attempts.load(Ordering::SeqCst)
        }
    }

    impl Drop for InterleaveHold {
        fn drop(&mut self) {
            let mut slot = slot().lock().expect("coherent capture test hook");
            if slot
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.hook))
            {
                *slot = None;
            }
        }
    }

    pub(super) fn hold(target_id: String, session_id: String) -> InterleaveHold {
        let hook = Arc::new(Hook {
            target_id,
            session_id,
            phase: AtomicU8::new(1),
            attempts: AtomicU64::new(0),
        });
        let mut slot = slot().lock().expect("coherent capture test hook");
        assert!(
            slot.is_none(),
            "a coherent capture test hook is already held"
        );
        *slot = Some(Arc::clone(&hook));
        InterleaveHold { hook }
    }

    pub(super) fn matching(target_id: &str, session_id: &str) -> Option<Arc<Hook>> {
        slot()
            .lock()
            .expect("coherent capture test hook")
            .as_ref()
            .filter(|hook| hook.target_id == target_id && hook.session_id == session_id)
            .cloned()
    }

    pub(super) fn enter(hook: &Hook) -> bool {
        hook.attempts.fetch_add(1, Ordering::SeqCst);
        hook.phase
            .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

/// Scoped barrier for forcing independent real `create` calls to race at the
/// no-replace rename rather than at the earlier existence check or capture.
#[cfg(test)]
mod publication_race_test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use tokio::sync::Notify;

    struct Barrier {
        final_path: PathBuf,
        participants: usize,
        entered: AtomicUsize,
        released: AtomicBool,
        entered_notify: Notify,
        released_notify: Notify,
    }

    impl Barrier {
        fn new(final_path: PathBuf, participants: usize) -> Self {
            Self {
                final_path,
                participants,
                entered: AtomicUsize::new(0),
                released: AtomicBool::new(false),
                entered_notify: Notify::new(),
                released_notify: Notify::new(),
            }
        }

        async fn wait_for_count(&self) {
            while self.entered.load(Ordering::Acquire) < self.participants {
                let notified = self.entered_notify.notified();
                if self.entered.load(Ordering::Acquire) >= self.participants {
                    break;
                }
                notified.await;
            }
        }

        async fn wait_for_release(&self) {
            while !self.released.load(Ordering::Acquire) {
                let notified = self.released_notify.notified();
                if self.released.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
        }

        fn enter(&self) {
            let entered = self.entered.fetch_add(1, Ordering::AcqRel) + 1;
            assert!(
                entered <= self.participants,
                "too many checkpoint create calls entered the scoped race barrier"
            );
            self.entered_notify.notify_waiters();
        }

        fn release(&self) {
            self.released.store(true, Ordering::Release);
            self.released_notify.notify_waiters();
        }
    }

    fn slot() -> &'static Mutex<Option<Arc<Barrier>>> {
        static SLOT: OnceLock<Mutex<Option<Arc<Barrier>>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    pub(super) struct PublicationRaceHold {
        barrier: Arc<Barrier>,
    }

    impl PublicationRaceHold {
        pub(super) async fn wait_until_all_entered(&self) {
            self.barrier.wait_for_count().await;
        }

        pub(super) fn release(&self) {
            self.barrier.release();
        }
    }

    impl Drop for PublicationRaceHold {
        fn drop(&mut self) {
            let mut slot = slot().lock().expect("checkpoint create race barrier");
            if slot
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.barrier))
            {
                *slot = None;
            }
            self.barrier.release();
        }
    }

    pub(super) fn hold(final_path: PathBuf, participants: usize) -> PublicationRaceHold {
        assert!(participants >= 2, "a race needs at least two participants");
        let barrier = Arc::new(Barrier::new(final_path, participants));
        let mut slot = slot().lock().expect("checkpoint create race barrier");
        assert!(slot.is_none(), "a checkpoint create race is already held");
        *slot = Some(Arc::clone(&barrier));
        PublicationRaceHold { barrier }
    }

    pub(super) async fn hold_if_requested(final_path: &Path) {
        let barrier = slot()
            .lock()
            .expect("checkpoint create race barrier")
            .as_ref()
            .filter(|pending| pending.final_path == final_path)
            .cloned();
        let Some(barrier) = barrier else {
            return;
        };
        barrier.enter();
        barrier.wait_for_release().await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error(transparent)]
    Page(#[from] crate::page::PageError),
    #[error("checkpoint path error: {0}")]
    Path(#[from] std::io::Error),
    #[error("checkpoint {0} already exists; refusing to overwrite it")]
    Collision(PathBuf),
    #[error(
        "checkpoint {final_path} was published concurrently; refusing to overwrite it; \
         this attempt remains recoverable at {partial_path}"
    )]
    ConcurrentCollision {
        final_path: PathBuf,
        partial_path: PathBuf,
    },
    #[error(
        "page content or diagnostics changed during {CAPTURE_ATTEMPTS} checkpoint capture attempts"
    )]
    Unstable,
    #[error("could not encode checkpoint JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct CheckpointOptions {
    pub session: String,
    pub name: String,
    pub full_page: bool,
    pub wait: WaitPolicy,
    pub timeout: Duration,
    pub quiet: Duration,
    pub park_pointer: bool,
    pub output_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEvidence {
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointResult {
    pub path: PathBuf,
    pub manifest: CheckpointManifest,
    pub durability: CheckpointDurability,
}

/// Durability of an already-published checkpoint directory.
///
/// `PublishedSyncUnknown` is still a successful, non-retryable publication:
/// the no-replace rename completed and the final path is authoritative, but
/// syncing the parent directory failed, so survival across a host crash is not
/// known. Callers must surface the uncertainty without retrying the create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckpointDurability {
    Durable,
    PublishedSyncUnknown { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub schema_version: u32,
    pub name: String,
    pub created_ms: u128,
    pub url: String,
    pub title: String,
    pub generation: u64,
    pub complete: bool,
    pub pointer_parked: bool,
    pub wait: ActionReceipt,
    pub screenshot: ScreenshotEvidence,
    pub diagnostics: DiagnosticEvidence,
    pub coverage_gaps: Vec<CoverageGap>,
    pub files: BTreeMap<String, FileEvidence>,
    pub brow_version: String,
    pub browser_version: String,
    pub privacy_warning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenshotEvidence {
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub tile_count: usize,
    pub tiled: bool,
    pub truncated: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticEvidence {
    pub console_observed: usize,
    pub console_retained: usize,
    pub console_limit: usize,
    pub console_ring_dropped: u64,
    pub network_observed: usize,
    pub network_retained: usize,
    pub network_limit: usize,
    pub network_ring_dropped: u64,
    pub event_stream_gaps: u64,
}

#[derive(Debug, Clone, Serialize)]
struct CapturedDiagnostics {
    console_all: Vec<ConsoleEntry>,
    network_all: Vec<NetworkEntry>,
    console_ring_dropped: u64,
    network_ring_dropped: u64,
    event_stream_gaps: u64,
}

struct CoherentCapture {
    snapshot: Snapshot,
    screenshot: Capture,
    diagnostics: CapturedDiagnostics,
}

pub async fn create(
    page: &mut Page,
    options: CheckpointOptions,
) -> Result<CheckpointResult, CheckpointError> {
    // Checkpoint names are user-controlled and become both a directory entry
    // and manifest/log text. Redact before either durable representation; two
    // names that differ only by a credential intentionally collide instead of
    // leaking that credential through the filesystem namespace.
    let durable_name = redact::storage_text(&options.name);
    let safe_name = paths::artifact_component(&durable_name)?;
    let root = options
        .output_root
        .clone()
        .unwrap_or_else(|| paths::artifacts(&options.session).join("checkpoints"));
    paths::create_dir_all_durable(&root)?;
    let final_path = root.join(&safe_name);
    if final_path.exists() {
        return Err(CheckpointError::Collision(final_path));
    }

    let pointer_parked = if options.park_pointer {
        page.park_pointer().await?;
        true
    } else {
        false
    };
    let wait = page
        .prepare_checkpoint(options.wait, options.timeout, options.quiet)
        .await?;

    let CoherentCapture {
        snapshot,
        screenshot,
        diagnostics,
    } = capture_coherent(
        page,
        if options.full_page {
            ScreenshotTarget::FullPage
        } else {
            ScreenshotTarget::Viewport
        },
    )
    .await?;

    let CapturedDiagnostics {
        console_all,
        network_all,
        console_ring_dropped,
        network_ring_dropped,
        event_stream_gaps,
    } = diagnostics;
    let console: Vec<_> = console_all
        .iter()
        .rev()
        .take(DIAGNOSTIC_LIMIT)
        .cloned()
        .rev()
        .collect();
    let network: Vec<_> = network_all
        .iter()
        .rev()
        .take(DIAGNOSTIC_LIMIT)
        .cloned()
        .rev()
        .collect();
    let browser_version = page.browser_version().await?;

    let sequence = PARTIAL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let partial = root.join(format!(
        ".{safe_name}.partial-{}-{sequence}",
        std::process::id()
    ));
    tokio::fs::create_dir(&partial).await?;

    let result = async {
        let mut snapshot_value = serde_json::to_value(&snapshot)?;
        redact_value(&mut snapshot_value, None);
        let snapshot_bytes = serde_json::to_vec_pretty(&snapshot_value)?;
        let console_bytes = serde_json::to_vec_pretty(&console)?;
        let network_bytes = serde_json::to_vec_pretty(&network)?;

        let mut files = BTreeMap::new();
        let mut fault = FaultPlan::none();
        write_file(
            &partial.join("snapshot.json"),
            &snapshot_bytes,
            &mut fault,
        )
        .await?;
        files.insert("snapshot.json".into(), evidence(&snapshot_bytes));
        write_file(
            &partial.join("screenshot.png"),
            &screenshot.bytes,
            &mut fault,
        )
        .await?;
        files.insert("screenshot.png".into(), evidence(&screenshot.bytes));
        write_file(
            &partial.join("console-errors.json"),
            &console_bytes,
            &mut fault,
        )
        .await?;
        files.insert("console-errors.json".into(), evidence(&console_bytes));
        write_file(
            &partial.join("network-failures.json"),
            &network_bytes,
            &mut fault,
        )
        .await?;
        files.insert("network-failures.json".into(), evidence(&network_bytes));

        let (width, height) = png_dimensions(&screenshot.bytes).unwrap_or((0, 0));
        let manifest = CheckpointManifest {
            schema_version: 1,
            name: durable_name.clone(),
            created_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default(),
            url: redact::url(&snapshot.url),
            title: redact::storage_text(&snapshot.title),
            generation: snapshot.generation,
            complete: event_stream_gaps == 0
                && snapshot.coverage_gaps.is_empty()
                && console_ring_dropped == 0
                && network_ring_dropped == 0
                && console_all.len() <= DIAGNOSTIC_LIMIT
                && network_all.len() <= DIAGNOSTIC_LIMIT,
            pointer_parked,
            wait: wait.clone(),
            screenshot: ScreenshotEvidence {
                format: "png".into(),
                width,
                height,
                tile_count: screenshot.tile_count,
                tiled: screenshot.tiled,
                truncated: screenshot.truncated.clone(),
            },
            diagnostics: DiagnosticEvidence {
                console_observed: console_all.len(),
                console_retained: console.len(),
                console_limit: DIAGNOSTIC_LIMIT,
                console_ring_dropped,
                network_observed: network_all.len(),
                network_retained: network.len(),
                network_limit: DIAGNOSTIC_LIMIT,
                network_ring_dropped,
                event_stream_gaps,
            },
            coverage_gaps: snapshot
                .coverage_gaps
                .iter()
                .cloned()
                .map(|mut gap| {
                    gap.reason = redact::storage_text(&gap.reason);
                    gap
                })
                .collect(),
            files,
            brow_version: env!("CARGO_PKG_VERSION").into(),
            browser_version,
            privacy_warning: "Best-effort secret-shape redaction does not remove arbitrary private content from DOM snapshots or images.".into(),
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        // Publication marker is deliberately written last.
        write_file(
            &partial.join("manifest.json"),
            &manifest_bytes,
            &mut fault,
        )
        .await?;
        let durability = publish_partial(&partial, &final_path, &root, &mut fault).await?;
        Ok::<_, CheckpointError>((manifest, durability))
    }
    .await;

    match result {
        Ok((manifest, durability)) => Ok(CheckpointResult {
            path: final_path,
            manifest,
            durability,
        }),
        Err(error) => Err(match error {
            CheckpointError::Path(io) if io.kind() == std::io::ErrorKind::AlreadyExists => {
                CheckpointError::ConcurrentCollision {
                    final_path,
                    partial_path: partial,
                }
            }
            CheckpointError::Path(io) => CheckpointError::Path(std::io::Error::new(
                io.kind(),
                format!(
                    "{}; incomplete evidence remains at {}",
                    io,
                    partial.display()
                ),
            )),
            other => other,
        }),
    }
}

async fn capture_coherent(
    page: &mut Page,
    target: ScreenshotTarget,
) -> Result<CoherentCapture, CheckpointError> {
    for _ in 0..CAPTURE_ATTEMPTS {
        // Bracket both the pixel capture and diagnostics with two independent
        // DOM snapshots. A document generation only detects cross-document
        // replacement; the content digest additionally catches same-document
        // mutation between the state and pixel reads.
        let before = page.snapshot().await?;
        let generation = before.generation;
        let before_content = snapshot_content_digest(&before)?;
        #[cfg(test)]
        run_same_document_interleave_hook(page).await?;
        let diagnostics_before = capture_diagnostics(page);
        let screenshot = page
            .screenshot(target.clone(), ImageFormat::Png, None)
            .await?;
        let diagnostics_after = capture_diagnostics(page);
        let after = page.snapshot().await?;
        let after_content = snapshot_content_digest(&after)?;

        if capture_epoch_matches(
            generation,
            page.generation(),
            after.generation,
            &before_content,
            &after_content,
            &diagnostics_before,
            &diagnostics_after,
        )? {
            return Ok(CoherentCapture {
                snapshot: after,
                screenshot,
                diagnostics: diagnostics_after,
            });
        }
    }
    Err(CheckpointError::Unstable)
}

#[cfg(test)]
async fn run_same_document_interleave_hook(page: &Page) -> Result<(), CheckpointError> {
    let Some(hook) = coherent_capture_test_support::matching(&page.target_id, &page.session_id)
    else {
        return Ok(());
    };
    if coherent_capture_test_support::enter(&hook) {
        page.evaluate(
            "document.querySelector('#marker').textContent = 'marker-B'; \
             document.querySelector('#pixel').style.background = 'rgb(0, 0, 255)'; \
             new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))",
            false,
        )
        .await?;
    }
    Ok(())
}

fn snapshot_content_digest(snapshot: &Snapshot) -> Result<[u8; 32], serde_json::Error> {
    let bytes = serde_json::to_vec(snapshot)?;
    Ok(Sha256::digest(bytes).into())
}

fn capture_diagnostics(page: &Page) -> CapturedDiagnostics {
    let console_all = page.events.console(true, usize::MAX);
    let network_all = page.events.network(true, usize::MAX);
    let (console_ring_dropped, network_ring_dropped) = page.events.dropped();
    CapturedDiagnostics {
        console_all,
        network_all,
        console_ring_dropped,
        network_ring_dropped,
        event_stream_gaps: page.events.event_stream_gaps(),
    }
}

fn capture_epoch_matches(
    expected_generation: u64,
    current_generation: u64,
    final_generation: u64,
    before_content: &[u8; 32],
    after_content: &[u8; 32],
    diagnostics_before: &CapturedDiagnostics,
    diagnostics_after: &CapturedDiagnostics,
) -> Result<bool, serde_json::Error> {
    Ok(expected_generation == current_generation
        && expected_generation == final_generation
        && before_content == after_content
        && serde_json::to_vec(diagnostics_before)? == serde_json::to_vec(diagnostics_after)?)
}

fn evidence(bytes: &[u8]) -> FileEvidence {
    FileEvidence {
        sha256: format!("{:x}", Sha256::digest(bytes)),
        bytes: bytes.len(),
    }
}

#[derive(Default)]
struct FaultPlan {
    fail_at: Option<usize>,
    seen: usize,
}

impl FaultPlan {
    fn none() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn at(index: usize) -> Self {
        Self {
            fail_at: Some(index),
            seen: 0,
        }
    }

    fn boundary(&mut self, label: &str) -> std::io::Result<()> {
        let index = self.seen;
        self.seen += 1;
        if self.fail_at == Some(index) {
            return Err(std::io::Error::other(format!(
                "injected checkpoint failure at {label}"
            )));
        }
        Ok(())
    }
}

async fn write_file(path: &Path, bytes: &[u8], fault: &mut FaultPlan) -> std::io::Result<()> {
    fault.boundary("file open")?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    fault.boundary("file write")?;
    file.write_all(bytes).await?;
    fault.boundary("file flush")?;
    file.flush().await?;
    fault.boundary("file sync")?;
    file.sync_all().await
}

async fn sync_directory(path: &Path, fault: &mut FaultPlan) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fault.boundary("directory open")?;
        let file = tokio::fs::File::open(path).await?;
        fault.boundary("directory sync")?;
        file.sync_all().await?;
    }
    Ok(())
}

async fn publish_partial(
    partial: &Path,
    final_path: &Path,
    root: &Path,
    fault: &mut FaultPlan,
) -> std::io::Result<CheckpointDurability> {
    sync_directory(partial, fault).await?;
    #[cfg(test)]
    publication_race_test_support::hold_if_requested(final_path).await;
    publish_rename_and_sync(partial, final_path, root, fault)
}

/// Performs the publication point and its durability proof without an await
/// boundary. A job's terminal timeout is allowed to cancel checkpoint capture;
/// once rename succeeds, cancellation must not strand a visible directory whose
/// parent entry was never fsynced while a later job manifest records its path.
fn publish_rename_and_sync(
    partial: &Path,
    final_path: &Path,
    root: &Path,
    fault: &mut FaultPlan,
) -> std::io::Result<CheckpointDurability> {
    fault.boundary("directory rename")?;
    rename_no_replace(partial, final_path)?;

    // The rename is the publication point. Once it succeeds, the bundle is
    // complete and hash-verifiable; a root-directory fsync failure must not
    // make the caller retry and collide with evidence that is already public.
    let root_sync = (|| {
        #[cfg(unix)]
        {
            fault.boundary("directory open")?;
            let directory = std::fs::File::open(root)?;
            fault.boundary("directory sync")?;
            directory.sync_all()?;
        }
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = root_sync {
        tracing::warn!(
            %error,
            path = %final_path.display(),
            "checkpoint published but root directory sync failed"
        );
        return Ok(CheckpointDurability::PublishedSyncUnknown {
            error: error.to_string(),
        });
    }
    Ok(CheckpointDurability::Durable)
}

#[cfg(target_os = "macos")]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let from = path_c_string(from)?;
    let to = path_c_string(to)?;
    // SAFETY: both C strings live through the call and contain no interior NUL.
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let from = path_c_string(from)?;
    let to = path_c_string(to)?;
    // SAFETY: renameat2 receives valid, NUL-terminated path pointers. The
    // no-replace flag makes the non-overwrite guarantee one kernel operation.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn path_c_string(path: &Path) -> std::io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path contains an interior NUL: {}", path.display()),
        )
    })
}

#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    // Windows rename already fails when the destination exists.
    std::fs::rename(from, to)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn rename_no_replace(_from: &Path, _to: &Path) -> std::io::Result<()> {
    // Fail closed on platforms where the standard rename operation may replace
    // a pre-existing destination and no atomic no-replace primitive is wired.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace directory publication is unsupported on this platform",
    ))
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    Some((
        u32::from_be_bytes(bytes[16..20].try_into().ok()?),
        u32::from_be_bytes(bytes[20..24].try_into().ok()?),
    ))
}

fn redact_value(value: &mut Value, key: Option<&str>) {
    match value {
        Value::String(text) => {
            *text = if key.is_some_and(is_url_key) {
                // Snapshot attributes use names such as href/src/action rather
                // than a generic `url` field. Apply both URL-aware redaction
                // (including userinfo) and assignment redaction (including
                // malformed/relative URL strings and srcset lists).
                redact::storage_text(&redact::url(text))
            } else {
                redact::storage_text(text)
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item, key);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                redact_value(value, Some(key));
            }
        }
        _ => {}
    }
}

fn is_url_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "url"
            | "href"
            | "src"
            | "srcset"
            | "action"
            | "formaction"
            | "poster"
            | "cite"
            | "data"
            | "background"
            | "xlink:href"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_url_attributes_are_redacted_before_bundle_serialization() {
        let mut snapshot = serde_json::json!({
            "nodes": [{
                "attrs": {
                    "href": "https://alice:swordfish@app.test/path?access_token=href-secret",
                    "SRC": "https://cdn.test/image.png#refresh_token=src-secret",
                    "formaction": "/submit?api_key=form-secret",
                    "srcset": "https://bob:second@one.test/a.png 1x, https://two.test/b.png?token=set-secret 2x",
                    "title": "ordinary accessible label"
                }
            }]
        });

        redact_value(&mut snapshot, None);
        let encoded = serde_json::to_string(&snapshot).expect("serialize redacted snapshot");
        for secret in [
            "alice",
            "swordfish",
            "href-secret",
            "src-secret",
            "form-secret",
            "bob",
            "second",
            "set-secret",
        ] {
            assert!(
                !encoded.contains(secret),
                "snapshot leaked {secret}: {encoded}"
            );
        }
        assert!(
            encoded.contains(redact::MASK),
            "redaction marker is present"
        );
        assert!(
            encoded.contains("ordinary accessible label"),
            "non-secret attributes remain useful: {encoded}"
        );
    }

    fn empty_diagnostics() -> CapturedDiagnostics {
        CapturedDiagnostics {
            console_all: Vec::new(),
            network_all: Vec::new(),
            console_ring_dropped: 0,
            network_ring_dropped: 0,
            event_stream_gaps: 0,
        }
    }

    fn state_digest(state: &str) -> [u8; 32] {
        Sha256::digest(state.as_bytes()).into()
    }

    fn png_rgba(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(
            png::Transformations::EXPAND
                | png::Transformations::STRIP_16
                | png::Transformations::ALPHA,
        );
        let mut reader = decoder.read_info().expect("PNG header");
        let mut pixels = vec![0; reader.output_buffer_size().expect("PNG buffer size")];
        let info = reader.next_frame(&mut pixels).expect("PNG pixels");
        pixels.truncate(info.buffer_size());
        assert_eq!(info.color_type, png::ColorType::Rgba);
        (info.width, info.height, pixels)
    }

    async fn stage_fixture(
        root: &Path,
        partial_name: &str,
        fault: &mut FaultPlan,
    ) -> std::io::Result<PathBuf> {
        tokio::fs::create_dir_all(root).await?;
        let partial = root.join(partial_name);
        tokio::fs::create_dir(&partial).await?;
        for name in [
            "snapshot.json",
            "screenshot.png",
            "console-errors.json",
            "network-failures.json",
            "manifest.json",
        ] {
            write_file(&partial.join(name), name.as_bytes(), fault).await?;
        }
        Ok(partial)
    }

    async fn publish_fixture(
        root: &Path,
        fault: &mut FaultPlan,
    ) -> std::io::Result<CheckpointDurability> {
        let partial = stage_fixture(root, ".bundle.partial", fault).await?;
        let final_path = root.join("bundle");
        publish_partial(&partial, &final_path, root, fault).await
    }

    #[test]
    fn png_header_dimensions_are_read_without_decoding_pixels() {
        let mut png = vec![0u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&1280u32.to_be_bytes());
        png[20..24].copy_from_slice(&7740u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((1280, 7740)));
    }

    #[test]
    fn evidence_hash_is_sha256() {
        assert_eq!(
            evidence(b"abc").sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn same_generation_content_or_diagnostic_skew_is_rejected_before_retry() {
        let diagnostics = empty_diagnostics();
        let mut changed_diagnostics = empty_diagnostics();
        changed_diagnostics.event_stream_gaps = 1;
        let a = state_digest("marker-A/red");
        let b = state_digest("marker-B/blue");

        assert!(
            !capture_epoch_matches(7, 7, 7, &a, &b, &diagnostics, &diagnostics)
                .expect("compare content epoch")
        );
        assert!(
            !capture_epoch_matches(7, 7, 7, &b, &b, &diagnostics, &changed_diagnostics,)
                .expect("compare diagnostic epoch")
        );

        // Deterministic interleave schedule: attempt one snapshots A, then the
        // hook changes DOM/pixels to B without a generation bump; attempt two
        // brackets the already-mutated B state. The production predicate must
        // reject the skewed attempt and accept only the coherent retry.
        let attempts = [(a, b), (b, b)];
        let accepted = attempts
            .iter()
            .position(|(before, after)| {
                capture_epoch_matches(7, 7, 7, before, after, &diagnostics, &diagnostics).unwrap()
            })
            .expect("the coherent B/B retry must be accepted");
        assert_eq!(accepted, 1, "the mixed A/B attempt was accepted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_same_document_interleave_retries_to_matching_dom_and_pixels() {
        if crate::browser::find().is_err() {
            eprintln!("SKIP real_same_document_interleave_retries_to_matching_dom_and_pixels");
            return;
        }
        let _slot = crate::browser::test_browser_slot();
        let scratch = tempfile::tempdir().unwrap();
        let mut options = crate::browser::LaunchOptions::new(scratch.path().join("profile"));
        options.headless = crate::browser::Headless::New;
        options.window_size = (320, 240);
        let mut launched = crate::browser::launch(&options)
            .await
            .expect("launch Chromium");
        let mut page = Page::create(std::sync::Arc::clone(&launched.client), "about:blank")
            .await
            .expect("create checkpoint page");
        page.evaluate(
            "document.documentElement.innerHTML = `<head><style>\
             html,body{margin:0;background:white}#pixel{position:fixed;left:0;top:0;width:20px;\
             height:20px;background:rgb(255,0,0)}</style></head><body><div id='pixel'></div>\
             <div id='marker'>marker-A</div></body>`; \
             new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))",
            false,
        )
        .await
        .expect("install marker A");
        let generation = page.generation();

        let interleave =
            coherent_capture_test_support::hold(page.target_id.clone(), page.session_id.clone());
        let captured = capture_coherent(&mut page, ScreenshotTarget::Viewport)
            .await
            .expect("retry to one coherent state");

        assert_eq!(page.generation(), generation, "hook navigated the page");
        assert_eq!(captured.snapshot.generation, generation);
        assert_eq!(
            interleave.attempts(),
            2,
            "the mixed A/B attempt was not rejected and retried exactly once"
        );
        assert!(
            captured.snapshot.nodes.iter().any(|node| {
                node.text.as_deref() == Some("marker-B") || node.name.as_deref() == Some("marker-B")
            }),
            "accepted snapshot did not contain marker B"
        );
        let (width, _height, pixels) = png_rgba(&captured.screenshot.bytes);
        let pixel = ((10 * width + 10) * 4) as usize;
        assert_eq!(
            &pixels[pixel..pixel + 4],
            &[0, 0, 255, 255],
            "accepted screenshot did not contain marker B's blue pixel"
        );

        let _ = launched.child.kill();
        let _ = launched.child.wait();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_create_race_has_exactly_one_immutable_winner() {
        if crate::browser::find().is_err() {
            eprintln!("SKIP real_create_race_has_exactly_one_immutable_winner");
            return;
        }

        let _slot = crate::browser::test_browser_slot();
        let repetitions = std::env::var("BROW_CHECKPOINT_RACE_REPETITIONS")
            .map(|value| {
                value
                    .parse::<usize>()
                    .expect("BROW_CHECKPOINT_RACE_REPETITIONS must be an integer")
                    .clamp(1, 20)
            })
            .unwrap_or(3);
        let scratch = tempfile::tempdir().unwrap();
        let profile = tempfile::tempdir().unwrap();
        let mut launch_options = crate::browser::LaunchOptions::new(profile.path().join("profile"));
        launch_options.headless = crate::browser::Headless::New;
        launch_options.window_size = (320, 240);
        let mut launched = crate::browser::launch(&launch_options)
            .await
            .expect("launch Chromium");
        let mut left_page = Page::create(std::sync::Arc::clone(&launched.client), "about:blank")
            .await
            .expect("create left checkpoint page");
        let mut right_page = Page::create(std::sync::Arc::clone(&launched.client), "about:blank")
            .await
            .expect("create right checkpoint page");

        for iteration in 0..repetitions {
            left_page
                .evaluate(
                    &format!(
                        "document.title = 'left-{iteration}'; \
                         document.body.innerHTML = '<main>left-{iteration}</main>'; true"
                    ),
                    false,
                )
                .await
                .expect("prepare left checkpoint page");
            right_page
                .evaluate(
                    &format!(
                        "document.title = 'right-{iteration}'; \
                         document.body.innerHTML = '<main>right-{iteration}</main>'; true"
                    ),
                    false,
                )
                .await
                .expect("prepare right checkpoint page");

            let root = scratch.path().join(format!("race-{iteration:03}"));
            let name = "shared-final";
            let final_path = root.join(name);
            let options = |session: &str| CheckpointOptions {
                session: session.into(),
                name: name.into(),
                full_page: false,
                wait: WaitPolicy::Load,
                timeout: Duration::from_secs(5),
                quiet: Duration::from_millis(50),
                park_pointer: false,
                output_root: Some(root.clone()),
            };
            let hold = publication_race_test_support::hold(final_path.clone(), 2);
            let left_create = create(&mut left_page, options("race-left"));
            let right_create = create(&mut right_page, options("race-right"));
            let release = async {
                tokio::time::timeout(Duration::from_secs(20), hold.wait_until_all_entered())
                    .await
                    .expect("both checkpoint creates must finish staging before publication");
                hold.release();
            };
            let (left_result, right_result, ()) = tokio::join!(left_create, right_create, release);
            drop(hold);

            let (winner, loser) = match (left_result, right_result) {
                (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => (winner, loser),
                (Ok(left), Ok(right)) => {
                    panic!("both no-replace publishers succeeded: left={left:?}, right={right:?}")
                }
                (Err(left), Err(right)) => {
                    panic!("neither no-replace publisher succeeded: left={left:?}, right={right:?}")
                }
            };
            assert_eq!(winner.path, final_path);
            let partial_path = match loser {
                CheckpointError::ConcurrentCollision {
                    final_path: collision_final,
                    partial_path,
                } => {
                    assert_eq!(collision_final, final_path);
                    partial_path
                }
                other => panic!("loser returned the wrong error: {other:?}"),
            };
            assert!(partial_path.is_dir(), "losing staging evidence vanished");
            for name in [
                "snapshot.json",
                "screenshot.png",
                "console-errors.json",
                "network-failures.json",
                "manifest.json",
            ] {
                assert!(
                    partial_path.join(name).is_file(),
                    "loser partial is missing {name}"
                );
            }

            let manifest_bytes = tokio::fs::read(final_path.join("manifest.json"))
                .await
                .expect("winner manifest");
            let disk_manifest: Value =
                serde_json::from_slice(&manifest_bytes).expect("valid winner manifest");
            assert_eq!(
                disk_manifest,
                serde_json::to_value(&winner.manifest).expect("serialize returned manifest"),
                "the final path does not contain the winning create's manifest"
            );
            for (name, expected) in &winner.manifest.files {
                let bytes = tokio::fs::read(final_path.join(name))
                    .await
                    .unwrap_or_else(|error| panic!("read winner file {name}: {error}"));
                let actual = evidence(&bytes);
                assert_eq!(actual.bytes, expected.bytes, "size changed for {name}");
                assert_eq!(actual.sha256, expected.sha256, "hash changed for {name}");
            }

            let mut immutable_bytes = std::collections::BTreeMap::new();
            for name in [
                "snapshot.json",
                "screenshot.png",
                "console-errors.json",
                "network-failures.json",
                "manifest.json",
            ] {
                immutable_bytes.insert(
                    name,
                    tokio::fs::read(final_path.join(name))
                        .await
                        .expect("read immutable winner file"),
                );
            }

            // A later ordinary create must fail at the precheck and leave the
            // already-selected winner byte-for-byte unchanged.
            let later = create(&mut left_page, options("race-later"))
                .await
                .expect_err("an existing final checkpoint must never be overwritten");
            assert!(
                matches!(later, CheckpointError::Collision(ref path) if path == &final_path),
                "later create returned the wrong collision: {later:?}"
            );
            for (name, before) in &immutable_bytes {
                let after = tokio::fs::read(final_path.join(name))
                    .await
                    .expect("reread immutable winner file");
                assert_eq!(&after, before, "winner file {name} was overwritten");
            }

            let mut final_entries = Vec::new();
            let mut partial_entries = Vec::new();
            let mut entries = tokio::fs::read_dir(&root).await.expect("race inventory");
            while let Some(entry) = entries.next_entry().await.expect("race entry") {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    partial_entries.push(entry.path());
                } else {
                    final_entries.push(entry.path());
                }
            }
            assert_eq!(final_entries, vec![final_path]);
            assert_eq!(partial_entries, vec![partial_path]);
        }

        let _ = launched.child.kill();
        let _ = launched.child.wait();
    }

    #[tokio::test]
    async fn concurrent_no_replace_publication_has_one_immutable_winner() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("concurrent");
        let mut first_fault = FaultPlan::none();
        let first = stage_fixture(&root, ".bundle.partial-first", &mut first_fault)
            .await
            .unwrap();
        let mut second_fault = FaultPlan::none();
        let second = stage_fixture(&root, ".bundle.partial-second", &mut second_fault)
            .await
            .unwrap();
        let final_path = root.join("bundle");
        let mut first_sync = FaultPlan::none();
        sync_directory(&first, &mut first_sync).await.unwrap();
        let mut second_sync = FaultPlan::none();
        sync_directory(&second, &mut second_sync).await.unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        let (first, first_result, second, second_result) = std::thread::scope(|scope| {
            let spawn_publisher = |partial: PathBuf| {
                let barrier = std::sync::Arc::clone(&barrier);
                let final_path = final_path.clone();
                scope.spawn(move || {
                    barrier.wait();
                    let result = rename_no_replace(&partial, &final_path);
                    (partial, result)
                })
            };
            let first_task = spawn_publisher(first);
            let second_task = spawn_publisher(second);
            barrier.wait();
            let (first, first_result) = first_task.join().unwrap();
            let (second, second_result) = second_task.join().unwrap();
            (first, first_result, second, second_result)
        });

        let results = [(&first, first_result), (&second, second_result)];
        assert_eq!(
            results.iter().filter(|(_, result)| result.is_ok()).count(),
            1,
            "exactly one publisher must win"
        );
        let loser = results
            .iter()
            .find(|(_, result)| result.is_err())
            .expect("one publisher must lose");
        assert_eq!(
            loser.1.as_ref().unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert!(loser.0.is_dir(), "losing partial evidence was removed");
        assert!(final_path.join("manifest.json").is_file());
        let before = std::fs::read(final_path.join("manifest.json")).unwrap();
        assert_eq!(before, b"manifest.json");
    }

    #[tokio::test]
    async fn no_replace_publication_preserves_an_empty_sentinel_directory() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("sentinel");
        let mut stage_fault = FaultPlan::none();
        let partial = stage_fixture(&root, ".bundle.partial", &mut stage_fault)
            .await
            .unwrap();
        let final_path = root.join("bundle");
        tokio::fs::create_dir(&final_path).await.unwrap();
        let mut fault = FaultPlan::none();

        let error = publish_partial(&partial, &final_path, &root, &mut fault)
            .await
            .expect_err("an empty final directory must not be replaced");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(partial.join("manifest.json").is_file());
        assert_eq!(std::fs::read_dir(&final_path).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn rename_and_parent_sync_complete_in_one_cancellation_poll() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("atomic-publication");
        let mut stage_fault = FaultPlan::none();
        let partial = stage_fixture(&root, ".bundle.partial", &mut stage_fault)
            .await
            .unwrap();
        let mut partial_sync = FaultPlan::none();
        sync_directory(&partial, &mut partial_sync).await.unwrap();
        let final_path = root.join("bundle");
        let mut publication_fault = FaultPlan::none();

        let durability = tokio::time::timeout(std::time::Duration::ZERO, async {
            publish_rename_and_sync(&partial, &final_path, &root, &mut publication_fault)
        })
        .await
        .expect("rename plus parent fsync must complete in the first poll")
        .expect("publish staged bundle");

        assert_eq!(durability, CheckpointDurability::Durable);
        assert!(final_path.join("manifest.json").is_file());
        assert!(!partial.exists());
    }

    #[tokio::test]
    async fn every_publication_io_boundary_has_a_typed_recoverable_outcome() {
        let base = std::env::temp_dir().join(format!(
            "brow-checkpoint-faults-{}-{}",
            std::process::id(),
            PARTIAL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));

        let success = base.join("success");
        let mut discovery = FaultPlan::none();
        let durability = publish_fixture(&success, &mut discovery)
            .await
            .expect("discover fault boundary count");
        assert_eq!(durability, CheckpointDurability::Durable);
        assert!(success.join("bundle/manifest.json").is_file());
        assert!(!success.join(".bundle.partial").exists());
        let boundary_count = discovery.seen;
        assert!(
            boundary_count >= 23,
            "too few IO boundaries: {boundary_count}"
        );

        let mut sync_unknown = 0;
        for index in 0..boundary_count {
            let root = base.join(format!("fault-{index}"));
            let mut fault = FaultPlan::at(index);
            match publish_fixture(&root, &mut fault).await {
                Err(error) => {
                    assert!(error.to_string().contains("injected checkpoint failure"));
                    assert!(
                        !root.join("bundle").exists(),
                        "prepublication fault {index} published a final bundle"
                    );
                    assert!(
                        root.join(".bundle.partial").is_dir(),
                        "prepublication fault {index} lost the partial evidence path"
                    );
                }
                Ok(CheckpointDurability::PublishedSyncUnknown { error }) => {
                    sync_unknown += 1;
                    assert!(error.contains("injected checkpoint failure"));
                    assert!(
                        root.join("bundle/manifest.json").is_file(),
                        "postpublication fault {index} lost the authoritative final bundle"
                    );
                    assert!(
                        !root.join(".bundle.partial").exists(),
                        "postpublication fault {index} left a duplicate staging path"
                    );
                }
                Ok(CheckpointDurability::Durable) => {
                    panic!("injected boundary {index} was not observed")
                }
            }
        }
        #[cfg(unix)]
        assert_eq!(
            sync_unknown, 2,
            "directory open and sync failures after rename must be typed"
        );

        std::fs::remove_dir_all(&base).expect("remove owned checkpoint fault scratch");
    }
}
