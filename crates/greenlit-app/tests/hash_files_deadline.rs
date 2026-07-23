use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use greenlit_expr::{
    Context, DirEntry, EntryKind, EvalError, HashFilesClock, HashFilesError, HashFilesFs,
    OpenDirectory, OpenedDir, evaluate, parse,
};

const BLOCK_DURATION: Duration = Duration::from_secs(3);

/// Adapts a fake's flat, path-addressed enumeration into the capability
/// boundary `HashFilesFs` now requires. These fakes exist to record
/// deadline/streaming behavior at the true I/O boundary, not the
/// held-parent-descriptor discipline `RealFs` itself must uphold (that is
/// pinned separately, against a real filesystem, in
/// `hash_files_invariants.rs`), so re-resolving a child by its full lexical
/// path against the same fake is a faithful, simple stand-in here.
struct SimpleOpenDir<'a> {
    fs: &'a dyn HashFilesFs,
    entries: Box<dyn Iterator<Item = io::Result<DirEntry>> + 'a>,
}

impl<'a> OpenDirectory<'a> for SimpleOpenDir<'a> {
    fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        self.entries.next().transpose()
    }

    fn open_child_dir(&self, _name: &str, path: &Path, _follow: bool) -> io::Result<OpenedDir<'a>> {
        self.fs.open_dir(path)
    }

    fn hash_child_file(
        &self,
        _name: &str,
        path: &Path,
        _follow: bool,
        check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        self.fs.hash_file_sha256(path, check_timeout)
    }
}

#[derive(Debug)]
struct AcceleratingClock {
    expired: Arc<AtomicBool>,
}

impl HashFilesClock for AcceleratingClock {
    fn elapsed(&self, _started: Instant) -> Duration {
        if self.expired.load(Ordering::Acquire) {
            Duration::from_secs(120)
        } else {
            Duration::ZERO
        }
    }
}

#[derive(Debug)]
struct BlockingDirectoryFs {
    root: PathBuf,
    blocked: Arc<AtomicBool>,
}

impl HashFilesFs for BlockingDirectoryFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        let blocked = Arc::clone(&self.blocked);
        let mut first = true;
        let entries: Box<dyn Iterator<Item = io::Result<DirEntry>>> =
            Box::new(std::iter::from_fn(move || {
                if !first {
                    return None;
                }
                first = false;
                blocked.store(true, Ordering::Release);
                thread::sleep(BLOCK_DURATION);
                Some(Ok(DirEntry {
                    name: "blocked-entry".to_owned(),
                    kind: EntryKind::Other,
                }))
            }));
        Ok(OpenedDir::new(
            path.to_path_buf(),
            Box::new(SimpleOpenDir { fs: self, entries }),
        ))
    }

    fn entry_kind(&self, _path: &Path) -> io::Result<EntryKind> {
        Ok(EntryKind::Dir)
    }

    fn hash_file_sha256(
        &self,
        _path: &Path,
        _check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        Err(io::Error::other("blocked directory must not become a file"))
    }
}

#[derive(Debug)]
struct RecordingDirectoryFs {
    root: PathBuf,
    expired: Arc<AtomicBool>,
    pulls: Arc<AtomicUsize>,
}

impl HashFilesFs for RecordingDirectoryFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn open_dir(&self, path: &Path) -> io::Result<OpenedDir<'_>> {
        let expired = Arc::clone(&self.expired);
        let pulls = Arc::clone(&self.pulls);
        let entries: Box<dyn Iterator<Item = io::Result<DirEntry>>> =
            Box::new(std::iter::from_fn(move || {
                match pulls.fetch_add(1, Ordering::AcqRel) {
                    0 => {
                        expired.store(true, Ordering::Release);
                        Some(Ok(DirEntry {
                            name: "first-entry".to_owned(),
                            kind: EntryKind::Other,
                        }))
                    }
                    1 => Some(Err(io::Error::other(
                        "directory stream was polled after its fixed deadline",
                    ))),
                    _ => None,
                }
            }));
        Ok(OpenedDir::new(
            path.to_path_buf(),
            Box::new(SimpleOpenDir { fs: self, entries }),
        ))
    }

    fn entry_kind(&self, _path: &Path) -> io::Result<EntryKind> {
        Ok(EntryKind::Dir)
    }

    fn hash_file_sha256(
        &self,
        _path: &Path,
        _check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        Err(io::Error::other(
            "recorded directory must not become a file",
        ))
    }
}

/// Blocks briefly (real wall-clock time, not the injected clock) inside
/// `open_dir`, marking `blocked` right before sleeping — the same shape as
/// `BlockingDirectoryFs` but with a caller-chosen short duration, so a test
/// can drive several stranding calls without waiting seconds each.
#[derive(Debug)]
struct ShortBlockFs {
    root: PathBuf,
    block_for: Duration,
    blocked: Arc<AtomicBool>,
}

impl HashFilesFs for ShortBlockFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn open_dir(&self, _path: &Path) -> io::Result<OpenedDir<'_>> {
        self.blocked.store(true, Ordering::Release);
        thread::sleep(self.block_for);
        Err(io::Error::other(
            "short-block fs never yields a real directory",
        ))
    }

    fn entry_kind(&self, _path: &Path) -> io::Result<EntryKind> {
        Ok(EntryKind::Dir)
    }

    fn hash_file_sha256(
        &self,
        _path: &Path,
        _check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        Err(io::Error::other("short-block fs never hashes a file"))
    }
}

#[test]
fn public_hash_files_deadline_interrupts_blocked_io_and_stops_expired_streams() {
    // Invariant class: a repository-controlled host filesystem operation may
    // block past the runner's fixed child-process deadline. The evaluator's
    // caller-side supervisor must return independently while its detached
    // worker remains asleep. The injected clock can only accelerate the same
    // 120-second boundary because production also measures monotonic time.
    let blocked = Arc::new(AtomicBool::new(false));
    let context = Context::new(Arc::new(BlockingDirectoryFs {
        root: PathBuf::from("/workspace"),
        blocked: Arc::clone(&blocked),
    }))
    .with_hash_files_clock(Arc::new(AcceleratingClock {
        expired: Arc::clone(&blocked),
    }));
    let expression = parse("hashFiles('it''s/**')").expect("parse blocking hashFiles expression");
    let started = Instant::now();
    let error = evaluate(&expression, &context).expect_err("blocked hashFiles must time out");
    let elapsed = started.elapsed();

    assert!(blocked.load(Ordering::Acquire));
    assert!(
        elapsed < Duration::from_secs(2),
        "caller waited {elapsed:?} for a {BLOCK_DURATION:?} filesystem block"
    );
    assert!(
        matches!(
            &error,
            EvalError::HashFiles(HashFilesError::Timeout { patterns, seconds })
                if patterns == "it''s/**" && *seconds == 120
        ),
        "unexpected blocking-boundary error: {error:?}"
    );
    assert_eq!(
        error.to_string(),
        "hashFiles('it''s/**') couldn't finish within 120 seconds."
    );

    // Directory streaming is a named host-filesystem invariant. Record at
    // that external boundary that the first completed pull can expire the
    // deadline and that traversal never requests the second entry afterward.
    let expired = Arc::new(AtomicBool::new(false));
    let pulls = Arc::new(AtomicUsize::new(0));
    let context = Context::new(Arc::new(RecordingDirectoryFs {
        root: PathBuf::from("/workspace"),
        expired: Arc::clone(&expired),
        pulls: Arc::clone(&pulls),
    }))
    .with_hash_files_clock(Arc::new(AcceleratingClock { expired }));
    let expression = parse("hashFiles('**/*')").expect("parse streaming hashFiles expression");
    let error = evaluate(&expression, &context).expect_err("streaming hashFiles must time out");

    assert!(matches!(
        error,
        EvalError::HashFiles(HashFilesError::Timeout { seconds: 120, .. })
    ));
    assert_eq!(
        pulls.load(Ordering::Acquire),
        1,
        "directory iterator was polled again after deadline expiry"
    );

    // Finding 3: a worker stuck in an uninterruptible host syscall can
    // never be killed from within this library, so the number of
    // simultaneously live workers is bounded. Each call below is backed by
    // a worker that blocks for a short real duration while an accelerated
    // clock makes the caller give up almost immediately, leaving that
    // worker stranded (still running, but abandoned) until its sleep
    // elapses. Enough of these in a row must eventually hit the fixed cap
    // and fail fast rather than starting yet another one; once every
    // stranded worker has actually finished, the cap must recover. The loop
    // bound is generous (well beyond any plausible cap value) so this stays
    // robust to the exact fixed cap rather than encoding it twice.
    const STRAND_BLOCK: Duration = Duration::from_millis(250);
    let mut saw_ordinary_timeout = false;
    let mut saw_stranded_limit = false;
    for _ in 0..40 {
        let blocked = Arc::new(AtomicBool::new(false));
        let context = Context::new(Arc::new(ShortBlockFs {
            root: PathBuf::from("/workspace"),
            block_for: STRAND_BLOCK,
            blocked: Arc::clone(&blocked),
        }))
        .with_hash_files_clock(Arc::new(AcceleratingClock {
            expired: Arc::clone(&blocked),
        }));
        let expression = parse("hashFiles('**')").expect("parse strand-cap hashFiles expression");
        match evaluate(&expression, &context) {
            Err(EvalError::HashFiles(HashFilesError::Timeout { .. })) => {
                saw_ordinary_timeout = true
            }
            Err(EvalError::HashFiles(HashFilesError::StrandedWorkerLimit { max })) => {
                saw_stranded_limit = true;
                assert!(max > 0, "the stranded-worker cap must be a positive bound");
                break;
            }
            other => panic!("unexpected strand-cap result: {other:?}"),
        }
    }
    assert!(
        saw_ordinary_timeout,
        "the cap must have headroom: an early call must not fail immediately"
    );
    assert!(
        saw_stranded_limit,
        "enough concurrently stranded workers must eventually fail fast at the fixed cap"
    );

    // Every worker above finishes its short sleep well within this wait;
    // once they have all exited, the cap must no longer be engaged.
    thread::sleep(STRAND_BLOCK * 4);
    let recovered_blocked = Arc::new(AtomicBool::new(false));
    let context = Context::new(Arc::new(ShortBlockFs {
        root: PathBuf::from("/workspace"),
        block_for: STRAND_BLOCK,
        blocked: Arc::clone(&recovered_blocked),
    }))
    .with_hash_files_clock(Arc::new(AcceleratingClock {
        expired: recovered_blocked,
    }));
    let expression = parse("hashFiles('**')").expect("parse recovered strand-cap expression");
    assert!(
        matches!(
            evaluate(&expression, &context),
            Err(EvalError::HashFiles(HashFilesError::Timeout { .. }))
        ),
        "the stranded-worker cap must recover once every prior worker has exited"
    );
    // Let the recovery call's own worker actually finish (it slept
    // `STRAND_BLOCK`) so the live-worker counter is genuinely back to zero
    // before the next block reasons about it from scratch.
    thread::sleep(STRAND_BLOCK * 2);

    // Finding 3's actual race: the earlier sequential loop above only ever
    // has one call in flight at a time, so it cannot tell a real per-spawn
    // reservation from one that merely increments once a call is later
    // detected as stranded — both look identical when calls never overlap.
    // This drives genuinely concurrent call starts (real OS threads, all
    // spawned before any of them can finish) against a fake that blocks
    // long enough for spawn/scheduling jitter to be negligible next to it,
    // and records the peak number of workers the fake ever saw running at
    // once. The fixed cap must hold under that real concurrency: no more
    // workers are ever concurrently live than were admitted, and every call
    // beyond whatever the cap turns out to be must fail fast with the
    // corrective `StrandedWorkerLimit` message instead of also starting a
    // worker.
    const CONCURRENT_BLOCK: Duration = Duration::from_millis(500);
    const CONCURRENT_STARTS: usize = 20;
    let live = Arc::new(AtomicUsize::new(0));
    let peak_live = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..CONCURRENT_STARTS)
        .map(|_| {
            let live = Arc::clone(&live);
            let peak_live = Arc::clone(&peak_live);
            // `Context` is not `Send` (it can hold `Rc`-based `Value`
            // payloads), so it must be built *inside* the spawned thread
            // rather than moved into it; only the plain, `Send + Sync`
            // fake filesystem state crosses the spawn boundary. `evaluate`'s
            // `Ok` case is likewise not `Send`, so the outcome is classified
            // to an owned, `Send` summary before this closure returns.
            thread::spawn(move || {
                let context = Context::new(Arc::new(ConcurrentBlockFs {
                    root: PathBuf::from("/workspace"),
                    block_for: CONCURRENT_BLOCK,
                    live,
                    peak_live,
                }));
                let expression =
                    parse("hashFiles('**')").expect("parse concurrent-start hashFiles expression");
                match evaluate(&expression, &context) {
                    Err(EvalError::HashFiles(HashFilesError::Io { .. })) => {
                        ConcurrentOutcome::Admitted
                    }
                    Err(EvalError::HashFiles(HashFilesError::StrandedWorkerLimit { max })) => {
                        ConcurrentOutcome::Rejected(max)
                    }
                    other => ConcurrentOutcome::Other(format!("{other:?}")),
                }
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("concurrent-start worker thread panicked")
        })
        .collect();

    let admitted = results
        .iter()
        .filter(|result| matches!(result, ConcurrentOutcome::Admitted))
        .count();
    let rejected: Vec<_> = results
        .iter()
        .filter_map(|result| match result {
            ConcurrentOutcome::Rejected(max) => Some(*max),
            _ => None,
        })
        .collect();
    let unexpected: Vec<&str> = results
        .iter()
        .filter_map(|result| match result {
            ConcurrentOutcome::Other(message) => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected concurrent-start outcome(s): {unexpected:?}"
    );
    assert_eq!(
        admitted + rejected.len(),
        CONCURRENT_STARTS,
        "every concurrent call must either run its worker or fail fast at the cap: {results:?}"
    );
    assert!(
        !rejected.is_empty(),
        "more concurrent starts than the live-worker cap must produce at least one fast failure"
    );
    assert!(
        admitted > 0,
        "the cap must have headroom: at least one concurrent call must be admitted"
    );
    let cap = rejected[0];
    assert!(
        rejected.iter().all(|max| *max == cap),
        "the fixed cap must be reported identically by every rejection: {rejected:?}"
    );
    let peak = peak_live.load(Ordering::Acquire);
    assert!(
        peak <= cap,
        "the cap must hold: at most {cap} workers may ever be concurrently live, saw {peak}"
    );
    assert!(
        admitted <= cap,
        "no more calls than the fixed cap can ever be admitted at once: admitted {admitted}, cap {cap}"
    );
    let rendered = HashFilesError::StrandedWorkerLimit { max: cap }.to_string();
    assert!(
        rendered.contains("wait for concurrent evaluations to finish"),
        "strand-cap error lacked its corrective action: {rendered}"
    );
}

/// An owned, `Send` classification of one concurrent-start call's outcome
/// (`evaluate`'s own `Result<Value, EvalError>` is not `Send`, since `Value`
/// can hold `Rc`-based array/object payloads).
#[derive(Debug)]
enum ConcurrentOutcome {
    /// The call's worker actually ran (and, for this fake, then failed with
    /// a plain I/O error once its block elapsed).
    Admitted,
    /// The call was refused before starting a worker because the live cap
    /// was already reserved.
    Rejected(usize),
    /// Anything else is a test-setup bug, not a case this test means to
    /// classify.
    Other(String),
}

/// Records the number of `hashFiles` filesystem workers concurrently
/// blocked inside `open_dir` at once (`peak_live`), to prove the live-worker
/// cap is a real per-spawn reservation rather than something only detected
/// after the fact. Each call sleeps for a fixed real duration far longer
/// than thread-spawn/scheduling jitter, so calls started at (approximately)
/// the same instant are guaranteed to overlap for the whole test.
#[derive(Debug)]
struct ConcurrentBlockFs {
    root: PathBuf,
    block_for: Duration,
    live: Arc<AtomicUsize>,
    peak_live: Arc<AtomicUsize>,
}

impl HashFilesFs for ConcurrentBlockFs {
    fn workspace_root(&self) -> &Path {
        &self.root
    }

    fn open_dir(&self, _path: &Path) -> io::Result<OpenedDir<'_>> {
        let now_live = self.live.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak_live.fetch_max(now_live, Ordering::AcqRel);
        thread::sleep(self.block_for);
        self.live.fetch_sub(1, Ordering::AcqRel);
        Err(io::Error::other(
            "concurrent-block fs never yields a real directory",
        ))
    }

    fn entry_kind(&self, _path: &Path) -> io::Result<EntryKind> {
        Ok(EntryKind::Dir)
    }

    fn hash_file_sha256(
        &self,
        _path: &Path,
        _check_timeout: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<[u8; 32]> {
        Err(io::Error::other("concurrent-block fs never hashes a file"))
    }
}
