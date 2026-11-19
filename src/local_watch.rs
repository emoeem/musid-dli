//! M5 local watcher event stream abstraction (docs/integration/
//! LOCAL_ENHANCEMENT_DESIGN.md §5.2, M5 slice s9, parity L7).
//!
//! voicefox couples its watcher directly to the library: `notify`'s
//! `recommended_watcher` feeds an internal thread that coalesces bursts and
//! then calls back into `LocalSource` to trigger a forced rescan and bump
//! `watch_generation` (voicefox `mod.rs:528-616`). This module keeps the
//! detection half and hands the REACTION half to the host:
//!
//! * notify's raw events are mapped onto the library-owned
//!   [`WatchEvent`] (`{path, kind}` with
//!   `Created/Modified/Removed/Renamed/Other`);
//! * delivery is the host's choice: a `FnMut(WatchEvent)` callback
//!   ([`WatchSink::Callback`]) or a `std::sync::mpsc` sender
//!   ([`WatchSink::Channel`]; [`watch_recursive_channel`] builds the pair).
//!   Zero tokio/runtime dependency is a hard boundary: this module uses only
//!   `std::thread` + `std::sync::mpsc`, and notify's own default-features
//!   channel is `std::sync::mpsc` as well (notify 8.2 `lib.rs:170-183`; the
//!   `crossbeam-channel`/`flume` features stay off);
//! * non-mutating access events are filtered before mapping — the
//!   `Access(Close(Read))/Open/Read` forms are ported verbatim from voicefox
//!   `is_non_mutating_access` (`mod.rs:617-627`) so a listener reading a
//!   watched file cannot self-excite. Linux note (evidence, notify 8.2.0
//!   `src/inotify.rs:427-434`): the inotify watch mask covers
//!   `ATTRIB|CREATE|OPEN|DELETE|CLOSE_WRITE|MODIFY|MOVED_FROM|MOVED_TO` and
//!   NOT `ACCESS`/`CLOSE_NOWRITE`, so on Linux the only observable member of
//!   the filtered set is `Access(Open(_))` (opening a watched directory, e.g.
//!   a `read_dir`); `Close(Read)`/`Read` still matter on the other platforms
//!   the filter is written for, and the pure function is pinned by tests for
//!   all three forms regardless of platform;
//! * bursts are coalesced by a quiet-window debounce: delivery happens after
//!   the event stream stays quiet for one window (default 250 ms,
//!   [`MIN_DEBOUNCE`], voicefox `mod.rs:558`; smaller configured values are
//!   clamped up to the floor). Inside one window a path's FIRST event kind
//!   wins and delivery follows first-seen order — a create-then-edit burst
//!   surfaces as `Created`, a pure rewrite as `Modified`, and notify's
//!   trailing access events (Linux emits `Access(Close(Write))` after every
//!   file write, notify `src/inotify.rs:325-331`) are absorbed instead of
//!   masking the informative kind. A batch also flushes when it reaches
//!   [`COALESCE_MAX_PATHS`] distinct paths or 8 windows of age, so a
//!   continuously churning stream (a bulk copy) cannot starve delivery
//!   forever or grow the batch without bound; a path whose raw events
//!   straddle a cap flush may legitimately surface in two consecutive
//!   batches (one event per window). Two inotify-observable details worth
//!   knowing: creating or removing an entry moves the watched directory's
//!   own mtime, which the dir watch reports as IN_ATTRIB — i.e. a
//!   `Modified` event for the root path alongside the entry's own event —
//!   and that root event is real change evidence too. This is a deliberate
//!   strengthening of upstream, which coalesces only the already-queued
//!   backlog and re-triggers per event (`mod.rs:575-577`);
//! * the returned [`StopHandle`] stops and joins the internal thread on Drop
//!   (voicefox `WatcherHandle` semantics, `mod.rs:61-75`): Drop returns only
//!   after the thread exited and released its sink, so on the channel face
//!   the `Receiver` deterministically observes `Disconnected` after the drop
//!   — the observable "watcher fully stopped" signal; events delivered
//!   before the drop remain in the channel buffer and drain first;
//!
//! **What the events mean** (`ScanOptions::force` contract): a `WatchEvent`
//! is positive evidence that something changed, but directory mtimes do not
//! reflect in-place edits and the s4 root directory-signature fast path only
//! proves the root's direct entries are unchanged. A host that reacts to a
//! [`WatchEvent`] by rescanning MUST set `ScanOptions::force = true` to
//! bypass that fast path (voicefox `mod.rs:592-596`; documented in
//! `ScanOptions::force`, LOCAL_ENHANCEMENT_DESIGN.md §7). Undelivered events
//! pending inside a debounce window are discarded on Drop, so a host needing
//! final-state certainty after stopping the watcher does one final force
//! rescan rather than draining the stream.
//!
//! **Out of scope by design (§9)**: no library-side automatic rescan, no
//! `watch_generation`, no thread holding library state — the handle carries
//! only a stop flag, a wake sender and a join handle, exactly like the
//! upstream handle but without the `Arc<LocalSource>` weak reference. The
//! host owns the scan state and the reaction.
//!
//! Feature economics: notify pulls the platform FFI backends (inotify on
//! Linux, ReadDirectoryChangesW/`windows-sys` on Windows, FSEvents/`fsevent-sys`
//! on macOS) plus its own event thread. Hosts that only batch-scan should not
//! pay for that, so the dependency lives behind `local-watch` — the same
//! separation rationale as `ncm` and `local-cover`
//! (docs/DEPENDENCY_AUDIT.md, 2026-11-20 entry).
//!
//! Delivery contract details:
//!
//! * callbacks run on the internal debouncer thread (not the calling
//!   thread); like [`crate::local_scan::ScanControl`]'s progress callback,
//!   the callback must not re-enter the scan core, and a panicking callback
//!   terminates the watcher thread — no further events are delivered while
//!   [`StopHandle::drop`] still joins cleanly;
//! * a [`WatchSink::Channel`] sender failing (`Receiver` dropped) stops the
//!   watcher: nobody is listening, so the OS watch is released with the
//!   thread;
//! * notify backend errors (e.g. the inotify watch-limit error) surface as
//!   one `Other` event for the error's first path (or an empty path when the
//!   error carries none) instead of being dropped: the host loses change
//!   evidence behind such an error and should treat it as a rescan
//!   trigger, the same reaction as any other event;
//! * roots are watched with `RecursiveMode::Recursive` when they are
//!   directories; non-directory roots are silently skipped exactly like the
//!   upstream watch loop (`mod.rs:545-555`) — pass the containing directory.
//!   Event paths derive from the root as given, so hosts should watch the
//!   same canonical root form they scan with (`ScanState` signature keys are
//!   canonicalized engine-side, §7).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind};
use notify::{RecursiveMode, Watcher, recommended_watcher};

/// Debounce window floor: configured values below this are clamped up
/// (voicefox `mod.rs:558`, `interval.max(Duration::from_millis(250))`).
pub const MIN_DEBOUNCE: Duration = Duration::from_millis(250);

/// Batch size cap: a debounce window flushes early once this many distinct
/// paths are pending, bounding the batch under sustained churn (see the
/// module docs on burst coalescing).
pub const COALESCE_MAX_PATHS: usize = 256;

/// A mapped filesystem event: one per raw event path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatchEvent {
    /// Path the platform backend reported. Derives from the watch root as
    /// given (see the module docs on watching the canonical scan root).
    pub path: PathBuf,
    /// Mapped event kind; inside one debounce window the first kind per path
    /// wins (the module docs on burst coalescing).
    pub kind: WatchEventKind,
}

/// Mapped change kind (voicefox passes any event to the same forced rescan,
/// `mod.rs:592-596`; the split is musid-only informational detail).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchEventKind {
    /// `notify::EventKind::Create(_)` (inotify IN_CREATE, file or dir).
    Created,
    /// `notify::EventKind::Modify(_)` other than a rename (inotify IN_MODIFY
    /// for data, IN_ATTRIB for metadata changes).
    Modified,
    /// `notify::EventKind::Remove(_)` (inotify IN_DELETE / DELETE_SELF).
    Removed,
    /// `notify::EventKind::Modify(ModifyKind::Name(_))` (inotify
    /// IN_MOVED_FROM / IN_MOVED_TO; a rename surfaces as one event for the
    /// old path and one for the new path).
    Renamed,
    /// Access kinds that survived the non-mutating filter (e.g. Linux
    /// `Access(Close(Write))` after every file write), notify's `Other` and
    /// backend error evidence. Still change evidence: react the same way.
    Other,
}

/// Watcher configuration. `Default` matches the upstream 250 ms floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchOptions {
    /// Quiet-window length for burst coalescing. Values below
    /// [`MIN_DEBOUNCE`] are clamped up to the floor.
    pub debounce: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            debounce: MIN_DEBOUNCE,
        }
    }
}

/// The delivery face, the host's choice (design §5.2): callback or channel.
pub enum WatchSink {
    /// Delivered by calling the closure with every event. Runs on the
    /// internal debouncer thread; must not re-enter the scan core, and a
    /// panic terminates the watcher thread (see the module docs).
    Callback(Box<dyn FnMut(WatchEvent) + Send + 'static>),
    /// Delivered by `send`ing every event; a dropped `Receiver` stops the
    /// watcher (nobody is listening any more).
    Channel(mpsc::Sender<WatchEvent>),
}

/// Effective debounce window: the configured value clamped to
/// [`MIN_DEBOUNCE`] (voicefox `mod.rs:558`).
fn effective_debounce(options: &WatchOptions) -> Duration {
    options.debounce.max(MIN_DEBOUNCE)
}

/// Drops the OS watch and joins the internal thread on Drop
/// (voicefox `WatcherHandle`, `mod.rs:61-75`). An inert handle from
/// [`watch_recursive`] with no roots is a no-op on Drop.
pub struct StopHandle {
    stop: Option<Arc<AtomicBool>>,
    wake: Option<mpsc::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Drop for StopHandle {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::SeqCst);
        }
        if let Some(wake) = &self.wake {
            let _ = wake.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Starts watching the given directory roots recursively and delivers
/// mapped [`WatchEvent`]s to `sink` after burst-coalescing quiet windows.
///
/// `roots` that are not directories are silently skipped (upstream watch
/// loop, voicefox `mod.rs:545-555`); empty roots yield an inert
/// [`StopHandle`] without spawning any thread. Backend failures while
/// creating the watcher or adding a watch return `Err` with the reason.
/// See the module docs for the delivery contract, the `force` rescan
/// contract and the out-of-scope boundary.
pub fn watch_recursive(
    roots: &[PathBuf],
    options: WatchOptions,
    sink: WatchSink,
) -> Result<StopHandle, String> {
    if roots.is_empty() {
        return Ok(StopHandle {
            stop: None,
            wake: None,
            join: None,
        });
    }
    watch_roots(roots, options, sink)
}

/// Channel-delivery convenience for [`watch_recursive`]: builds the
/// `std::sync::mpsc` pair for the host and hands back the receiver plus the
/// [`StopHandle`] (design §5.2's "std::sync::mpsc" face).
pub fn watch_recursive_channel(
    roots: &[PathBuf],
    options: WatchOptions,
) -> Result<(mpsc::Receiver<WatchEvent>, StopHandle), String> {
    let (tx, rx) = mpsc::channel();
    let handle = watch_recursive(roots, options, WatchSink::Channel(tx))?;
    Ok((rx, handle))
}

fn watch_roots(
    roots: &[PathBuf],
    options: WatchOptions,
    sink: WatchSink,
) -> Result<StopHandle, String> {
    let debounce = effective_debounce(&options);
    // Internal channel from notify's dispatch thread into the debouncer
    // thread only; the host-facing delivery goes through `sink`.
    let (ev_tx, ev_rx) = mpsc::channel::<WatchEvent>();
    let forward_tx = ev_tx.clone();
    let mut fs_watcher = recommended_watcher(move |event: notify::Result<notify::Event>| {
        for watch_event in event_to_watch_events(event) {
            let _ = forward_tx.send(watch_event);
        }
    })
    .map_err(|error| format!("failed to create the file watcher: {error}"))?;
    for path in roots {
        if path.is_dir() {
            fs_watcher
                .watch(path, RecursiveMode::Recursive)
                .map_err(|error| format!("failed to watch {}: {error}", path.display()))?;
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    let join = spawn_watcher_thread(
        Arc::clone(&stop),
        debounce,
        ev_rx,
        wake_rx,
        sink,
        fs_watcher,
    );
    Ok(StopHandle {
        stop: Some(stop),
        wake: Some(wake_tx),
        join: Some(join),
    })
}

/// Non-mutating access filter, ported verbatim from voicefox
/// `is_non_mutating_access` (`mod.rs:617-627`).
fn is_non_mutating_access(event: &notify::Event) -> bool {
    matches!(
        event.kind,
        EventKind::Access(AccessKind::Close(AccessMode::Read))
            | EventKind::Access(AccessKind::Open(_))
            | EventKind::Access(AccessKind::Read)
    )
}

/// Maps one raw notify event onto zero or more [`WatchEvent`]s: filtered
/// access events map to nothing, an `Ok` event maps to one event per path
/// (no paths = nothing to report), and a backend error maps to one `Other`
/// evidence event (see the module docs).
fn event_to_watch_events(event: notify::Result<notify::Event>) -> Vec<WatchEvent> {
    match event {
        Ok(raw) => {
            if is_non_mutating_access(&raw) {
                return Vec::new();
            }
            let kind = map_event_kind(&raw.kind);
            raw.paths
                .iter()
                .map(|path| WatchEvent {
                    path: path.clone(),
                    kind,
                })
                .collect()
        }
        Err(error) => {
            let path = error.paths.first().cloned().unwrap_or_default();
            vec![WatchEvent {
                path,
                kind: WatchEventKind::Other,
            }]
        }
    }
}

/// Event-kind mapping (see the module docs; the rename arm must precede the
/// general `Modify` arm).
fn map_event_kind(kind: &EventKind) -> WatchEventKind {
    match kind {
        EventKind::Create(_) => WatchEventKind::Created,
        EventKind::Modify(ModifyKind::Name(_)) => WatchEventKind::Renamed,
        EventKind::Modify(_) => WatchEventKind::Modified,
        EventKind::Remove(_) => WatchEventKind::Removed,
        EventKind::Access(_) | EventKind::Other | EventKind::Any => WatchEventKind::Other,
    }
}

/// First-wins coalescing push: later events for a path already in the batch
/// are absorbed (see the module docs on burst coalescing).
fn coalesce_push(
    pending: &mut Vec<WatchEvent>,
    seen: &mut HashMap<PathBuf, usize>,
    event: WatchEvent,
) {
    if seen.contains_key(&event.path) {
        return;
    }
    seen.insert(event.path.clone(), pending.len());
    pending.push(event);
}

/// Delivers one coalesced batch; `false` means the channel receiver was
/// dropped and the watcher should stop.
fn deliver_batch(sink: &mut WatchSink, batch: Vec<WatchEvent>) -> bool {
    for event in batch {
        match sink {
            WatchSink::Callback(callback) => callback(event),
            WatchSink::Channel(tx) => {
                if tx.send(event).is_err() {
                    return false;
                }
            }
        }
    }
    true
}

// ───────────────────────── Watcher thread ─────────────────────────

/// The debouncer thread body, factored out so the spawn helper stays
/// readable. See the module docs for the loop contract: idle wait → absorb a
/// burst until one quiet window (or the batch caps) → deliver; stop discards
/// pending events.
fn watcher_loop(
    stop: &AtomicBool,
    debounce: Duration,
    ev_rx: mpsc::Receiver<WatchEvent>,
    wake_rx: mpsc::Receiver<()>,
    sink: &mut WatchSink,
) {
    let max_batch_age = debounce * 8;
    'run: loop {
        if stop.load(Ordering::Acquire) || wake_rx.try_recv().is_ok() {
            break 'run;
        }
        // Idle: wait for the first event of a burst. The stop flag and the
        // wake sender are re-checked at the loop head after every timeout.
        let first = match ev_rx.recv_timeout(debounce) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break 'run,
        };
        let mut pending = Vec::new();
        let mut seen: HashMap<PathBuf, usize> = HashMap::new();
        let batch_started = Instant::now();
        coalesce_push(&mut pending, &mut seen, first);
        // Absorb the rest of the burst: deliver once the stream is quiet for
        // one debounce window, the batch hits the path cap, or the batch age
        // cap elapses under sustained churn.
        loop {
            if stop.load(Ordering::Acquire) || wake_rx.try_recv().is_ok() {
                // Drop semantics: undelivered pending events are discarded.
                break 'run;
            }
            let remaining = batch_started
                .checked_add(max_batch_age)
                .and_then(|deadline| deadline.checked_duration_since(Instant::now()));
            if remaining.is_none() {
                break; // batch age cap reached
            }
            // Wait at most one window, but never past the batch deadline: a
            // timeout therefore means either a full quiet window or the
            // deadline, both of which flush the batch.
            match ev_rx.recv_timeout(remaining.unwrap().min(debounce)) {
                Ok(event) => {
                    coalesce_push(&mut pending, &mut seen, event);
                    if pending.len() >= COALESCE_MAX_PATHS {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break 'run,
            }
        }
        if !deliver_batch(sink, pending) {
            break 'run;
        }
    }
}

/// Spawns the debouncer thread; `fs_watcher` is moved in so the OS watch
/// lives exactly as long as the thread.
fn spawn_watcher_thread(
    stop: Arc<AtomicBool>,
    debounce: Duration,
    ev_rx: mpsc::Receiver<WatchEvent>,
    wake_rx: mpsc::Receiver<()>,
    mut sink: WatchSink,
    fs_watcher: notify::RecommendedWatcher,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("musid-local-watch".into())
        .spawn(move || {
            watcher_loop(&stop, debounce, ev_rx, wake_rx, &mut sink);
            // Dropping the watcher removes the OS-level watch and disconnects
            // the internal channel.
            drop(fs_watcher);
        })
        .expect("failed to spawn the musid-local-watch thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc as StdArc, Mutex};

    // Step spacing needs no explicit sleep: `expect_event` blocks until the
    // coalescing window flushed, so every subsequent synthesized change
    // starts a fresh window (real inotify events, no fake clocks).
    /// How long a "nothing arrives" assertion waits; comfortably longer than
    /// one debounce window.
    const QUIET_ASSERT: Duration = Duration::from_millis(1200);
    /// Upper bound for waiting on expected deliveries.
    const DELIVERY_TIMEOUT: Duration = Duration::from_millis(3000);

    /// Temp dir host (formats_cue CueFixture precedent): runtime-synthesized
    /// material only, removed on drop; no real user music directories.
    struct WatchFixture {
        root: PathBuf,
    }

    impl WatchFixture {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("musid-dli-watch-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self {
                root: fs::canonicalize(dir).unwrap(),
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
    }

    impl Drop for WatchFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Minimal PCM WAV stub (formats_cue `write_wav` precedent); a valid
    /// RIFF header plus zeros is enough because the watcher never parses
    /// audio — but keeping it a real audio stub matches the repo convention.
    fn write_wav(dir: &Path, name: &str) {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&44u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        fs::write(dir.join(name), &wav).unwrap();
    }

    /// Waits for the next event for `path`, asserting its kind. Interleaved
    /// events for OTHER paths (typically the watched root itself: creating or
    /// removing an entry moves the directory's mtime, which inotify reports
    /// as IN_ATTRIB on the dir watch, i.e. a `Modified` event for the root
    /// path) are skipped as legitimate noise. Panics on timeout or kind
    /// mismatch for `path` itself.
    fn expect_event(
        rx: &mpsc::Receiver<WatchEvent>,
        path: &Path,
        expected: WatchEventKind,
    ) -> WatchEvent {
        let deadline = Instant::now() + DELIVERY_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for {expected:?} on {}",
                path.display()
            );
            match rx.recv_timeout(remaining) {
                Ok(event) => {
                    if event.path != path {
                        continue;
                    }
                    assert_eq!(
                        event.kind,
                        expected,
                        "unexpected kind for {}",
                        path.display()
                    );
                    return event;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for {expected:?} on {}", path.display());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("watcher channel disconnected while waiting for {expected:?}");
                }
            }
        }
    }

    /// Waits for the next event of any kind; panics on timeout.
    fn expect_any(rx: &mpsc::Receiver<WatchEvent>) -> WatchEvent {
        match rx.recv_timeout(DELIVERY_TIMEOUT) {
            Ok(event) => event,
            Err(err) => panic!("expected an event, got {err:?}"),
        }
    }

    /// Asserts the stream stays quiet for [`QUIET_ASSERT`].
    fn expect_quiet(rx: &mpsc::Receiver<WatchEvent>) {
        match rx.recv_timeout(QUIET_ASSERT) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(event) => panic!("expected quiet, got {event:?}"),
            Err(err) => panic!("expected quiet, got {err:?}"),
        }
    }

    /// After `StopHandle::drop` (which joins the thread) the watcher thread
    /// has exited and dropped its sink, so the channel face deterministically
    /// disconnects — the observable "watcher fully stopped" signal. Events
    /// delivered before the drop stay in the buffer and drain first.
    fn expect_disconnected(rx: &mpsc::Receiver<WatchEvent>) {
        match rx.recv_timeout(QUIET_ASSERT) {
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!("expected the channel to disconnect after drop, got {other:?}"),
        }
    }

    // ───────────────────── pure-function pins ─────────────────────

    /// T59a: the three non-mutating access forms are filtered (verbatim
    /// port of voicefox `mod.rs:617-627`, pinned on every platform).
    #[test]
    fn access_filter_drops_non_mutating_forms() {
        let filtered = |kind: EventKind| is_non_mutating_access(&notify::Event::new(kind));
        assert!(filtered(EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(filtered(EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(filtered(EventKind::Access(AccessKind::Read)));
        // Mutating or evidence-bearing access survives the filter.
        assert!(!filtered(EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(!filtered(EventKind::Access(AccessKind::Close(
            AccessMode::Any
        ))));
        assert!(!filtered(EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Any
        ))));
    }

    /// T59b: filtered access events map to zero events even when they carry
    /// paths (the mapping happens before the fan-out).
    #[test]
    fn filtered_access_maps_to_no_watch_events() {
        let raw = notify::Event::new(EventKind::Access(AccessKind::Open(AccessMode::Any)))
            .add_path(PathBuf::from("/tmp/whatever.mp3"));
        assert!(event_to_watch_events(Ok(raw)).is_empty());
    }

    /// T63: the kind-mapping table, including the rename arm ordering
    /// (`Modify(Name)` before the general `Modify` arm).
    #[test]
    fn event_kind_mapping_table() {
        let mapped = |kind: EventKind| map_event_kind(&kind);
        assert_eq!(
            mapped(EventKind::Create(notify::event::CreateKind::File)),
            WatchEventKind::Created
        );
        assert_eq!(
            mapped(EventKind::Modify(ModifyKind::Data(
                notify::event::DataChange::Any
            ))),
            WatchEventKind::Modified
        );
        assert_eq!(
            mapped(EventKind::Modify(ModifyKind::Metadata(
                notify::event::MetadataKind::Any
            ))),
            WatchEventKind::Modified
        );
        assert_eq!(
            mapped(EventKind::Modify(ModifyKind::Name(
                notify::event::RenameMode::To
            ))),
            WatchEventKind::Renamed
        );
        assert_eq!(
            mapped(EventKind::Modify(ModifyKind::Name(
                notify::event::RenameMode::From
            ))),
            WatchEventKind::Renamed
        );
        assert_eq!(
            mapped(EventKind::Remove(notify::event::RemoveKind::File)),
            WatchEventKind::Removed
        );
        assert_eq!(
            mapped(EventKind::Access(AccessKind::Close(AccessMode::Write))),
            WatchEventKind::Other
        );
        assert_eq!(mapped(EventKind::Other), WatchEventKind::Other);
    }

    /// T58-with-paths: one `Ok` event fans out to one `WatchEvent` per raw
    /// path; an empty path list maps to nothing.
    #[test]
    fn ok_event_fans_out_per_path() {
        let raw = notify::Event::new(EventKind::Create(notify::event::CreateKind::File))
            .add_path(PathBuf::from("/a"))
            .add_path(PathBuf::from("/b"));
        let events = event_to_watch_events(Ok(raw));
        assert_eq!(
            events,
            vec![
                WatchEvent {
                    path: PathBuf::from("/a"),
                    kind: WatchEventKind::Created,
                },
                WatchEvent {
                    path: PathBuf::from("/b"),
                    kind: WatchEventKind::Created,
                },
            ]
        );
        assert!(event_to_watch_events(Ok(notify::Event::new(EventKind::Other))).is_empty());
    }

    /// T65: backend errors surface as one `Other` evidence event, for the
    /// error's first path or an empty path when it carries none (the module
    /// docs: evidence that change evidence may be missing).
    #[test]
    fn backend_errors_surface_as_other_evidence() {
        let bare = notify::Error::new(notify::ErrorKind::Generic("overflow".into()));
        let events = event_to_watch_events(Err(bare));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WatchEventKind::Other);
        assert_eq!(events[0].path, PathBuf::new());

        let with_path = notify::Error::new(notify::ErrorKind::MaxFilesWatch)
            .add_path(PathBuf::from("/watched"));
        let events = event_to_watch_events(Err(with_path));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WatchEventKind::Other);
        assert_eq!(events[0].path, PathBuf::from("/watched"));
    }

    /// T64: the debounce floor clamps configured values up to
    /// [`MIN_DEBOUNCE`] and passes larger values through unchanged
    /// (voicefox `mod.rs:558`).
    #[test]
    fn debounce_floor_clamps_small_values() {
        assert_eq!(effective_debounce(&WatchOptions::default()), MIN_DEBOUNCE);
        assert_eq!(
            effective_debounce(&WatchOptions {
                debounce: Duration::from_millis(0),
            }),
            MIN_DEBOUNCE
        );
        assert_eq!(
            effective_debounce(&WatchOptions {
                debounce: Duration::from_millis(100),
            }),
            MIN_DEBOUNCE
        );
        assert_eq!(
            effective_debounce(&WatchOptions {
                debounce: Duration::from_secs(2),
            }),
            Duration::from_secs(2)
        );
    }

    /// First-wins coalescing pinned as a pure function: later events for a
    /// path already in the batch are absorbed.
    #[test]
    fn coalesce_keeps_first_kind_per_path() {
        let mut pending = Vec::new();
        let mut seen = HashMap::new();
        let modified = WatchEvent {
            path: PathBuf::from("/a.wav"),
            kind: WatchEventKind::Modified,
        };
        coalesce_push(&mut pending, &mut seen, modified.clone());
        let other = WatchEvent {
            path: PathBuf::from("/a.wav"),
            kind: WatchEventKind::Other,
        };
        coalesce_push(&mut pending, &mut seen, other);
        let created = WatchEvent {
            path: PathBuf::from("/b.wav"),
            kind: WatchEventKind::Created,
        };
        coalesce_push(&mut pending, &mut seen, created.clone());
        assert_eq!(pending, vec![modified, created]);
    }

    // ───────────────────── live-event tests ─────────────────────

    /// T58: create / modify / remove arrive over the channel with the mapped
    /// kinds (real inotify events, temp dir only).
    #[test]
    fn create_modify_remove_reach_the_channel() {
        let fixture = WatchFixture::new("create-modify-remove");
        let (rx, handle) =
            watch_recursive_channel(std::slice::from_ref(&fixture.root), WatchOptions::default())
                .unwrap();

        write_wav(&fixture.root, "a.wav");
        expect_event(&rx, &fixture.path("a.wav"), WatchEventKind::Created);

        fs::write(fixture.path("a.wav"), b"second write").unwrap();
        expect_event(&rx, &fixture.path("a.wav"), WatchEventKind::Modified);

        fs::remove_file(fixture.path("a.wav")).unwrap();
        expect_event(&rx, &fixture.path("a.wav"), WatchEventKind::Removed);

        drop(handle);
        expect_disconnected(&rx);
    }

    /// Rename: `Modify(Name(To))` on the new path (and the `From` half on
    /// the old path) map to [`WatchEventKind::Renamed`].
    #[test]
    fn rename_arrives_as_renamed_kind() {
        let fixture = WatchFixture::new("rename");
        let (rx, handle) =
            watch_recursive_channel(std::slice::from_ref(&fixture.root), WatchOptions::default())
                .unwrap();

        write_wav(&fixture.root, "old.wav");
        expect_event(&rx, &fixture.path("old.wav"), WatchEventKind::Created);

        fs::rename(fixture.path("old.wav"), fixture.path("new.wav")).unwrap();
        let mut saw_new = false;
        let deadline = Instant::now() + DELIVERY_TIMEOUT;
        while !saw_new && !deadline.saturating_duration_since(Instant::now()).is_zero() {
            match rx.recv_timeout(remaining_or_panic(&deadline)) {
                Ok(event) => {
                    if event.path == fixture.root {
                        continue; // the rename moves the root's mtime (IN_ATTRIB)
                    }
                    if event.path == fixture.path("new.wav") {
                        assert_eq!(event.kind, WatchEventKind::Renamed);
                        saw_new = true;
                    } else {
                        // The `From` half reports the old path as Renamed.
                        assert_eq!(event.path, fixture.path("old.wav"));
                        assert_eq!(event.kind, WatchEventKind::Renamed);
                    }
                }
                Err(err) => panic!("waiting for the renamed event, got {err:?}"),
            }
        }
        assert!(saw_new, "no Renamed event for the new path");

        drop(handle);
        expect_disconnected(&rx);
    }

    fn remaining_or_panic(deadline: &Instant) -> Duration {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for the renamed event"
        );
        remaining
    }

    /// T59c (live): reading watched content produces only non-mutating
    /// access events, which never reach the channel. On Linux the observable
    /// access form is `Access(Open(_))` from opening the watched directory
    /// itself (`read_dir`); a plain file read additionally emits nothing on
    /// Linux because files are watched only when the root IS the file.
    /// Either way the delivered stream must stay quiet.
    #[test]
    fn access_events_are_filtered_live() {
        let fixture = WatchFixture::new("access-filter");
        write_wav(&fixture.root, "existing.wav");
        let (rx, handle) =
            watch_recursive_channel(std::slice::from_ref(&fixture.root), WatchOptions::default())
                .unwrap();

        // The pre-watch file must not have produced any event.
        expect_quiet(&rx);
        // Open the watched directory (IN_OPEN → Access(Open)) and read the
        // watched file (no dir-watch event at all on Linux). Neither may
        // surface.
        let _ = fs::read_dir(&fixture.root).unwrap().count();
        let _ = fs::read(fixture.path("existing.wav")).unwrap();
        expect_quiet(&rx);

        // Sanity: the watcher is still alive — a real change still arrives.
        fs::write(fixture.path("created-after-read.wav"), b"x").unwrap();
        expect_event(
            &rx,
            &fixture.path("created-after-read.wav"),
            WatchEventKind::Created,
        );

        drop(handle);
        expect_disconnected(&rx);
    }

    /// T60: a rapid same-file write burst coalesces into exactly ONE event
    /// after the quiet window (the debounce merge, ask requirement).
    #[test]
    fn burst_within_debounce_window_coalesces() {
        let fixture = WatchFixture::new("debounce-merge");
        write_wav(&fixture.root, "burst.wav");
        let (rx, handle) =
            watch_recursive_channel(std::slice::from_ref(&fixture.root), WatchOptions::default())
                .unwrap();
        expect_quiet(&rx);

        // Five writes well inside one 250 ms window.
        for round in 0..5u8 {
            fs::write(fixture.path("burst.wav"), format!("write {round}")).unwrap();
        }

        // Exactly one coalesced event for the path, after the window.
        let event = expect_any(&rx);
        assert_eq!(event.path, fixture.path("burst.wav"));
        assert_eq!(event.kind, WatchEventKind::Modified);
        expect_quiet(&rx);

        drop(handle);
        expect_disconnected(&rx);
    }

    /// T68: the [`COALESCE_MAX_PATHS`] cap flushes a large burst before the
    /// quiet window completes, without losing any path (bulk-copy bound).
    #[test]
    fn coalesce_cap_flushes_large_bursts_without_loss() {
        let fixture = WatchFixture::new("coalesce-cap");
        let (rx, handle) =
            watch_recursive_channel(std::slice::from_ref(&fixture.root), WatchOptions::default())
                .unwrap();

        let total = COALESCE_MAX_PATHS + 44;
        for index in 0..total {
            write_wav(&fixture.root, &format!("file-{index:04}.wav"));
        }

        // Collect until quiet, then assert the full set, exactly once each.
        // Creating an entry also moves the watched root's own mtime, which
        // inotify reports as IN_ATTRIB on the dir watch: root-path events are
        // legitimate noise here and arrive in whatever window they land in.
        let mut events = Vec::new();
        loop {
            match rx.recv_timeout(DELIVERY_TIMEOUT) {
                Ok(event) => {
                    if event.path != fixture.root {
                        events.push(event);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(err) => panic!("channel error while collecting: {err:?}"),
            }
        }
        // Every path must surface at least once. A path whose raw events
        // straddle a cap flush may legitimately surface twice (the boundary
        // file's CREATE is flushed with batch 1, its MODIFY arrives in batch
        // 2 with kind Modified), so the bound is "1 or 2 per path", never an
        // unbounded storm.
        let mut counts: HashMap<PathBuf, usize> = HashMap::new();
        for event in &events {
            assert!(
                matches!(
                    event.kind,
                    WatchEventKind::Created | WatchEventKind::Modified
                ),
                "unexpected kind {event:?}"
            );
            *counts.entry(event.path.clone()).or_default() += 1;
        }
        assert_eq!(counts.len(), total, "every path must surface");
        for (path, count) in &counts {
            assert!(
                (1..=2).contains(count),
                "path {path:?} surfaced {count} times (straddle duplicate or storm)"
            );
        }
        for index in 0..total {
            let path = fixture.path(&format!("file-{index:04}.wav"));
            assert!(counts.contains_key(&path), "missing {path:?}");
        }
        assert!(
            events.len() <= 2 * total,
            "duplicate storm: {} events for {total} paths",
            events.len()
        );

        drop(handle);
        expect_disconnected(&rx);
    }

    /// T61: `StopHandle` Drop stops delivery and joins the thread (voicefox
    /// `mod.rs:61-75`): proven liveness first, then silence after the drop.
    #[test]
    fn drop_stops_delivery_and_joins() {
        let fixture = WatchFixture::new("drop-stops");
        let (rx, handle) =
            watch_recursive_channel(std::slice::from_ref(&fixture.root), WatchOptions::default())
                .unwrap();

        write_wav(&fixture.root, "before.wav");
        expect_event(&rx, &fixture.path("before.wav"), WatchEventKind::Created);

        // Drop joins the thread: it returns only after the watcher thread has
        // exited and dropped its sink, so the disconnect below is the
        // observable "watcher fully stopped" signal and simultaneously proves
        // no event for a later change can ever surface.
        drop(handle);

        write_wav(&fixture.root, "after.wav");
        expect_disconnected(&rx);
        fs::write(fixture.path("before.wav"), b"touched after drop").unwrap();
        // Already disconnected: nothing more to drain.
        expect_disconnected(&rx);
    }

    /// T62: the callback face receives the same events (the host's choice
    /// between `FnMut` and `std::sync::mpsc`, ask requirement).
    #[test]
    fn callback_face_receives_events() {
        let fixture = WatchFixture::new("callback-face");
        let received: StdArc<Mutex<Vec<WatchEvent>>> = StdArc::default();
        let sink_received = StdArc::clone(&received);
        let sink = WatchSink::Callback(Box::new(move |event: WatchEvent| {
            sink_received.lock().unwrap().push(event);
        }));
        let handle = watch_recursive(
            std::slice::from_ref(&fixture.root),
            WatchOptions::default(),
            sink,
        )
        .unwrap();

        write_wav(&fixture.root, "callback.wav");
        let deadline = Instant::now() + DELIVERY_TIMEOUT;
        loop {
            let snapshot = received.lock().unwrap().clone();
            if snapshot
                .iter()
                .any(|event| event.path == fixture.path("callback.wav"))
            {
                let event = snapshot
                    .iter()
                    .find(|event| event.path == fixture.path("callback.wav"))
                    .unwrap();
                assert_eq!(event.kind, WatchEventKind::Created);
                break;
            }
            assert!(
                !deadline.saturating_duration_since(Instant::now()).is_zero(),
                "timed out waiting for the callback event"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        drop(handle);
    }

    /// T66: empty roots yield an inert handle (upstream early-return,
    /// `mod.rs:540-542`), Drop is a no-op.
    #[test]
    fn empty_roots_yield_inert_handle() {
        let handle = watch_recursive(
            &[],
            WatchOptions::default(),
            WatchSink::Channel({
                let (tx, _rx) = mpsc::channel();
                tx
            }),
        )
        .unwrap();
        drop(handle); // must not hang or panic
    }

    /// T67: a non-directory root is silently skipped like the upstream watch
    /// loop (`mod.rs:545-555`); watching succeeds and reports nothing.
    #[test]
    fn non_directory_root_is_skipped() {
        let fixture = WatchFixture::new("non-dir-root");
        write_wav(&fixture.root, "the-file.wav");
        let (rx, handle) =
            watch_recursive_channel(&[fixture.path("the-file.wav")], WatchOptions::default())
                .unwrap();

        // Changes around the unwatched file stay silent.
        fs::write(fixture.path("the-file.wav"), b"rewritten").unwrap();
        write_wav(&fixture.root, "sibling.wav");
        expect_quiet(&rx);

        drop(handle);
        expect_disconnected(&rx);
    }
}
