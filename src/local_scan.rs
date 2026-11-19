//! M5 local scan core: streaming, failure-tolerant, incremental
//! (docs/integration/LOCAL_ENHANCEMENT_DESIGN.md §3/§6/§7; M5 slices s1, s2,
//! s4, s5 and s10).
//!
//! The legacy scan face in [`crate::local`] keeps its exact signatures and
//! its one-bad-file-aborts-everything semantics; this module is the new core
//! that hosts migrate to when they need:
//!
//! * a streaming engine: walkdir discovery stays lazy on the calling thread
//!   and every entry/failure is handed out through a callback one at a time;
//!   the library core never accumulates entries (large-library memory
//!   boundary);
//! * a failure ledger: unreadable files, permission errors, walker errors
//!   and bad metadata are recorded as [`ScanFailure`] and the scan continues
//!   (parity L8; an intentional superset of upstream, whose walker errors
//!   were silently dropped, voicefox `scanner.rs:247`);
//! * CUE sheets as scan products: a `.cue` source yields one entry per
//!   logical track with the source-vs-playback dual path model (parity L21;
//!   identity `"{cue}#{NN}"`, voicefox `scanner.rs:353`);
//! * incremental scanning: [`FileFingerprint`] (size + mtime nanos, voicefox
//!   `scanner.rs:47-66`) keyed by the source file, so one CUE maps to all of
//!   its tracks without displacing the plain audio entry of the same file
//!   (voicefox `scanner.rs:147-229` + `mod.rs:219-227`); a root-level
//!   [`DirSignature`] fast path skips trees whose ROOT signature is unchanged
//!   (root-direct entries only, see [`DirSignature`]) and `ScanOptions::force`
//!   bypasses it (voicefox `mod.rs:200-217`, `592-596`);
//! * cancellation, progress and bounded parse concurrency, all std-only
//!   (musid-only, upstream has none of these);
//! * diagnostics as pure functions over host-held state (parity L9/L12,
//!   M5 s5): [`duplicate_groups`] groups suspect duplicates by the
//!   whitespace-folded, lowercased title/artist/album/duration key
//!   (voicefox `duplicate_key` `mod.rs:643-658`; deliberately not the
//!   matcher's de-symbolized normalization, `src/matcher.rs:98-103`),
//!   [`missing_entries`] folds the disappeared-source difference set into a
//!   host-held missing list whose records clear when the path returns
//!   (voicefox `mod.rs:259-269, 627-641`), and [`corrupt_files`] is the
//!   named corrupt-files view of the failure ledger (voicefox
//!   `mod.rs:467-470`). The library keeps none of this state.
//!
//! There is no in-memory library here: the previous scan state is supplied
//! by the host ([`ScanState`]) and each scan folds events into a new state.
//! Library-owned state, generation counters and automatic rescans are out of
//! scope by design.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::UNIX_EPOCH;

use crate::formats::{ContainerFormatHandler, ContainerTrack};
use crate::formats_cue::CueHandler;
use crate::local::{
    AUDIO_EXTENSIONS, LocalMetadata, MetadataReadPolicy, finalize_metadata, normalize_text,
    read_lyrics, read_metadata_inner,
};

/// CUE sheets are media-describing sources alongside the audio extensions
/// (voicefox `CUE_EXTENSION`, `scanner.rs:21, 35-41`).
pub const CUE_EXTENSION: &str = "cue";

/// Extra key carrying the source file size in bytes (parity L24 wiring,
/// voicefox `scanner.rs:203-210`).
pub const EXTRA_FILE_SIZE: &str = "file_size";

/// Extra key carrying the source file mtime in nanos since the Unix epoch.
/// Key name copied verbatim from voicefox `core/src/model/song.rs:8`.
pub const EXTRA_FILE_MODIFIED_UNIX_NANOS: &str = "file_modified_unix_nanos";

/// Extra key carrying the owning CUE sheet path (voicefox `scanner.rs:381-385`).
pub const EXTRA_CUE_FILE: &str = "cue_file";

/// Extra key carrying the two-digit CUE track number (voicefox `scanner.rs:382`).
pub const EXTRA_CUE_TRACK: &str = "cue_track";

/// Extra key carrying the CUE track start offset in milliseconds
/// (voicefox `scanner.rs:383-385`).
pub const EXTRA_CUE_START_MS: &str = "cue_start_ms";

/// Extra key carrying the full lyrics text on plain audio entries when
/// [`ScanOptions::read_lyrics`] is enabled (s7, LOCAL_ENHANCEMENT_DESIGN.md
/// §3.10). The value is whatever [`crate::local::read_lyrics`] resolves for
/// the audio file: the sidecar `.lrc` verbatim or the trimmed embedded
/// `ItemKey::Lyrics`.
pub const EXTRA_LYRICS: &str = "lyrics";

/// Extra key carrying the resolved cover path on audio entries when
/// [`ScanOptions::cover`] is configured (s8, feature `local-cover`,
/// LOCAL_ENHANCEMENT_DESIGN.md §3.11). The value is whatever
/// [`crate::local_cover::read_cover`] resolves for the playback file: the
/// validated cache path of the embedded cover or the same-directory
/// external file. A path, never image bytes (the s8 memory boundary).
#[cfg(feature = "local-cover")]
pub const EXTRA_COVER_CACHE_PATH: &str = "cover_cache_path";

/// Excluded directory names ported from voicefox `scanner.rs:24`; applied by
/// name, never as path patterns (voicefox `scanner.rs:242-246`).
const DEFAULT_EXCLUDED_DIRS: &[&str] = &[".stfolder", "node_modules", ".git", ".Trash"];

// ───────────────────────── Scan configuration ─────────────────────────

/// How symbol links behave during the walk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SymlinkPolicy {
    /// Skip every symlink entry: the current behavior
    /// (`follow_links(false)`, `src/local.rs:66`).
    #[default]
    Skip,
    /// Resolve symlinks (`walkdir` `follow_links(true)`). One entity
    /// reachable through N links yields N entries (distinct `source_path`,
    /// each counted in `discovered`/`parsed` and keyed separately in the
    /// incremental state); looping links come back as walker errors and land
    /// in the ledger without hanging the walk.
    Follow,
}

/// Configurable scan surface (M5 s1). `Default` matches the legacy walk plus
/// the ported upstream exclusion list; every deviation is opt-in.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Directory depth limit; `0` means unlimited (voicefox `scanner.rs:231-238`).
    pub max_depth: u32,
    /// Directory names to prune during the walk. Defaults to the ported
    /// upstream list; callers append their own. The root itself (depth 0) is
    /// never pruned.
    pub excluded_dirs: Vec<String>,
    /// Skip files and directories whose name starts with `.`. Defaults to
    /// `false`, which matches both the legacy behavior and upstream (upstream
    /// has no hidden filter); flipping it on is opt-in.
    pub skip_hidden: bool,
    /// Symbol link policy; [`SymlinkPolicy::Skip`] is the current behavior.
    pub symlinks: SymlinkPolicy,
    /// Bypass the root directory-signature fast path. Callers with positive
    /// evidence of change (a watcher event, a scheduled full pass) must set
    /// this: directory mtimes do not reflect in-place edits, and an add or
    /// remove INSIDE an existing subdirectory moves only that subdirectory's
    /// mtime, not the root's, so the fast path would skip it (voicefox
    /// `mod.rs:592-596`, same root-level scope). Hosts without a recursive
    /// watcher must force periodically or diff the tree themselves. Changing
    /// other options between scans (`skip_hidden`, `excluded_dirs`,
    /// `max_depth`, `symlinks`) does NOT invalidate the fast path either: the
    /// signature only tracks the root's direct entries, so an option change
    /// needs `force` (or option-keyed signatures kept by the host) to take
    /// effect on an unchanged tree.
    pub force: bool,
    /// Parse-stage worker count; `1` parses inline on the calling thread with
    /// zero extra threads. Discovery stays single-threaded and lazy for any
    /// value.
    pub max_parse_workers: usize,
    /// With more than one worker, deliver parse results in discovery order
    /// (a reorder buffer). `false` delivers in completion order.
    pub ordered_output: bool,
    /// Read the lyrics of every plain audio file during the parse stage and
    /// put the full text under `extra[EXTRA_LYRICS]`. Defaults to `false`:
    /// lyric text can be many KiB per track, so the default scan product
    /// stays memory-light and hosts fetch on demand via the standalone
    /// [`crate::local::read_lyrics`]. The lookup is the same sidecar-first
    /// face as the standalone function: `audio.with_extension("lrc")`
    /// (UTF-8 with a GB18030 fallback) wins over the embedded
    /// `ItemKey::Lyrics` tags (LOCAL_ENHANCEMENT_DESIGN.md §3.10).
    ///
    /// Boundaries: CUE entries never carry lyrics (one sheet fans out to N
    /// tracks sharing one audio file whose text is file-relative, so a
    /// per-track copy would be N duplicated texts with track-wrong
    /// timestamps; the sheet's own sidecar is out of scan scope too, hosts
    /// call `read_lyrics` themselves if they want it). A changed sidecar
    /// next to an unchanged audio does not move the audio's fingerprint, so
    /// incremental scans reuse the stale text until the host forces a
    /// rescan.
    pub read_lyrics: bool,
    /// Cover resolution opt-in (s8, feature `local-cover`,
    /// LOCAL_ENHANCEMENT_DESIGN.md §3.11). Empty by default: a scan without
    /// a host-specified cache directory does no cover work — no extra
    /// reads, no cache writes, no [`EXTRA_COVER_CACHE_PATH`] key, and the
    /// default scan product carries no image bytes (the s8 memory
    /// boundary). With a cache directory set, every plain audio entry gets
    /// `extra[EXTRA_COVER_CACHE_PATH]` from
    /// [`crate::local_cover::read_cover`] (embedded cover through the
    /// decode-validated cache, external cover per the configured priority),
    /// and CUE entries resolve the cover of each distinct referenced audio
    /// file once (a cover path is a single path shared by all tracks of a
    /// sheet, so per-track copies are exact — unlike the lyrics text above).
    /// Reused entries keep the cover path their previous scan recorded.
    #[cfg(feature = "local-cover")]
    pub cover: crate::local_cover::CoverScanOptions,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_depth: 0,
            excluded_dirs: DEFAULT_EXCLUDED_DIRS
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            skip_hidden: false,
            symlinks: SymlinkPolicy::default(),
            force: false,
            max_parse_workers: 1,
            ordered_output: true,
            read_lyrics: false,
            #[cfg(feature = "local-cover")]
            cover: crate::local_cover::CoverScanOptions::default(),
        }
    }
}

// ───────────────────────── Fingerprints ─────────────────────────

/// Filesystem fingerprint: size plus mtime in nanos. Pure fs metadata, no
/// content hashing; nanos so tag editors saving twice inside one second are
/// still seen (voicefox `FileFingerprint`, `scanner.rs:47-66`). Granularity
/// caveat: filesystems with coarse timestamps (exFAT/FAT32 round to 2
/// seconds) can miss a same-size in-place edit inside one granularity
/// window; the upstream fingerprint accepts the same limitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileFingerprint {
    pub size: u64,
    pub modified_unix_nanos: u128,
}

impl FileFingerprint {
    pub fn from_path(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        let modified_unix_nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Ok(Self {
            size: metadata.len(),
            modified_unix_nanos,
        })
    }
}

/// Directory-level signature: the directory's own size and mtime. Only
/// changes among the directory's DIRECT entries (create, delete, rename)
/// move it: an in-place content edit moves no signature, and a change inside
/// an existing SUBdirectory moves only that subdirectory's signature, never
/// this one. A matching root signature therefore proves "no direct-entry
/// churn" and nothing more; rescans with positive change evidence (watcher
/// event, scheduled full pass) must pass [`ScanOptions::force`], and hosts
/// without a recursive watcher must force periodically or diff the tree
/// themselves (voicefox `DirSignature`, `scanner.rs:68-96`, same root-level
/// scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirSignature {
    pub size: u64,
    pub modified_unix_nanos: u128,
}

impl DirSignature {
    pub fn from_path(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_dir() {
            return None;
        }
        let modified_unix_nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Some(Self {
            size: metadata.len(),
            modified_unix_nanos,
        })
    }
}

// ───────────────────────── Ledger ─────────────────────────

/// Why a source failed. Stage-based classification: walk/stat errors split by
/// the io error kind; lofty reads split IO from parse through
/// `lofty::error::ErrorKind::Io`; the CUE pipeline reports `Parse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanFailureKind {
    Permission,
    Io,
    Parse,
}

/// One unreadable or unparseable source. Ledger entries never enter the
/// entry stream; the file is retried on the next scan because its fingerprint
/// is only persisted with a successfully parsed entry (same as upstream,
/// whose reuse branch requires previous songs for the file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFailure {
    pub path: PathBuf,
    pub error: String,
    pub kind: ScanFailureKind,
}

// ───────────────────────── Entries ─────────────────────────

/// Scan counters, the upstream `LocalScanStats` shape (voicefox
/// `mod.rs:52-59`). `discovered` counts media files whose fingerprint stat
/// succeeded (CUE sheets included, corrupt files included); `parsed` counts
/// newly parsed source files; `reused` counts entries reused from the
/// previous state; `removed` counts previous sources under the root that the
/// scan no longer saw (skipped, i.e. `0`, on cancelled scans).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalScanStats {
    pub discovered: usize,
    pub reused: usize,
    pub parsed: usize,
    pub failed: usize,
    pub removed: usize,
}

/// One library entry with the source-vs-playback dual path model
/// (LOCAL_ENHANCEMENT_DESIGN.md §3.4): a plain audio file is its own source
/// and playback path, a CUE track's source is the sheet and its playback path
/// is the referenced audio file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
    /// Incremental key: the file itself, or the owning `.cue` for a container
    /// track (one key, many tracks, no mutual displacement).
    pub source_path: PathBuf,
    /// File a player opens: the audio itself, or the CUE-referenced audio.
    /// Path resolution follows the upstream parser (`canonicalize` of
    /// `cue.parent().join(FILE name)`), so a sheet with `../` or absolute
    /// FILE names can point outside the scan root; containment is the
    /// host's responsibility (LOCAL_ENHANCEMENT_DESIGN.md §9).
    pub play_path: PathBuf,
    /// Container track identity: `"{cue path}#{NN}"` for CUE tracks, `None`
    /// for plain audio (voicefox `scanner.rs:353`).
    pub container_track: Option<String>,
    /// CUE INDEX 01 offset in milliseconds; `0` for plain audio.
    pub start_ms: u64,
    pub metadata: LocalMetadata,
    /// Fingerprint of `source_path`, persisted with the entry for reuse.
    pub fingerprint: FileFingerprint,
    /// `true` when this scan cloned the entry from the previous state
    /// without reparsing it.
    pub reused: bool,
    /// L24 wiring (voicefox `scanner.rs:203-210, 381-385`), ready to merge
    /// into `Track.extra`: `file_size`, `file_modified_unix_nanos`, and for
    /// CUE tracks also `cue_file` / `cue_track` / `cue_start_ms`. The `file_*`
    /// keys describe the source file (an intentional superset: upstream only
    /// writes them for plain audio).
    pub extra: BTreeMap<String, String>,
}

/// The previous scan state a host owns and passes back. Nothing here holds
/// threads, runtimes or library state; persistence stays with the host.
#[derive(Debug, Default, Clone)]
pub struct ScanState {
    /// Entries grouped by `source_path`; a CUE sheet groups all of its tracks.
    pub entries: HashMap<PathBuf, Vec<ScanEntry>>,
    /// Root directory signatures recorded by previous scans
    /// ([`ScanSummary::dir_signature`]).
    pub dir_signatures: HashMap<PathBuf, DirSignature>,
}

impl ScanState {
    /// Folds one event into the state; the single-step fold used with
    /// [`scan_stream`]. Failures are not absorbed (they live on the summary).
    pub fn absorb(&mut self, event: &ScanEvent) {
        if let ScanEvent::Entry(entry) = event {
            self.entries
                .entry(entry.source_path.clone())
                .or_default()
                .push(entry.as_ref().clone());
        }
    }

    /// Previous entry groups whose source path sits under `root`.
    pub fn entries_under(&self, root: &Path) -> impl Iterator<Item = (&PathBuf, &Vec<ScanEntry>)> {
        self.entries
            .iter()
            .filter(move |(source, _)| source.starts_with(root))
    }

    /// Fingerprint of a source file, as persisted with its entries.
    pub fn fingerprint_of(&self, source: &Path) -> Option<FileFingerprint> {
        self.entries
            .get(source)?
            .first()
            .map(|entry| entry.fingerprint)
    }

    /// Records the root signature reported by a finished scan. The root is
    /// canonicalized exactly like `scan_stream` does, so the key matches the
    /// engine's lookup key even when the host passes a symlinked or relative
    /// root.
    pub fn set_dir_signature(&mut self, root: &Path, signature: DirSignature) {
        let root = Self::canonical_root(root);
        self.dir_signatures.insert(root, signature);
    }

    /// Folds one finished scan into the state; the recommended fold for the
    /// root signature after entries were absorbed with
    /// [`ScanState::absorb`]. A complete, clean walk
    /// (`summary.dir_signature` is `Some`) records the signature; anything
    /// else (cancelled run, ledger entry) REMOVES the stored signature for
    /// the root, so no later fast path can skip files that walk never
    /// observed: permission fixes update ctime, not mtime, and repaired
    /// corrupt files are equally invisible to a signature hit. The root is
    /// canonicalized exactly like `scan_stream` does, so the keys line up
    /// regardless of the root form the host passes.
    pub fn apply_summary(&mut self, root: &Path, summary: &ScanSummary) {
        let root = Self::canonical_root(root);
        match summary.dir_signature {
            Some(signature) => {
                self.dir_signatures.insert(root, signature);
            }
            None => {
                self.dir_signatures.remove(&root);
            }
        }
    }

    /// Same root normalization as the engine: canonicalize, fall back to the
    /// given path when the file system cannot resolve it.
    fn canonical_root(root: &Path) -> PathBuf {
        root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
    }
}

// ───────────────────────── Engine surface ─────────────────────────

/// One item of the scan output stream. Entries and failures are delivered on
/// the calling thread as they are produced; the library core keeps none of
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanEvent {
    /// Boxed so the event stays small: `ScanEntry` is the largest payload of
    /// the stream and failure events must not pay for it (clippy
    /// large_enum_variant).
    Entry(Box<ScanEntry>),
    Failure(ScanFailure),
}

/// Live counter snapshot passed to [`ScanControl::progress`]. Mirrors the
/// first four [`LocalScanStats`] fields; `removed` only exists on the final
/// summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanProgress {
    pub discovered: usize,
    pub parsed: usize,
    pub failed: usize,
    pub reused: usize,
}

/// Result of one finished scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanSummary {
    pub stats: LocalScanStats,
    /// Convenience view of the ledger (low frequency). Entries are not
    /// repeated here: the core does not accumulate them.
    pub failures: Vec<ScanFailure>,
    /// `true` when cancellation stopped the scan. A cancelled summary is a
    /// partial result: the host must not wholesale-replace its previous state
    /// with the fold of a cancelled run.
    pub cancelled: bool,
    /// Root signature observed by this scan, handed out only for complete,
    /// clean walks: `None` when the scan was cancelled or the ledger holds
    /// any entry, because such a walk did not see the whole tree and its
    /// signature would let a later fast path skip files it never observed
    /// (permission fixes update ctime, not mtime; repaired corrupt files are
    /// likewise invisible to a signature hit). Prefer folding through
    /// [`ScanState::apply_summary`], which also drops a stale stored
    /// signature after an incomplete walk.
    pub dir_signature: Option<DirSignature>,
}

/// Cooperative cancellation, std-only. Cheap to clone; the token only flips
/// a flag, the engine checks it between files and between parse jobs.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Per-scan control surface, reusable across scans: [`scan_stream`] never
/// consumes it, and the progress slot is a shared reference (`&dyn Fn`,
/// covariant) so the borrow checker lets the same control (progress callback
/// and cancel token) drive any number of scans. Progress callbacks receive
/// `Copy` counter snapshots and must not re-enter this library
/// (re-entrancy constraint); hosts thread state through interior mutability
/// (`Cell`, `Arc<AtomicUsize>`) instead of captured `&mut`.
#[derive(Default)]
pub struct ScanControl<'a> {
    pub progress: Option<&'a dyn Fn(ScanProgress)>,
    pub cancel: Option<&'a CancelToken>,
}

/// Streaming scan engine. Walks `root` lazily, reuses previous entries whose
/// fingerprint is unchanged, parses the rest (optionally on worker threads),
/// and reports every entry and failure through `on_event` as it is produced.
///
/// The typical incremental fold keeps the previous and the next states
/// separate so borrows do not conflict:
///
/// ```no_run
/// use musid_dli::local_scan::{ScanControl, ScanOptions, ScanState, scan_stream};
/// use std::path::Path;
///
/// let root = Path::new("/music");
/// let previous = ScanState::default();
/// let mut next = ScanState::default();
/// let mut control = ScanControl::default(); // reusable across scans
/// let summary = scan_stream(root, &ScanOptions::default(), &previous,
///     &mut |event| next.absorb(&event), &mut control);
/// next.apply_summary(root, &summary);
/// // the host decides when `next` replaces `previous`; the fold of a
/// // cancelled or failed run is partial and must not be committed wholesale
/// ```
pub fn scan_stream<'a>(
    root: &Path,
    options: &ScanOptions,
    previous: &'a ScanState,
    on_event: &'a mut dyn FnMut(ScanEvent),
    control: &mut ScanControl<'_>,
) -> ScanSummary {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // Plain copies out of a covariant struct: nothing is consumed, so the
    // same ScanControl can drive later scans with its callbacks intact.
    let cancel = control.cancel.cloned().unwrap_or_default();
    let progress = control.progress;

    if cancel.is_cancelled() {
        return ScanSummary {
            stats: LocalScanStats::default(),
            failures: Vec::new(),
            cancelled: true,
            dir_signature: None,
        };
    }

    // Root-level fast path (voicefox `mod.rs:200-217`): the signature only
    // proves "no add/remove/rename", so callers with positive change evidence
    // must pass `ScanOptions::force = true`.
    if !options.force
        && let (Some(previous_signature), Some(current)) = (
            previous.dir_signatures.get(&root).copied(),
            DirSignature::from_path(&root),
        )
        && previous_signature == current
        && previous.entries_under(&root).next().is_some()
    {
        return fast_path_summary(&root, previous, current, on_event, progress, &cancel);
    }

    let previous_sources: HashSet<PathBuf> = previous
        .entries_under(&root)
        .map(|(source, _)| source.clone())
        .collect();
    let mut driver = ScanDriver {
        on_event,
        progress,
        ordered: options.ordered_output,
        next_seq: 0,
        pending: BTreeMap::new(),
        stats: LocalScanStats::default(),
        failures: Vec::new(),
        cancelled: false,
        previous,
        previous_sources,
        seen_previous: 0,
        read_lyrics: options.read_lyrics,
        #[cfg(feature = "local-cover")]
        cover: options.cover.clone(),
    };
    let workers = options.max_parse_workers.max(1);
    if workers == 1 {
        scan_serial(&root, options, &cancel, &mut driver)
    } else {
        scan_parallel(&root, options, workers, &cancel, &mut driver)
    }
}

/// Convenience surface: collects every entry. The library core stays
/// streaming; this buffer is the caller's choice and scales with the library.
pub fn scan_entries(
    root: &Path,
    options: &ScanOptions,
    previous: &ScanState,
) -> (Vec<ScanEntry>, ScanSummary) {
    let mut entries = Vec::new();
    let mut control = ScanControl::default();
    let summary = scan_stream(
        root,
        options,
        previous,
        &mut |event| {
            if let ScanEvent::Entry(entry) = event {
                entries.push(*entry);
            }
        },
        &mut control,
    );
    (entries, summary)
}

// ───────────────────────── Diagnostics (s5) ─────────────────────────

/// One suspect-duplicate group (parity L12, voicefox `DuplicateLocalSongs`,
/// `mod.rs:49-59` + `duplicate_groups` `mod.rs:495-512`): entries beyond the
/// first that share one `duplicate_key` normalization of
/// title/artist/album/duration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    /// The normalized key, verbatim shape of the upstream one: the
    /// whitespace-folded, lowercased title/artist/album joined with
    /// `\u{1f}` separators plus the exact duration in milliseconds
    /// (voicefox `duplicate_key`, `mod.rs:643-658`).
    pub key: String,
    /// The group's entries, ordered by (source path, container track); the
    /// source path mirrors the upstream `songs.sort_by(file_path)` and the
    /// track identity keeps a multi-track CUE group deterministic. Groups
    /// themselves are ordered by `key`.
    pub entries: Vec<ScanEntry>,
}

/// A previously scanned source that has disappeared (parity L9, voicefox
/// `MissingLocalSong` `mod.rs:37-49`, found by the difference set of
/// `mod.rs:259-269`). `path` is the source path (`ScanEntry::source_path`),
/// `track` a representative entry (the smallest container identity of the
/// vanished source, plain audio before CUE tracks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingLocalEntry {
    /// The vanished source path. A multi-track CUE sheet collapsing to one
    /// record shares this path across its tracks, exactly like the upstream
    /// `deduplicate_missing` by-path collapse (`mod.rs:627-635`).
    pub path: PathBuf,
    /// The representative previous entry of that path, kept so hosts can
    /// still render what was lost (the upstream record carries the whole
    /// `SongInfo`).
    pub track: ScanEntry,
}

/// Groups suspect duplicates (parity L12, voicefox `duplicate_groups`
/// `mod.rs:495-512`): entries whose whitespace-folded, lowercased
/// title/artist/album and EXACT duration match (`duplicate_key`; no
/// tolerance, upstream compares `duration.as_millis()` verbatim). Singleton
/// groups are dropped. A pure function over any entry sequence (a
/// `scan_entries` result, a `ScanState` via
/// `state.entries.values().flatten()`, or any iterator of `&ScanEntry`);
/// it holds no library state.
///
/// The key keeps punctuation and does not strip symbols: `"song a"` and
/// `"song-a"` are different keys here even though the crate's matcher
/// normalization (`src/matcher.rs:98-103`) would fold them together. Two
/// normalizers, two semantics, never mixed (LOCAL_ENHANCEMENT_DESIGN.md
/// §10). Untagged fields normalize to the empty string (musid keeps
/// `LocalMetadata` fields optional; upstream always has names through its
/// tag fallbacks, parity L15), so two same-duration untagged entries land
/// in one group.
pub fn duplicate_groups<'a>(
    entries: impl IntoIterator<Item = &'a ScanEntry>,
) -> Vec<DuplicateGroup> {
    let mut groups: HashMap<String, Vec<&ScanEntry>> = HashMap::new();
    for entry in entries {
        groups.entry(duplicate_key(entry)).or_default().push(entry);
    }
    let mut duplicates: Vec<DuplicateGroup> = groups
        .into_iter()
        .filter(|(_, entries)| entries.len() > 1)
        .map(|(key, mut entries)| {
            entries.sort_by(|left, right| {
                left.source_path
                    .cmp(&right.source_path)
                    .then_with(|| left.container_track.cmp(&right.container_track))
            });
            DuplicateGroup {
                key,
                entries: entries.into_iter().cloned().collect(),
            }
        })
        .collect();
    duplicates.sort_by(|left, right| left.key.cmp(&right.key));
    duplicates
}

/// Missing diagnostics (parity L9): folds one finished scan into the
/// host-held missing list and returns the updated list. New records are the
/// difference set of the previous state against this scan: previous entries
/// under `root` whose source path this scan did not see AND that no longer
/// exists on disk (voicefox `mod.rs:259-269`; the on-disk check is what
/// keeps a file hidden by an option change, e.g. a newly excluded directory,
/// from being reported missing). Records from earlier rounds passed as
/// `retained` survive until their path comes back in this scan
/// (`missing.retain(!present)`, `mod.rs:636-641`: path regression clears the
/// record). The result is deduplicated by path keeping the oldest record (a
/// vanished multi-track CUE is ONE record; upstream `deduplicate_missing`
/// `mod.rs:627-635`) and sorted by path.
///
/// Host contract: pass the list the host holds from earlier rounds as
/// `retained` (empty on the first round) and commit the folded
/// [`ScanState`] after each scan; a path that returns simply stops being
/// reported. `current` is any entry sequence of the finished scan, across
/// every root that scan covered: presence is judged globally, which is what
/// lets a record clear no matter which root the rescanning pass saw (with
/// the on-disk check this is equivalent to the upstream root-scoped diff
/// check plus its global `present` prune, `mod.rs:630-641`). `root` is
/// canonicalized exactly like the engine does, so a symlinked or relative
/// root still matches the stored entry keys. Never fold a cancelled scan
/// here: its result set is incomplete (§6.3) and would both emit false
/// missing records and clear retained ones.
pub fn missing_entries<'a>(
    retained: &[MissingLocalEntry],
    previous: &ScanState,
    current: impl IntoIterator<Item = &'a ScanEntry>,
    root: &Path,
) -> Vec<MissingLocalEntry> {
    let root = ScanState::canonical_root(root);
    let current_paths: HashSet<PathBuf> = current
        .into_iter()
        .map(|entry| entry.source_path.clone())
        .collect();

    // Difference set: previous under `root` that this scan did not see and
    // the disk no longer has (voicefox `mod.rs:259-269`).
    let mut fresh: Vec<MissingLocalEntry> = Vec::new();
    for (source, entries) in previous.entries_under(&root) {
        if current_paths.contains(source) || source.exists() {
            continue;
        }
        let track = entries
            .iter()
            .min_by(|left, right| {
                left.container_track
                    .cmp(&right.container_track)
                    .then_with(|| left.play_path.cmp(&right.play_path))
            })
            .expect("ScanState never stores an empty entry group")
            .clone();
        fresh.push(MissingLocalEntry {
            path: source.clone(),
            track,
        });
    }

    // Earlier rounds survive until the path returns (mod.rs:636-641). The
    // retained list goes first so the by-path dedup below keeps the oldest
    // record, mirroring the upstream insertion order.
    let mut records: Vec<MissingLocalEntry> = retained.to_vec();
    records.extend(fresh);
    records.retain(|record| !current_paths.contains(&record.path));

    // Dedup by path, then sort by path (upstream `deduplicate_missing`,
    // `mod.rs:627-635`).
    let mut seen = HashSet::new();
    records.retain(|record| seen.insert(record.path.clone()));
    records.sort_by(|left, right| left.path.cmp(&right.path));
    records
}

/// The corrupt-files view of a failure ledger (the UI face of parity L8,
/// voicefox `corrupt_files`, `mod.rs:467-470`: a convenience read over the
/// scan failure list). Query face only, no new mechanism: the ledger is
/// still produced exclusively by the scan engine ([`ScanSummary::failures`],
/// hosts may extend it across rounds); this returns it as-is, same order,
/// no filtering and no dedup, exactly like the upstream alias.
pub fn corrupt_files(failures: &[ScanFailure]) -> Vec<ScanFailure> {
    failures.to_vec()
}

/// Normalization for duplicate keys: whitespace-fold plus lowercase, ported
/// verbatim from the upstream `duplicate_key` closure (`mod.rs:643-658`),
/// with the collapse shared with the read layer
/// ([`crate::local::normalize_text`], s6 item b) and a fullwidth-to-halfwidth
/// fold on top. The fullwidth fold and the lowercase exist ONLY here: they
/// are comparison-key material and never reach stored values, which get the
/// collapse alone. Deliberately NOT the matcher normalization
/// (`src/matcher.rs:98-103`, which strips every non-alphanumeric character):
/// the matcher folds `"song a"` and `"song-a"` together, duplicate keys do
/// not. Two normalizers, kept apart by design (LOCAL_ENHANCEMENT_DESIGN.md
/// §10).
fn duplicate_normalize(value: Option<&str>) -> String {
    normalize_text(value)
        .map(|collapsed| {
            collapsed
                .chars()
                .map(|ch| match ch {
                    // Fullwidth ASCII variants (！..～, Ａ..ｚ) fold onto
                    // their halfwidth counterparts; nothing else folds.
                    '\u{FF01}'..='\u{FF5E}' => char::from_u32(u32::from(ch) - 0xFEE0).unwrap_or(ch),
                    _ => ch,
                })
                .collect::<String>()
                .to_lowercase()
        })
        .unwrap_or_default()
}

/// The scanner duplicate key: whitespace-folded, fullwidth-folded lowercase
/// title/artist/album ([`duplicate_normalize`]) plus the exact duration in
/// milliseconds, joined with the upstream `\u{1f}` unit separators (voicefox
/// `duplicate_key`, `mod.rs:643-658`). Untagged fields normalize to the
/// empty string; the duration enters raw so two candidates only group when
/// their measured durations are exactly equal.
fn duplicate_key(entry: &ScanEntry) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        duplicate_normalize(entry.metadata.title.as_deref()),
        duplicate_normalize(entry.metadata.artist.as_deref()),
        duplicate_normalize(entry.metadata.album.as_deref()),
        entry.metadata.duration_ms
    )
}

/// Root-signature hit: re-emit the previous entries of the root, path-sorted
/// for determinism, without walking anything.
fn fast_path_summary(
    root: &Path,
    previous: &ScanState,
    signature: DirSignature,
    on_event: &mut dyn FnMut(ScanEvent),
    progress: Option<&dyn Fn(ScanProgress)>,
    cancel: &CancelToken,
) -> ScanSummary {
    let mut stats = LocalScanStats::default();
    let mut cancelled = false;
    let mut groups: Vec<(&PathBuf, &Vec<ScanEntry>)> = previous.entries_under(root).collect();
    groups.sort_by(|(left, _), (right, _)| (*left).cmp(*right));
    'outer: for (_, entries) in groups {
        for entry in entries {
            if cancel.is_cancelled() {
                cancelled = true;
                break 'outer;
            }
            let mut entry = entry.clone();
            entry.reused = true;
            on_event(ScanEvent::Entry(Box::new(entry)));
            // counted per entry so a cancelled multi-track group still
            // accounts exactly what was emitted
            stats.reused += 1;
        }
    }
    if let Some(progress) = progress {
        progress(ScanProgress {
            reused: stats.reused,
            ..ScanProgress::default()
        });
    }
    // A cancelled fast path emitted only part of the tree, so it hands out no
    // signature (same policy as a cancelled walk).
    let dir_signature = if cancelled { None } else { Some(signature) };
    ScanSummary {
        stats,
        failures: Vec::new(),
        cancelled,
        dir_signature,
    }
}

// ───────────────────────── Driver ─────────────────────────

/// One pipeline step in delivery order terms.
enum StepResult {
    /// Entries cloned from the previous state (fingerprint hit or fast path).
    Reused(Vec<ScanEntry>),
    /// Entries freshly parsed from one source file.
    Parsed(Vec<ScanEntry>),
    Failed(ScanFailure),
    /// A queued job was skipped because cancellation was requested.
    Cancelled,
}

/// What the discovery stage decided for one media file.
enum StepPlan {
    Ready(StepResult),
    Parse(ParseJob),
}

/// A parse job handed to the worker pool (paths only: bounded memory).
struct ParseJob {
    seq: u64,
    source: PathBuf,
    is_cue: bool,
    fingerprint: FileFingerprint,
    /// s7 opt-in copied from `ScanOptions` at construction; lyric text is
    /// read during the parse stage only, so the default (false) keeps it
    /// out of the scan product.
    read_lyrics: bool,
    /// s8 opt-in (`ScanOptions::cover`, feature local-cover): the
    /// host-specified cache directory and priority, copied into every job.
    /// `cache_dir = None` disables all cover work for the job.
    #[cfg(feature = "local-cover")]
    cover: crate::local_cover::CoverScanOptions,
}

/// Result of one worker parse.
enum ParseOutcome {
    Done {
        seq: u64,
        result: std::result::Result<Vec<ScanEntry>, ScanFailure>,
    },
    Cancelled {
        seq: u64,
    },
}

/// Shared state machine: stats, progress, ordered delivery, ledger.
struct ScanDriver<'a> {
    on_event: &'a mut dyn FnMut(ScanEvent),
    progress: Option<&'a dyn Fn(ScanProgress)>,
    ordered: bool,
    /// Delivery cursor: the next discovery sequence that may be emitted.
    next_seq: u64,
    pending: BTreeMap<u64, StepResult>,
    stats: LocalScanStats,
    failures: Vec<ScanFailure>,
    cancelled: bool,
    previous: &'a ScanState,
    previous_sources: HashSet<PathBuf>,
    seen_previous: usize,
    /// s7 opt-in (`ScanOptions::read_lyrics`), copied into every parse job.
    read_lyrics: bool,
    /// s8 opt-in (`ScanOptions::cover`, feature local-cover), copied into
    /// every parse job.
    #[cfg(feature = "local-cover")]
    cover: crate::local_cover::CoverScanOptions,
}

impl ScanDriver<'_> {
    fn fire_progress(&mut self) {
        if let Some(progress) = self.progress {
            progress(ScanProgress {
                discovered: self.stats.discovered,
                parsed: self.stats.parsed,
                failed: self.stats.failed,
                reused: self.stats.reused,
            });
        }
    }

    /// Fingerprint stat plus reuse decision for one media file.
    fn decide(&mut self, path: &Path, is_cue: bool, seq: u64) -> StepPlan {
        let fingerprint = match FileFingerprint::from_path(path) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                let kind = classify_io(&error);
                return StepPlan::Ready(StepResult::Failed(ScanFailure {
                    path: path.to_path_buf(),
                    error: error.to_string(),
                    kind,
                }));
            }
        };
        self.stats.discovered += 1;
        if self.previous_sources.contains(path) {
            self.seen_previous += 1;
        }
        self.fire_progress();
        if self.previous.fingerprint_of(path) == Some(fingerprint)
            && let Some(entries) = self.previous.entries.get(path)
        {
            let entries = entries
                .iter()
                .map(|entry| {
                    let mut entry = entry.clone();
                    entry.reused = true;
                    entry
                })
                .collect();
            return StepPlan::Ready(StepResult::Reused(entries));
        }
        StepPlan::Parse(ParseJob {
            seq,
            source: path.to_path_buf(),
            is_cue,
            fingerprint,
            read_lyrics: self.read_lyrics,
            #[cfg(feature = "local-cover")]
            cover: self.cover.clone(),
        })
    }

    /// Delivers one step, honoring discovery order when `ordered_output`.
    /// Every media file yields exactly one step, so the sequence has no holes
    /// and parked steps always drain.
    fn emit_step(&mut self, seq: u64, step: StepResult) {
        if !self.ordered {
            self.deliver(step);
            return;
        }
        if seq != self.next_seq {
            self.pending.insert(seq, step);
            return;
        }
        self.deliver(step);
        self.next_seq += 1;
        while let Some(step) = self.pending.remove(&self.next_seq) {
            self.deliver(step);
            self.next_seq += 1;
        }
    }

    fn deliver(&mut self, step: StepResult) {
        match step {
            StepResult::Reused(entries) => {
                let count = entries.len();
                for entry in entries {
                    (self.on_event)(ScanEvent::Entry(Box::new(entry)));
                }
                self.stats.reused += count;
            }
            StepResult::Parsed(entries) => {
                self.stats.parsed += 1;
                for entry in entries {
                    (self.on_event)(ScanEvent::Entry(Box::new(entry)));
                }
            }
            StepResult::Failed(failure) => {
                self.stats.failed += 1;
                self.failures.push(failure.clone());
                (self.on_event)(ScanEvent::Failure(failure));
            }
            StepResult::Cancelled => self.cancelled = true,
        }
        self.fire_progress();
    }

    /// Walker errors bypass the reorder buffer: they are failures, not
    /// entries, and carry no media-file sequence.
    fn ledger_walk_error(&mut self, error: &walkdir::Error, root: &Path) {
        let path = error
            .path()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf());
        let kind = error.io_error().map_or(ScanFailureKind::Io, classify_io);
        self.deliver(StepResult::Failed(ScanFailure {
            path,
            error: error.to_string(),
            kind,
        }));
    }

    fn handle_outcome(&mut self, outcome: ParseOutcome) {
        match outcome {
            ParseOutcome::Done { seq, result } => {
                let step = match result {
                    Ok(entries) => StepResult::Parsed(entries),
                    Err(failure) => StepResult::Failed(failure),
                };
                self.emit_step(seq, step);
            }
            // Routed through the delivery pipeline so the sequence stays
            // hole-free and parked results behind it still drain.
            ParseOutcome::Cancelled { seq } => self.emit_step(seq, StepResult::Cancelled),
        }
    }

    fn finish(&mut self, root: &Path) -> ScanSummary {
        self.stats.removed = if self.cancelled {
            0
        } else {
            self.previous_sources
                .len()
                .saturating_sub(self.seen_previous)
        };
        // Hand out the signature only for a complete, clean walk: a
        // cancelled run or any ledger entry means the walk did not see the
        // whole tree, and its signature would poison later fast paths
        // (LOCAL_ENHANCEMENT_DESIGN.md §7).
        let complete = !self.cancelled && self.failures.is_empty();
        let dir_signature = if complete {
            DirSignature::from_path(root)
        } else {
            None
        };
        ScanSummary {
            stats: self.stats,
            failures: std::mem::take(&mut self.failures),
            cancelled: self.cancelled,
            dir_signature,
        }
    }
}

// ───────────────────────── Serial and parallel engines ─────────────────────────

fn scan_serial(
    root: &Path,
    options: &ScanOptions,
    cancel: &CancelToken,
    driver: &mut ScanDriver<'_>,
) -> ScanSummary {
    let mut seq: u64 = 0;
    let walker = build_walker(root, options);
    for entry in walker
        .into_iter()
        .filter_entry(|entry| keep_entry(entry, options))
    {
        if cancel.is_cancelled() {
            driver.cancelled = true;
            break;
        }
        match entry {
            Ok(entry) => {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.into_path();
                if !is_media_path(&path) {
                    continue;
                }
                let this_seq = seq;
                seq += 1;
                match driver.decide(&path, is_cue_path(&path), this_seq) {
                    StepPlan::Ready(step) => driver.emit_step(this_seq, step),
                    StepPlan::Parse(job) => {
                        // Same panic guard as the parallel path: a lofty
                        // panic on malformed input is a ledger entry, not an
                        // unwind into the caller.
                        match parse_guarded(&job.source, || parse_source(&job)) {
                            Ok(entries) => driver.emit_step(this_seq, StepResult::Parsed(entries)),
                            Err(failure) => driver.emit_step(this_seq, StepResult::Failed(failure)),
                        }
                    }
                }
            }
            Err(error) => driver.ledger_walk_error(&error, root),
        }
    }
    driver.finish(root)
}

#[derive(Default)]
struct QueueState {
    jobs: VecDeque<ParseJob>,
    discovery_done: bool,
}

/// Poison-tolerant lock: the guarded sections contain no user code, so a
/// panic inside one cannot corrupt the queue; recover the guard instead of
/// dying (worker death without an outcome would stall the drain loop).
fn lock_queue(queue: &(Mutex<QueueState>, Condvar)) -> std::sync::MutexGuard<'_, QueueState> {
    queue
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn worker_loop(
    queue: Arc<(Mutex<QueueState>, Condvar)>,
    sender: &mpsc::Sender<ParseOutcome>,
    cancel: &CancelToken,
) {
    loop {
        let job = {
            let cvar = &queue.1;
            let mut state = lock_queue(&queue);
            loop {
                if let Some(job) = state.jobs.pop_front() {
                    break Some(job);
                }
                if state.discovery_done {
                    break None;
                }
                state = cvar
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        let Some(job) = job else { return };
        if cancel.is_cancelled() {
            // Skipped without parsing; the outcome keeps the sequence
            // bookkeeping exact so the drain loop cannot deadlock.
            let _ = sender.send(ParseOutcome::Cancelled { seq: job.seq });
            continue;
        }
        let result = parse_guarded(&job.source, || parse_source(&job));
        let _ = sender.send(ParseOutcome::Done {
            seq: job.seq,
            result,
        });
    }
}

/// Runs one parse under a panic guard on BOTH delivery paths (serial inline
/// and parallel workers): a lofty panic on malformed input becomes a ledger
/// entry instead of unwinding into the caller's thread.
fn parse_guarded<T>(
    source: &Path,
    parse: impl FnOnce() -> std::result::Result<T, ScanFailure>,
) -> std::result::Result<T, ScanFailure> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(parse)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|message| (*message).to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        Err(ScanFailure {
            path: source.to_path_buf(),
            error: format!("parse panicked: {message}"),
            kind: ScanFailureKind::Parse,
        })
    })
}

fn scan_parallel(
    root: &Path,
    options: &ScanOptions,
    workers: usize,
    cancel: &CancelToken,
    driver: &mut ScanDriver<'_>,
) -> ScanSummary {
    let queue = Arc::new((Mutex::new(QueueState::default()), Condvar::new()));
    let capacity = (workers * 4).max(8);
    let mut in_flight: usize = 0;
    let mut seq: u64 = 0;
    let mut channel_closed = false;

    let walker = build_walker(root, options);
    let mut files = walker
        .into_iter()
        .filter_entry(|entry| keep_entry(entry, options));

    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let sender = sender.clone();
            scope.spawn(move || worker_loop(queue, &sender, cancel));
        }
        // Only the workers keep senders from here on: when the last worker
        // exits, recv() really can return Err, so the channel-closed branches
        // below are a genuine last-resort signal instead of dead code.
        drop(sender);
        // Discovery stays on the calling thread and lazy: reuse decisions are
        // immediate, parse jobs are queued with paths only.
        for entry in files.by_ref() {
            if cancel.is_cancelled() {
                driver.cancelled = true;
                break;
            }
            match entry {
                Ok(entry) => {
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let path = entry.into_path();
                    if !is_media_path(&path) {
                        continue;
                    }
                    let this_seq = seq;
                    seq += 1;
                    match driver.decide(&path, is_cue_path(&path), this_seq) {
                        StepPlan::Ready(step) => driver.emit_step(this_seq, step),
                        StepPlan::Parse(job) => {
                            while lock_queue(&queue).jobs.len() >= capacity {
                                match receiver.recv() {
                                    Ok(outcome) => {
                                        driver.handle_outcome(outcome);
                                        in_flight -= 1;
                                    }
                                    Err(_) => {
                                        channel_closed = true;
                                        break;
                                    }
                                }
                            }
                            if channel_closed {
                                driver.cancelled = true;
                                break;
                            }
                            {
                                let cvar = &queue.1;
                                lock_queue(&queue).jobs.push_back(job);
                                cvar.notify_one();
                            }
                            in_flight += 1;
                        }
                    }
                }
                Err(error) => driver.ledger_walk_error(&error, root),
            }
        }
        {
            let cvar = &queue.1;
            let mut state = lock_queue(&queue);
            state.discovery_done = true;
            drop(state);
            cvar.notify_all();
        }
        while in_flight > 0 && !channel_closed {
            match receiver.recv() {
                Ok(outcome) => {
                    driver.handle_outcome(outcome);
                    in_flight -= 1;
                }
                Err(_) => channel_closed = true,
            }
        }
    });
    driver.finish(root)
}

// ───────────────────────── Walk helpers ─────────────────────────

fn build_walker(root: &Path, options: &ScanOptions) -> walkdir::WalkDir {
    walkdir::WalkDir::new(root)
        .follow_links(options.symlinks == SymlinkPolicy::Follow)
        .max_depth(if options.max_depth == 0 {
            usize::MAX
        } else {
            options.max_depth as usize
        })
}

/// Depth 0 (the root) is never pruned; directories are pruned by name
/// (exclusion list), everything else only by the hidden switch.
fn keep_entry(entry: &walkdir::DirEntry, options: &ScanOptions) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if options.skip_hidden && entry.file_name().to_string_lossy().starts_with('.') {
        return false;
    }
    if entry.file_type().is_dir() {
        let name = entry.file_name().to_string_lossy();
        return !options
            .excluded_dirs
            .iter()
            .any(|excluded| *excluded == name);
    }
    true
}

fn is_media_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            let extension = extension.to_ascii_lowercase();
            extension == CUE_EXTENSION || AUDIO_EXTENSIONS.contains(&extension.as_str())
        })
}

fn is_cue_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(CUE_EXTENSION))
}

// ───────────────────────── Parsing and classification ─────────────────────────

fn classify_io(error: &std::io::Error) -> ScanFailureKind {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        ScanFailureKind::Permission
    } else {
        ScanFailureKind::Io
    }
}

fn classify_lofty(error: &lofty::error::LoftyError) -> ScanFailureKind {
    match error.kind() {
        lofty::error::ErrorKind::Io(io) => classify_io(io),
        _ => ScanFailureKind::Parse,
    }
}

fn parse_source(job: &ParseJob) -> std::result::Result<Vec<ScanEntry>, ScanFailure> {
    if job.is_cue {
        parse_cue_source(job)
    } else {
        match read_metadata_inner(&job.source) {
            Ok(metadata) => {
                // s6: the read layer finalizes between the raw lofty read
                // and entry assembly (LOCAL_ENHANCEMENT_DESIGN.md §3.9);
                // the scan core keeps the TagFirst default.
                let metadata =
                    finalize_metadata(metadata, &job.source, MetadataReadPolicy::TagFirst);
                let mut entry = audio_entry(&job.source, metadata, job.fingerprint);
                // s7 opt-in: the full lyric text lands under
                // extra[EXTRA_LYRICS] (LOCAL_ENHANCEMENT_DESIGN.md §3.10),
                // same sidecar-first face the standalone
                // `local::read_lyrics` uses. CUE entries skip this on
                // purpose (N tracks over one file-relative text).
                if job.read_lyrics
                    && let Some(lyrics) = read_lyrics(&job.source)
                {
                    entry.extra.insert(EXTRA_LYRICS.to_string(), lyrics);
                }
                // s8 opt-in (feature local-cover): the resolved cover path
                // lands under extra[EXTRA_COVER_CACHE_PATH]
                // (LOCAL_ENHANCEMENT_DESIGN.md §3.11), the same
                // `read_cover` face the standalone
                // `local_cover::read_cover` serves. A cache write failure
                // degrades to the external face or None and never fails
                // the parse; a path lands, image bytes never do (the s8
                // memory boundary).
                #[cfg(feature = "local-cover")]
                if let Some(cache_dir) = job.cover.cache_dir.as_deref()
                    && let Some(cover) =
                        crate::local_cover::read_cover(&job.source, cache_dir, job.cover.priority)
                {
                    entry.extra.insert(
                        EXTRA_COVER_CACHE_PATH.to_string(),
                        cover.to_string_lossy().into_owned(),
                    );
                }
                Ok(vec![entry])
            }
            Err(error) => Err(ScanFailure {
                path: job.source.clone(),
                error: error.to_string(),
                kind: classify_lofty(&error),
            }),
        }
    }
}

/// One entry for a plain audio file: source and playback path coincide.
fn audio_entry(source: &Path, metadata: LocalMetadata, fingerprint: FileFingerprint) -> ScanEntry {
    ScanEntry {
        source_path: source.to_path_buf(),
        play_path: source.to_path_buf(),
        container_track: None,
        start_ms: 0,
        metadata,
        fingerprint,
        reused: false,
        extra: file_extras(fingerprint),
    }
}

/// CUE source: one entry per logical track of the sheet. The base metadata
/// (audio spec and container duration) comes from the referenced audio file,
/// read once per distinct file; track title/artist/album come from the
/// container (formats_cue already applies the "Track NN" / unknown-artist /
/// album fallbacks, `formats_cue.rs:240-272`). Referenced audio paths are
/// resolved exactly like upstream and are NOT confined to the scan root (a
/// sheet can name `../` or absolute files); containment stays with the host.
fn parse_cue_source(job: &ParseJob) -> std::result::Result<Vec<ScanEntry>, ScanFailure> {
    let source = &job.source;
    let fingerprint = job.fingerprint;
    let tracks = CueHandler
        .inspect_tracks(source)
        .map_err(|error| ScanFailure {
            path: source.to_path_buf(),
            error: error.to_string(),
            kind: ScanFailureKind::Parse,
        })?;
    let mut bases: HashMap<PathBuf, LocalMetadata> = HashMap::new();
    // s8 opt-in (feature local-cover): one cover resolution per distinct
    // referenced audio file — every track of a sheet shares it, so the
    // resolution result is cached per audio path inside this job (jobs run
    // one at a time on their worker; no sharing between jobs).
    #[cfg(feature = "local-cover")]
    let mut covers: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut entries = Vec::with_capacity(tracks.len());
    for track in tracks {
        let base = match bases.get(&track.audio_path) {
            Some(base) => base.clone(),
            None => match read_metadata_inner(&track.audio_path) {
                Ok(base) => {
                    bases.insert(track.audio_path.clone(), base.clone());
                    base
                }
                Err(error) => {
                    return Err(ScanFailure {
                        path: source.to_path_buf(),
                        error: format!(
                            "读取 CUE 音频失败 ({}): {error}",
                            track.audio_path.display()
                        ),
                        kind: classify_lofty(&error),
                    });
                }
            },
        };
        // Mutated only under `local-cover`; the allow keeps plain-`local`
        // builds (e.g. clippy --features local-watch) warning-free.
        #[cfg_attr(not(feature = "local-cover"), allow(unused_mut))]
        let mut entry = cue_entry(source, track.clone(), base, fingerprint);
        #[cfg(feature = "local-cover")]
        if let Some(cache_dir) = job.cover.cache_dir.as_deref() {
            let cover = covers.entry(track.audio_path.clone()).or_insert_with(|| {
                crate::local_cover::read_cover(&track.audio_path, cache_dir, job.cover.priority)
                    .map(|cover| cover.to_string_lossy().into_owned())
            });
            if let Some(cover) = cover.clone() {
                entry
                    .extra
                    .insert(EXTRA_COVER_CACHE_PATH.to_string(), cover);
            }
        }
        entries.push(entry);
    }
    Ok(entries)
}

fn cue_entry(
    source: &Path,
    track: ContainerTrack,
    base: LocalMetadata,
    fingerprint: FileFingerprint,
) -> ScanEntry {
    let mut extra = file_extras(fingerprint);
    extra.insert(
        EXTRA_CUE_FILE.to_string(),
        source.to_string_lossy().into_owned(),
    );
    extra.insert(EXTRA_CUE_TRACK.to_string(), track.track_number.clone());
    extra.insert(EXTRA_CUE_START_MS.to_string(), track.start_ms.to_string());
    // The CUE face wins over the referenced audio's own tags (container
    // layer, upstream `scanner.rs:351-378`): title/artist/album come from
    // the sheet, whose fallbacks already ran in formats_cue. s6 item b
    // applies the same field normalization as the plain read layer (trim
    // plus whitespace collapse, never case/fullwidth folding).
    let artist = track.artists.join(", ");
    let metadata = LocalMetadata {
        title: normalize_text(track.title.as_deref()),
        artist: normalize_text(Some(&artist)),
        album: normalize_text(track.album.as_deref()),
        duration_ms: track.duration_ms,
        format: track
            .audio_path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase()),
        audio: base.audio,
    };
    ScanEntry {
        source_path: source.to_path_buf(),
        play_path: track.audio_path,
        container_track: Some(format!(
            "{}#{}",
            source.to_string_lossy(),
            track.track_number
        )),
        start_ms: track.start_ms,
        metadata,
        fingerprint,
        reused: false,
        extra,
    }
}

fn file_extras(fingerprint: FileFingerprint) -> BTreeMap<String, String> {
    BTreeMap::from([
        (EXTRA_FILE_SIZE.to_string(), fingerprint.size.to_string()),
        (
            EXTRA_FILE_MODIFIED_UNIX_NANOS.to_string(),
            fingerprint.modified_unix_nanos.to_string(),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AudioSpec;
    use std::fs;
    use std::time::Duration;

    /// Minimal PCM WAV stub: 44100 Hz, mono, 16-bit; lofty reports
    /// `{706, 44100, Some(16), lossless(Wav)}` (88200 B/s, same as the
    /// src/local.rs test stub).
    fn wav_bytes(duration_ms: u64) -> Vec<u8> {
        const SAMPLE_RATE: u32 = 44_100;
        const BYTES_PER_SECOND: u32 = SAMPLE_RATE * 2; // mono, 16-bit
        let data_len = (BYTES_PER_SECOND as u128 * duration_ms as u128 / 1000) as u32;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&BYTES_PER_SECOND.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(wav.len() + data_len as usize, 0);
        wav
    }

    /// Two-track sheet; the second INDEX 01 is at 2 s, so with a 5 s audio
    /// the track durations are 2000 ms and 3000 ms.
    const SCAN_SHEET: &str = "\
TITLE \"Scan Album\"\r\n\
PERFORMER \"Scan Artist\"\r\n\
FILE \"scan album.wav\" WAVE\r\n\
  TRACK 01 AUDIO\r\n\
    TITLE \"First Track\"\r\n\
    INDEX 01 00:00:00\r\n\
  TRACK 02 AUDIO\r\n\
    TITLE \"Second Track\"\r\n\
    INDEX 01 00:02:00\r\n\
";

    const CUE_AUDIO_MS: u64 = 5_000;

    struct ScanFixture {
        root: PathBuf,
    }
    impl ScanFixture {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("musid-dli-scan-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self {
                root: fs::canonicalize(dir).unwrap(),
            }
        }
        fn write(&self, rel: &str, bytes: &[u8]) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, bytes).unwrap();
        }
        fn wav(&self, rel: &str, duration_ms: u64) {
            self.write(rel, &wav_bytes(duration_ms));
        }
        fn small_wav(&self, rel: &str, duration_ms: u64) {
            self.write(rel, &small_wav_bytes(duration_ms));
        }
        fn write_text(&self, rel: &str, text: &str) {
            self.write(rel, text.as_bytes());
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }
    }
    impl Drop for ScanFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn run_scan(
        root: &Path,
        options: &ScanOptions,
        previous: &ScanState,
    ) -> (Vec<ScanEntry>, ScanSummary) {
        scan_entries(root, options, previous)
    }

    /// Folds entries into a previous-shaped state (entries only).
    fn fold_entries(entries: &[ScanEntry]) -> ScanState {
        let mut state = ScanState::default();
        for entry in entries {
            state.absorb(&ScanEvent::Entry(Box::new(entry.clone())));
        }
        state
    }

    /// Sorts entries by (source path, track identity) for order-insensitive
    /// comparisons.
    fn sorted(mut entries: Vec<ScanEntry>) -> Vec<ScanEntry> {
        entries.sort_by(|left, right| {
            left.source_path
                .cmp(&right.source_path)
                .then_with(|| left.container_track.cmp(&right.container_track))
        });
        entries
    }

    fn strip_reused(entries: &[ScanEntry]) -> Vec<ScanEntry> {
        entries
            .iter()
            .map(|entry| {
                let mut entry = entry.clone();
                entry.reused = false;
                entry
            })
            .collect()
    }

    fn cue_track_id(cue: &Path, number: &str) -> String {
        format!("{}#{number}", cue.to_string_lossy())
    }

    #[test]
    fn excluded_dirs_are_pruned_by_name_and_extendable() {
        let fx = ScanFixture::new("excluded");
        fx.wav("node_modules/song.wav", 100);
        fx.wav(".git/song.wav", 100);
        fx.wav(".stfolder/song.wav", 100);
        fx.wav(".Trash/song.wav", 100);
        fx.wav("music/song.wav", 100);

        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(summary.stats.discovered, 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_path, fx.path("music/song.wav"));

        let mut options = ScanOptions::default();
        options.excluded_dirs.push("music".to_string());
        let (entries, summary) = run_scan(&fx.root, &options, &ScanState::default());
        assert_eq!(summary.stats.discovered, 0);
        assert!(entries.is_empty());
    }

    #[test]
    fn hidden_entries_follow_the_skip_hidden_switch() {
        let fx = ScanFixture::new("hidden");
        fx.wav(".hidden.wav", 100);
        fx.wav(".hid/song.wav", 100);

        // Default keeps the upstream/legacy behavior: hidden files are scanned.
        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(summary.stats.discovered, 2);
        assert_eq!(entries.len(), 2);

        let options = ScanOptions {
            skip_hidden: true,
            ..ScanOptions::default()
        };
        let (entries, summary) = run_scan(&fx.root, &options, &ScanState::default());
        assert_eq!(summary.stats.discovered, 0);
        assert!(entries.is_empty());
    }

    #[test]
    fn max_depth_zero_is_unlimited_and_one_is_shallow() {
        let fx = ScanFixture::new("depth");
        fx.wav("top.wav", 100);
        fx.wav("nested/deep.wav", 100);

        let shallow = ScanOptions {
            max_depth: 1,
            ..ScanOptions::default()
        };
        let (entries, summary) = run_scan(&fx.root, &shallow, &ScanState::default());
        assert_eq!(summary.stats.discovered, 1);
        assert_eq!(entries[0].source_path, fx.path("top.wav"));

        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(summary.stats.discovered, 2);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    #[cfg(unix)]
    fn symlink_policy_skips_by_default_and_follows_when_asked() {
        use std::os::unix::fs::symlink;
        let fx = ScanFixture::new("symlink");
        fx.wav("real/song.wav", 100);
        symlink(fx.path("real/song.wav"), fx.path("link.wav")).unwrap();

        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(summary.stats.discovered, 1);
        assert!(
            entries
                .iter()
                .all(|entry| !entry.source_path.ends_with("link.wav"))
        );

        let follow = ScanOptions {
            symlinks: SymlinkPolicy::Follow,
            ..ScanOptions::default()
        };
        let (entries, summary) = run_scan(&fx.root, &follow, &ScanState::default());
        assert_eq!(summary.stats.discovered, 2);
        assert!(
            entries
                .iter()
                .any(|entry| entry.source_path.ends_with("link.wav"))
        );
    }

    /// Flips read bits off; returns false when the environment ignores the
    /// change (for example when running as root) so the test can bow out.
    #[cfg(unix)]
    fn deny_read(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o000);
        !(fs::set_permissions(path, permissions).is_err() || fs::read(path).is_ok())
    }

    #[cfg(unix)]
    fn restore_read(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_file_lands_in_the_ledger_without_aborting() {
        let fx = ScanFixture::new("perm-file");
        fx.wav("locked.wav", 100);
        fx.wav("open.wav", 100);
        if !deny_read(&fx.path("locked.wav")) {
            restore_read(&fx.path("locked.wav"));
            return;
        }

        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        restore_read(&fx.path("locked.wav"));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_path, fx.path("open.wav"));
        assert_eq!(summary.stats.failed, 1);
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.failures[0].path, fx.path("locked.wav"));
        assert_eq!(summary.failures[0].kind, ScanFailureKind::Permission);
    }

    #[test]
    fn corrupt_files_land_in_the_ledger_without_aborting() {
        let fx = ScanFixture::new("corrupt");
        fx.write("broken.mp3", b"definitely not audio");
        fx.write("broken.flac", b"definitely not audio");
        fx.wav("good.wav", 100);

        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());

        // discovered counts every stat-able media file, failed files included
        assert_eq!(summary.stats.discovered, 3);
        assert_eq!(summary.stats.parsed, 1);
        assert_eq!(summary.stats.failed, 2);
        assert_eq!(summary.failures.len(), 2);
        // Empirical lofty classification (probed 2026-11-20): garbage MPEG
        // bytes end the frame-sync search with a raw io error, so an .mp3
        // surfaces as Io; containers without their magic (.flac here) are
        // Parse. Both classes are ledger entries, neither aborts the scan.
        assert!(
            summary
                .failures
                .iter()
                .any(|failure| failure.kind == ScanFailureKind::Io
                    && failure.path.ends_with("broken.mp3"))
        );
        assert!(
            summary
                .failures
                .iter()
                .any(|failure| failure.kind == ScanFailureKind::Parse
                    && failure.path.ends_with("broken.flac"))
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_path, fx.path("good.wav"));
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_subdirectory_lands_in_the_ledger() {
        let fx = ScanFixture::new("perm-dir");
        fx.wav("vault/locked.wav", 100);
        fx.wav("open.wav", 100);
        if !deny_read(&fx.path("vault")) {
            restore_read(&fx.path("vault"));
            return;
        }

        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        restore_read(&fx.path("vault"));

        assert_eq!(summary.stats.failed, 1);
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.failures[0].path, fx.path("vault"));
        assert_eq!(summary.failures[0].kind, ScanFailureKind::Permission);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_path, fx.path("open.wav"));
    }

    #[test]
    fn events_stream_entry_by_entry_and_the_summary_carries_no_entries() {
        let fx = ScanFixture::new("stream");
        for index in 0..24 {
            fx.wav(&format!("d{index}/song{index}.wav"), 100);
        }

        let mut events = 0;
        let mut control = ScanControl::default();
        let summary = scan_stream(
            &fx.root,
            &ScanOptions::default(),
            &ScanState::default(),
            &mut |event| {
                events += 1;
                assert!(matches!(event, ScanEvent::Entry(_) | ScanEvent::Failure(_)));
            },
            &mut control,
        );

        assert_eq!(events, 24);
        assert!(summary.failures.is_empty());
        assert_eq!(summary.stats.discovered, 24);
        assert_eq!(summary.stats.parsed, 24);
    }

    #[test]
    fn cue_source_yields_dual_path_container_entries() {
        let fx = ScanFixture::new("cue-scan");
        fx.wav("scan album.wav", CUE_AUDIO_MS);
        fx.write_text("album.cue", SCAN_SHEET);

        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());

        // the sheet and its referenced audio are discovered separately
        assert_eq!(summary.stats.discovered, 2);
        assert_eq!(summary.stats.parsed, 2);
        assert_eq!(entries.len(), 3);

        let cue = fx.path("album.cue");
        let audio = fx.path("scan album.wav");
        let mut tracks: Vec<ScanEntry> = entries
            .iter()
            .filter(|entry| entry.container_track.is_some())
            .cloned()
            .collect();
        tracks.sort_by(|left, right| left.container_track.cmp(&right.container_track));
        assert_eq!(tracks.len(), 2);

        let cue_fingerprint = FileFingerprint::from_path(&cue).unwrap();
        let first = &tracks[0];
        assert_eq!(first.source_path, cue);
        assert_eq!(first.play_path, audio);
        assert_eq!(
            first.container_track.as_deref(),
            Some(cue_track_id(&cue, "01").as_str())
        );
        assert_eq!(first.start_ms, 0);
        assert_eq!(first.metadata.duration_ms, 2_000);
        assert_eq!(first.metadata.title.as_deref(), Some("First Track"));
        assert_eq!(first.metadata.artist.as_deref(), Some("Scan Artist"));
        assert_eq!(first.metadata.album.as_deref(), Some("Scan Album"));
        assert_eq!(first.metadata.format.as_deref(), Some("wav"));
        assert_eq!(
            first.metadata.audio,
            Some(AudioSpec {
                bitrate: Some(706),
                sample_rate: Some(44100),
                bit_depth: Some(16),
                lossless: true,
            })
        );
        assert_eq!(first.fingerprint, cue_fingerprint);
        assert_eq!(
            first.extra.get(EXTRA_CUE_FILE).unwrap(),
            &cue.to_string_lossy().into_owned()
        );
        assert_eq!(first.extra.get(EXTRA_CUE_TRACK).unwrap(), "01");
        assert_eq!(first.extra.get(EXTRA_CUE_START_MS).unwrap(), "0");
        assert_eq!(
            first.extra.get(EXTRA_FILE_SIZE).unwrap(),
            &cue_fingerprint.size.to_string()
        );
        assert_eq!(
            first.extra.get(EXTRA_FILE_MODIFIED_UNIX_NANOS).unwrap(),
            &cue_fingerprint.modified_unix_nanos.to_string()
        );

        let second = &tracks[1];
        assert_eq!(
            second.container_track.as_deref(),
            Some(cue_track_id(&cue, "02").as_str())
        );
        assert_eq!(second.start_ms, 2_000);
        assert_eq!(second.metadata.duration_ms, 3_000);
        assert_eq!(second.metadata.title.as_deref(), Some("Second Track"));

        // the referenced audio also keeps its own plain full-track entry
        let plain: Vec<&ScanEntry> = entries
            .iter()
            .filter(|entry| entry.container_track.is_none())
            .collect();
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].source_path, audio);
        assert_eq!(plain[0].play_path, audio);
        assert_eq!(plain[0].metadata.duration_ms, CUE_AUDIO_MS);
    }

    #[test]
    fn cue_with_missing_referenced_audio_is_a_ledger_failure() {
        let fx = ScanFixture::new("cue-missing");
        fx.write_text("album.cue", SCAN_SHEET); // scan album.wav is absent
        fx.wav("other.wav", 100);

        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());

        assert_eq!(summary.stats.discovered, 2);
        assert_eq!(summary.stats.failed, 1);
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.failures[0].path, fx.path("album.cue"));
        assert_eq!(summary.failures[0].kind, ScanFailureKind::Parse);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_path, fx.path("other.wav"));
    }

    // ───────────── committed fixture corpus (tests/fixtures/cue, the
    // formats_cue vector batch) scanned as scan products ─────────────

    const FIXTURE_UTF8: &[u8] = include_bytes!("../tests/fixtures/cue/utf8_basic.cue");
    const FIXTURE_GBK: &[u8] = include_bytes!("../tests/fixtures/cue/gbk_sheet.cue");
    const FIXTURE_LEGACY: &[u8] = include_bytes!("../tests/fixtures/cue/legacy_sheet.cue");
    const FIXTURE_MULTI: &[u8] = include_bytes!("../tests/fixtures/cue/multi_file.cue");
    const FIXTURE_CATALOG_OK: &[u8] = include_bytes!("../tests/fixtures/cue/catalog_ok.cue");
    const FIXTURE_CATALOG_SHORT: &[u8] = include_bytes!("../tests/fixtures/cue/catalog_short.cue");
    const FIXTURE_INDEX_RANGE: &[u8] =
        include_bytes!("../tests/fixtures/cue/index_seconds_out_of_range.cue");
    const FIXTURE_MISSING_AUDIO: &[u8] = include_bytes!("../tests/fixtures/cue/missing_audio.cue");

    /// 1000 Hz mono 16-bit PCM stub (2000 B/s), the formats_cue fixture
    /// recipe: the multi-minute fixture audio stays a few hundred kilobytes.
    fn small_wav_bytes(duration_ms: u64) -> Vec<u8> {
        const BYTES_PER_SECOND: u32 = 2_000;
        let data_len = (duration_ms as u128 * BYTES_PER_SECOND as u128 / 1000) as u32;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&1_000u32.to_le_bytes()); // sample rate
        wav.extend_from_slice(&BYTES_PER_SECOND.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(wav.len() + data_len as usize, 0);
        wav
    }

    /// The measured spec of [`small_wav_bytes`] (16 kb/s, 1000 Hz, 16-bit,
    /// lossless by container).
    const SMALL_WAV_SPEC: AudioSpec = AudioSpec {
        bitrate: Some(16),
        sample_rate: Some(1000),
        bit_depth: Some(16),
        lossless: true,
    };

    /// Track entries of one CUE source, ordered by container identity.
    fn cue_tracks_of<'a>(entries: &'a [ScanEntry], cue: &Path) -> Vec<&'a ScanEntry> {
        let mut tracks: Vec<&ScanEntry> = entries
            .iter()
            .filter(|entry| entry.container_track.is_some() && entry.source_path == cue)
            .collect();
        tracks.sort_by(|left, right| left.container_track.cmp(&right.container_track));
        tracks
    }

    #[test]
    fn fixture_cue_corpus_scans_to_the_pinned_track_tables() {
        let fx = ScanFixture::new("cue-fixtures");
        // Six scannable sheets, each in its own subdirectory (sheets share
        // referenced-audio names). Audio durations mirror the formats_cue
        // vectors so their pinned track tables apply verbatim through the
        // scan pipeline.
        fx.write("utf8/utf8_basic.cue", FIXTURE_UTF8);
        fx.small_wav("utf8/album one.wav", 400_000);
        fx.write("gbk/gbk_sheet.cue", FIXTURE_GBK);
        fx.small_wav("gbk/album gbk.wav", 300_000);
        fx.write("legacy/legacy_sheet.cue", FIXTURE_LEGACY);
        fx.small_wav("legacy/legacy album.wav", 600_000);
        fx.write("multi/multi_file.cue", FIXTURE_MULTI);
        fx.small_wav("multi/side-a.wav", 400_000);
        fx.small_wav("multi/side-b.wav", 200_000);
        fx.write("catalog-ok/catalog_ok.cue", FIXTURE_CATALOG_OK);
        fx.small_wav("catalog-ok/catalog album.wav", 120_000);
        fx.write("catalog-short/catalog_short.cue", FIXTURE_CATALOG_SHORT);
        fx.small_wav("catalog-short/catalog album.wav", 120_000);

        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());

        // 6 sheets + 7 audio files, all parsed, nothing failed
        assert_eq!(
            summary.stats,
            LocalScanStats {
                discovered: 13,
                reused: 0,
                parsed: 13,
                failed: 0,
                removed: 0,
            }
        );
        assert_eq!(entries.len(), 20); // 7 plain audio + 13 container tracks
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.container_track.is_none())
                .count(),
            7
        );

        // utf8_basic: crate path, positional numbering, escape decoding,
        // track-performer fallback, spec measured from the referenced audio
        let cue = fx.path("utf8/utf8_basic.cue");
        let audio = fx.path("utf8/album one.wav");
        let tracks = cue_tracks_of(&entries, &cue);
        assert_eq!(tracks.len(), 3);
        let starts = [0, 62_493, 210_986];
        let durations = [62_493, 148_493, 189_013];
        let titles = [
            Some("First \"quoted\" title"),
            Some("Back\\slash path"),
            Some("Track 03"), // empty TITLE falls back
        ];
        let artists = [Some("Various"), Some("Track Artist"), Some("Various")];
        for (index, track) in tracks.iter().enumerate() {
            assert_eq!(track.source_path, cue);
            assert_eq!(track.play_path, audio);
            assert_eq!(
                track.container_track.as_deref(),
                Some(cue_track_id(&cue, &format!("{:02}", index + 1)).as_str())
            );
            assert_eq!(track.start_ms, starts[index]);
            assert_eq!(track.metadata.duration_ms, durations[index]);
            assert_eq!(track.metadata.title.as_deref(), titles[index]);
            assert_eq!(track.metadata.artist.as_deref(), artists[index]);
            assert_eq!(track.metadata.album.as_deref(), Some("Escape Test Album"));
            assert_eq!(track.metadata.format.as_deref(), Some("wav"));
            assert_eq!(track.metadata.audio, Some(SMALL_WAV_SPEC));
            assert_eq!(
                track.extra.get(EXTRA_CUE_FILE).unwrap(),
                &cue.to_string_lossy().into_owned()
            );
            assert_eq!(
                track.extra.get(EXTRA_CUE_TRACK).unwrap(),
                &format!("{:02}", index + 1)
            );
            assert_eq!(
                track.extra.get(EXTRA_CUE_START_MS).unwrap(),
                &starts[index].to_string()
            );
        }

        // gbk_sheet: GB18030 fallback decode through the scan pipeline
        let cue = fx.path("gbk/gbk_sheet.cue");
        let tracks = cue_tracks_of(&entries, &cue);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].metadata.title.as_deref(), Some("第一軌"));
        assert_eq!(tracks[1].metadata.title.as_deref(), Some("第二軌"));
        assert_eq!(tracks[0].metadata.album.as_deref(), Some("天賦專輯"));
        assert_eq!(tracks[1].metadata.artist, Some("測試歌手".to_string()));
        assert_eq!(tracks[1].start_ms, 120_493);
        assert_eq!(tracks[1].metadata.duration_ms, 179_506);
        assert_eq!(tracks[0].play_path, fx.path("gbk/album gbk.wav"));

        // legacy_sheet: legacy line parser, verbatim tokens, INDEX 00
        // ignored, stem-fallback album, unknown-artist default
        let cue = fx.path("legacy/legacy_sheet.cue");
        let tracks = cue_tracks_of(&entries, &cue);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].start_ms, 493);
        assert_eq!(tracks[0].metadata.duration_ms, 251_640);
        assert_eq!(tracks[1].start_ms, 252_133);
        assert_eq!(tracks[1].metadata.duration_ms, 347_866);
        assert_eq!(
            tracks[0].container_track.as_deref(),
            Some(cue_track_id(&cue, "01").as_str())
        );
        assert_eq!(tracks[0].metadata.title.as_deref(), Some("Track 01"));
        assert_eq!(tracks[0].metadata.artist.as_deref(), Some("未知艺术家"));
        assert_eq!(tracks[0].metadata.album.as_deref(), Some("legacy_sheet"));

        // multi_file: each track plays the audio its FILE names
        let cue = fx.path("multi/multi_file.cue");
        let tracks = cue_tracks_of(&entries, &cue);
        assert_eq!(tracks.len(), 4);
        assert_eq!(tracks[0].play_path, fx.path("multi/side-a.wav"));
        assert_eq!(tracks[1].play_path, fx.path("multi/side-a.wav"));
        assert_eq!(tracks[2].play_path, fx.path("multi/side-b.wav"));
        assert_eq!(tracks[3].play_path, fx.path("multi/side-b.wav"));
        assert_eq!(tracks[0].start_ms, 0);
        assert_eq!(tracks[0].metadata.duration_ms, 150_000);
        assert_eq!(tracks[1].start_ms, 150_000);
        assert_eq!(tracks[1].metadata.duration_ms, 250_000);
        assert_eq!(tracks[2].start_ms, 133);
        assert_eq!(tracks[2].metadata.duration_ms, 60_360);
        assert_eq!(tracks[3].start_ms, 60_493);
        assert_eq!(tracks[3].metadata.duration_ms, 139_506);

        // catalog_ok (13-digit CATALOG) takes the crate path: positional
        // "01"; catalog_short is rejected by the crate and re-parsed by the
        // legacy parser: the declared TRACK 07 token survives verbatim
        let ok = fx.path("catalog-ok/catalog_ok.cue");
        let tracks = cue_tracks_of(&entries, &ok);
        assert_eq!(tracks.len(), 1);
        assert_eq!(
            tracks[0].container_track.as_deref(),
            Some(cue_track_id(&ok, "01").as_str())
        );
        assert_eq!(tracks[0].metadata.title.as_deref(), Some("Only Track"));
        assert_eq!(tracks[0].start_ms, 60_000);
        assert_eq!(tracks[0].metadata.duration_ms, 60_000);
        assert_eq!(tracks[0].play_path, fx.path("catalog-ok/catalog album.wav"));

        let short = fx.path("catalog-short/catalog_short.cue");
        let tracks = cue_tracks_of(&entries, &short);
        assert_eq!(tracks.len(), 1);
        assert_eq!(
            tracks[0].container_track.as_deref(),
            Some(cue_track_id(&short, "07").as_str())
        );
        assert_eq!(tracks[0].metadata.title.as_deref(), Some("Only Track"));

        // one key per source: every sheet groups all of its tracks
        let state = fold_entries(&entries);
        assert_eq!(state.entries.len(), 13); // 6 cue keys + 7 audio keys
        assert_eq!(
            state
                .entries
                .get(&fx.path("multi/multi_file.cue"))
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            state
                .entries
                .get(&fx.path("utf8/utf8_basic.cue"))
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn fixture_cue_failure_sheets_land_in_the_ledger_without_aborting() {
        let fx = ScanFixture::new("cue-fixture-failures");
        fx.write(
            "index-range/index_seconds_out_of_range.cue",
            FIXTURE_INDEX_RANGE,
        );
        fx.small_wav("index-range/album one.wav", 400_000); // the audio is fine
        fx.write("missing/missing_audio.cue", FIXTURE_MISSING_AUDIO); // no audio
        fx.wav("healthy.wav", 100); // an unrelated file keeps flowing

        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());

        // 2 sheets + 2 audio files; the audio parsed, both sheets failed
        assert_eq!(
            summary.stats,
            LocalScanStats {
                discovered: 4,
                reused: 0,
                parsed: 2,
                failed: 2,
                removed: 0,
            }
        );
        assert_eq!(summary.failures.len(), 2);

        // strict INDEX grammar (01:99:00) fails the sheet as a whole — no
        // silent partial split and no legacy re-parse
        let out_of_range = summary
            .failures
            .iter()
            .find(|failure| failure.path == fx.path("index-range/index_seconds_out_of_range.cue"))
            .unwrap();
        assert_eq!(out_of_range.kind, ScanFailureKind::Parse);
        assert!(
            out_of_range.error.contains("INDEX 01 无效"),
            "{}",
            out_of_range.error
        );

        // missing referenced audio fails the whole sheet as well
        let missing = summary
            .failures
            .iter()
            .find(|failure| failure.path == fx.path("missing/missing_audio.cue"))
            .unwrap();
        assert_eq!(missing.kind, ScanFailureKind::Parse);
        assert!(
            missing.error.contains("读取 CUE 音频失败"),
            "{}",
            missing.error
        );

        // the scan continued: both audio files produced plain entries and no
        // cue produced any track
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.container_track.is_none()));
    }

    #[test]
    fn unchanged_sources_reuse_previous_entries_without_reparsing() {
        let fx = ScanFixture::new("reuse");
        fx.wav("one.wav", 100);
        fx.wav("two.wav", 100);

        let (first, first_summary) =
            run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(first_summary.stats.parsed, 2);
        let previous = fold_entries(&first);

        let (second, second_summary) = run_scan(&fx.root, &ScanOptions::default(), &previous);
        assert_eq!(second_summary.stats.parsed, 0);
        assert_eq!(second_summary.stats.reused, 2);
        assert_eq!(second_summary.stats.discovered, 2);
        assert!(second.iter().all(|entry| entry.reused));
        assert_eq!(strip_reused(&second), strip_reused(&first));
    }

    #[test]
    fn in_place_edit_of_the_same_size_reparses_via_the_fingerprint() {
        let fx = ScanFixture::new("inplace");
        fx.wav("song.wav", 100);
        let (first, _) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());

        let path = fx.path("song.wav");
        let original_size = fs::metadata(&path).unwrap().len();
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&path, &bytes).unwrap();
        // same size on purpose: only the mtime nanos move
        assert_eq!(fs::metadata(&path).unwrap().len(), original_size);
        std::thread::sleep(Duration::from_millis(2));

        let previous = fold_entries(&first);
        let (second, summary) = run_scan(&fx.root, &ScanOptions::default(), &previous);
        assert_eq!(summary.stats.parsed, 1);
        assert_eq!(summary.stats.reused, 0);
        assert!(!second[0].reused);
        assert_ne!(second[0].fingerprint, first[0].fingerprint);
    }

    #[test]
    fn dir_signature_fast_path_requires_force_after_in_place_edits() {
        let fx = ScanFixture::new("force");
        fx.wav("song.wav", 100);

        let (first, first_summary) =
            run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        let signature = first_summary
            .dir_signature
            .expect("walked root records a signature");
        let mut state = fold_entries(&first);
        state.set_dir_signature(&fx.root, signature);

        // in-place edit: the file fingerprint moves, the directory mtime does not
        let path = fx.path("song.wav");
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&path, &bytes).unwrap();
        std::thread::sleep(Duration::from_millis(2));

        // without force the fast path reuses the stale entries wholesale
        let (stale, stale_summary) = run_scan(&fx.root, &ScanOptions::default(), &state);
        assert_eq!(stale_summary.stats.reused, 1);
        assert_eq!(stale_summary.stats.parsed, 0);
        assert_eq!(stale[0].fingerprint, first[0].fingerprint);

        // force bypasses the fast path and the file fingerprint catches the edit
        let forced = ScanOptions {
            force: true,
            ..ScanOptions::default()
        };
        let (fresh, fresh_summary) = run_scan(&fx.root, &forced, &state);
        assert_eq!(fresh_summary.stats.parsed, 1);
        assert_eq!(fresh_summary.stats.reused, 0);
        assert_ne!(fresh[0].fingerprint, first[0].fingerprint);
        assert_eq!(
            fresh[0].fingerprint,
            FileFingerprint::from_path(&path).unwrap()
        );
    }

    #[test]
    fn removed_files_are_counted_against_the_previous_slice() {
        let fx = ScanFixture::new("removed");
        fx.wav("keep.wav", 100);
        fx.wav("gone.wav", 100);

        let (first, _) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        let previous = fold_entries(&first);

        fs::remove_file(fx.path("gone.wav")).unwrap();
        let (second, summary) = run_scan(&fx.root, &ScanOptions::default(), &previous);
        assert_eq!(summary.stats.removed, 1);
        assert_eq!(summary.stats.reused, 1);
        assert_eq!(second.len(), 1);
        assert!(
            !second
                .iter()
                .any(|entry| entry.source_path == fx.path("gone.wav"))
        );
    }

    #[test]
    fn cue_tracks_reuse_as_a_group_without_displacing_the_plain_entry() {
        let fx = ScanFixture::new("cue-reuse");
        fx.wav("scan album.wav", CUE_AUDIO_MS);
        fx.write_text("album.cue", SCAN_SHEET);

        let (first, _) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(first.len(), 3);
        let previous = fold_entries(&first);

        let (second, summary) = run_scan(&fx.root, &ScanOptions::default(), &previous);
        assert_eq!(summary.stats.parsed, 0);
        assert_eq!(summary.stats.reused, 3);
        let tracks = second
            .iter()
            .filter(|entry| entry.container_track.is_some())
            .count();
        assert_eq!(tracks, 2);
        let plain = second
            .iter()
            .filter(|entry| entry.container_track.is_none())
            .count();
        assert_eq!(plain, 1);
    }

    #[test]
    fn scan_stats_count_media_files_including_failures_and_cue() {
        let fx = ScanFixture::new("stats");
        fx.wav("scan album.wav", CUE_AUDIO_MS);
        fx.write_text("album.cue", SCAN_SHEET);
        fx.write("broken.mp3", b"junk bytes");

        let (_, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(
            summary.stats,
            LocalScanStats {
                discovered: 3,
                reused: 0,
                parsed: 2,
                failed: 1,
                removed: 0,
            }
        );
    }

    #[test]
    fn cancel_before_the_scan_produces_an_empty_cancelled_summary() {
        let fx = ScanFixture::new("cancel-pre");
        fx.wav("song.wav", 100);
        let token = CancelToken::new();
        token.cancel();

        let mut control = ScanControl {
            cancel: Some(&token),
            ..ScanControl::default()
        };
        let summary = scan_stream(
            &fx.root,
            &ScanOptions::default(),
            &ScanState::default(),
            &mut |_event| panic!("a pre-cancelled scan must not emit"),
            &mut control,
        );

        assert!(summary.cancelled);
        assert_eq!(summary.stats, LocalScanStats::default());
        assert!(summary.failures.is_empty());
        assert!(summary.dir_signature.is_none());
    }

    #[test]
    fn cancel_midway_keeps_completed_work_and_skips_removed_accounting() {
        let fx = ScanFixture::new("cancel-mid");
        fx.wav("a.wav", 100);
        fx.wav("b.wav", 100);
        fx.wav("c.wav", 100);
        let (first, _) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        let previous = fold_entries(&first);
        fs::remove_file(fx.path("c.wav")).unwrap();

        let token = CancelToken::new();
        let mut entries_seen = 0;
        let mut control = ScanControl {
            cancel: Some(&token),
            ..ScanControl::default()
        };
        // The token flips inside the first entry event while one more file is
        // still waiting to be discovered, so the engine stops mid-walk.
        let summary = scan_stream(
            &fx.root,
            &ScanOptions::default(),
            &previous,
            &mut |event| {
                if let ScanEvent::Entry(_) = event {
                    entries_seen += 1;
                    token.cancel();
                }
            },
            &mut control,
        );

        assert!(summary.cancelled);
        assert_eq!(entries_seen, 1);
        assert_eq!(summary.stats.discovered, 1);
        assert_eq!(summary.stats.parsed + summary.stats.reused, 1);
        // a cancelled run has an incomplete view: removed stays 0 even though
        // c.wav is gone (the deletion was never observed this run)
        assert_eq!(summary.stats.removed, 0);
        assert!(summary.dir_signature.is_none());
    }

    #[test]
    fn progress_snapshots_track_the_stream_counters() {
        use std::cell::Cell;
        let fx = ScanFixture::new("progress");
        fx.wav("one.wav", 100);
        fx.write("bad.mp3", b"junk");
        fx.wav("two.wav", 100);

        let last = Cell::new(ScanProgress::default());
        let snapshots = Cell::new(0);
        let mut control = ScanControl {
            progress: Some(&|progress| {
                last.set(progress);
                snapshots.set(snapshots.get() + 1);
            }),
            ..ScanControl::default()
        };
        let summary = scan_stream(
            &fx.root,
            &ScanOptions::default(),
            &ScanState::default(),
            &mut |_event| {},
            &mut control,
        );

        assert!(snapshots.get() > 0);
        assert_eq!(
            last.get(),
            ScanProgress {
                discovered: summary.stats.discovered,
                parsed: summary.stats.parsed,
                failed: summary.stats.failed,
                reused: summary.stats.reused,
            }
        );
        assert_eq!(summary.stats.discovered, 3);
        assert_eq!(summary.stats.parsed, 2);
        assert_eq!(summary.stats.failed, 1);
    }

    #[test]
    fn parallel_workers_match_the_serial_result_in_both_delivery_orders() {
        let fx = ScanFixture::new("parallel");
        for index in 0..24 {
            fx.wav(&format!("batch{}/song{index}.wav", index % 4), 100);
        }

        let (serial_entries, serial_summary) =
            run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());

        let ordered = ScanOptions {
            max_parse_workers: 4,
            ..ScanOptions::default()
        };
        let (parallel_entries, parallel_summary) =
            run_scan(&fx.root, &ordered, &ScanState::default());
        assert_eq!(parallel_summary.stats, serial_summary.stats);
        assert_eq!(parallel_entries, serial_entries);

        let unordered = ScanOptions {
            max_parse_workers: 4,
            ordered_output: false,
            ..ScanOptions::default()
        };
        let (unordered_entries, unordered_summary) =
            run_scan(&fx.root, &unordered, &ScanState::default());
        assert_eq!(unordered_summary.stats, serial_summary.stats);
        assert_eq!(sorted(unordered_entries), sorted(serial_entries));
    }

    /// §6.2 memory shape at scale: 500 files across nested directories drive
    /// the parallel pipeline far past the bounded queue capacity, so the
    /// blocking-drain branch and the reorder buffer run for real. The
    /// library core still hands out exactly one entry per event and keeps
    /// nothing (the summary is stats + ledger only, never entries), the
    /// workers=4 ordered product is byte-equal to the serial one, and
    /// unordered delivery changes only the order, not the set.
    #[test]
    fn large_synthetic_tree_keeps_the_streaming_memory_shape() {
        let fx = ScanFixture::new("memory-shape");
        const ALBUMS: usize = 10;
        const TRACKS: usize = 50;
        let total = ALBUMS * TRACKS;
        for album in 0..ALBUMS {
            for track in 0..TRACKS {
                fx.small_wav(&format!("album{album:02}/track{track:02}.wav"), 100);
            }
        }

        let (serial_entries, serial_summary) =
            run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(serial_summary.stats.discovered, total);
        assert_eq!(serial_summary.stats.parsed, total);

        let ordered = ScanOptions {
            max_parse_workers: 4,
            ..ScanOptions::default()
        };
        let mut delivered = Vec::new();
        let mut control = ScanControl::default();
        let summary = scan_stream(
            &fx.root,
            &ordered,
            &ScanState::default(),
            &mut |event| match event {
                ScanEvent::Entry(entry) => delivered.push(*entry),
                ScanEvent::Failure(_) => panic!("a clean synthetic tree must not fail"),
            },
            &mut control,
        );
        assert!(!summary.cancelled);
        assert!(summary.dir_signature.is_some());
        assert_eq!(summary.stats, serial_summary.stats);
        assert_eq!(summary.failures.len(), 0);
        assert_eq!(delivered.len(), total);
        assert_eq!(delivered, serial_entries);

        let unordered = ScanOptions {
            max_parse_workers: 4,
            ordered_output: false,
            ..ScanOptions::default()
        };
        let (unordered_entries, unordered_summary) =
            run_scan(&fx.root, &unordered, &ScanState::default());
        assert_eq!(unordered_summary.stats, serial_summary.stats);
        assert_eq!(sorted(unordered_entries), sorted(serial_entries));
    }

    #[test]
    fn scan_state_helpers_key_by_source_path() {
        let fx = ScanFixture::new("state");
        fx.wav("a.wav", 100);
        fx.wav("sub/b.wav", 100);
        let (entries, _) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        let mut state = fold_entries(&entries);

        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.entries_under(&fx.root).count(), 2);
        let sub_root = fx.path("sub");
        let under_sub: Vec<&PathBuf> = state
            .entries_under(&sub_root)
            .map(|(source, _)| source)
            .collect();
        assert_eq!(under_sub.len(), 1);
        assert_eq!(under_sub[0], &fx.path("sub/b.wav"));
        assert_eq!(
            state.fingerprint_of(&fx.path("a.wav")),
            Some(FileFingerprint::from_path(&fx.path("a.wav")).unwrap())
        );
        assert_eq!(state.fingerprint_of(&fx.path("nope.wav")), None);

        // failures are not absorbed
        state.absorb(&ScanEvent::Failure(ScanFailure {
            path: fx.path("a.wav"),
            error: "boom".to_string(),
            kind: ScanFailureKind::Parse,
        }));
        assert_eq!(state.entries.len(), 2);

        state.set_dir_signature(&fx.root, DirSignature::from_path(&fx.root).unwrap());
        assert!(state.dir_signatures.contains_key(&fx.root));
    }

    #[test]
    fn apply_summary_folds_or_invalidates_by_walk_completeness() {
        let fx = ScanFixture::new("apply-summary");
        let signature = DirSignature::from_path(&fx.root).unwrap();

        // clean walk: signature folded
        let mut state = ScanState::default();
        let clean = ScanSummary {
            stats: LocalScanStats::default(),
            failures: Vec::new(),
            cancelled: false,
            dir_signature: Some(signature),
        };
        state.apply_summary(&fx.root, &clean);
        assert!(state.dir_signatures.contains_key(&fx.root));

        // ledger entry: the stored signature is dropped
        let failed = ScanSummary {
            failures: vec![ScanFailure {
                path: fx.path("x.wav"),
                error: "boom".to_string(),
                kind: ScanFailureKind::Parse,
            }],
            dir_signature: None, // the engine withholds it for failed walks
            ..clean.clone()
        };
        state.apply_summary(&fx.root, &failed);
        assert!(!state.dir_signatures.contains_key(&fx.root));

        // cancelled walk: dropped as well
        state.set_dir_signature(&fx.root, signature);
        let cancelled = ScanSummary {
            cancelled: true,
            dir_signature: None,
            ..clean
        };
        state.apply_summary(&fx.root, &cancelled);
        assert!(!state.dir_signatures.contains_key(&fx.root));
    }

    #[test]
    #[cfg(unix)]
    fn failed_walks_withhold_and_invalidate_the_stored_signature() {
        let fx = ScanFixture::new("sig-poison");

        // Clean walk with everything readable: signature handed out and folded.
        fx.wav("open.wav", 100);
        let (first, summary0) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert!(summary0.dir_signature.is_some());
        let mut state = fold_entries(&first);
        state.apply_summary(&fx.root, &summary0);
        assert!(state.dir_signatures.contains_key(&fx.root));

        // A new file arrives unreadable (mode 000). Creating it changes the
        // directory mtime, so the next scan walks; the parse fails and the
        // ledger records it. (A mere chmod on an already-known file would be
        // legitimately reused without opening it, since the fingerprint is
        // unchanged; a new file must be parsed.)
        fx.wav("locked.wav", 100);
        if !deny_read(&fx.path("locked.wav")) {
            restore_read(&fx.path("locked.wav"));
            return;
        }
        let (_, summary1) = run_scan(&fx.root, &ScanOptions::default(), &state);
        restore_read(&fx.path("locked.wav"));
        assert_eq!(summary1.stats.failed, 1);
        assert_eq!(summary1.failures[0].kind, ScanFailureKind::Permission);
        // the failed walk withholds the signature...
        assert!(summary1.dir_signature.is_none());
        // ...and folding it removes the stale stored one
        state.apply_summary(&fx.root, &summary1);
        assert!(!state.dir_signatures.contains_key(&fx.root));

        // Permissions repaired: no stale signature remains, so the next scan
        // walks again and both files are visible.
        let (third, summary2) = run_scan(&fx.root, &ScanOptions::default(), &state);
        assert_eq!(summary2.stats.discovered, 2);
        assert_eq!(third.len(), 2);
    }

    #[test]
    fn fast_path_emission_is_cancellable() {
        let fx = ScanFixture::new("fast-cancel");
        fx.wav("a.wav", 100);
        fx.wav("b.wav", 100);

        let (first, summary0) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert!(summary0.dir_signature.is_some());
        let mut state = fold_entries(&first);
        state.apply_summary(&fx.root, &summary0);

        let token = CancelToken::new();
        let mut entries_seen = 0;
        let mut control = ScanControl {
            cancel: Some(&token),
            ..ScanControl::default()
        };
        let summary = scan_stream(
            &fx.root,
            &ScanOptions::default(),
            &state,
            &mut |event| {
                if let ScanEvent::Entry(_) = event {
                    entries_seen += 1;
                    token.cancel();
                }
            },
            &mut control,
        );

        // the fast path checks the token between emissions
        assert!(summary.cancelled);
        assert_eq!(entries_seen, 1);
        assert_eq!(summary.stats.reused, 1);
        // a partial emission hands out no signature
        assert!(summary.dir_signature.is_none());
    }

    #[test]
    fn the_same_scan_control_is_reusable_across_scans() {
        use std::cell::Cell;
        let fx = ScanFixture::new("control-reuse");
        fx.wav("song.wav", 100);

        let progress_snapshots = Cell::new(0);
        let mut control = ScanControl {
            progress: Some(&|_progress| progress_snapshots.set(progress_snapshots.get() + 1)),
            ..ScanControl::default()
        };
        let mut next = ScanState::default();
        let summary0 = scan_stream(
            &fx.root,
            &ScanOptions::default(),
            &ScanState::default(),
            &mut |event| next.absorb(&event),
            &mut control,
        );
        assert!(progress_snapshots.get() > 0);

        let mut next2 = ScanState::default();
        let summary1 = scan_stream(
            &fx.root,
            &ScanOptions::default(),
            &next,
            &mut |event| next2.absorb(&event),
            &mut control,
        );
        // the same control (progress + cancel) still worked in round two
        assert!(progress_snapshots.get() > 1);
        assert!(summary0.dir_signature.is_some() && summary1.dir_signature.is_some());
        assert_eq!(summary1.stats.parsed, 0);
        assert_eq!(summary1.stats.reused, 1);
        assert_eq!(next2.entries.len(), 1);
    }

    #[test]
    fn panicking_parses_land_in_the_ledger_on_both_paths() {
        // the guard is shared by the serial and the parallel engines
        let failure = parse_guarded(
            &PathBuf::from("/nowhere/x.wav"),
            || -> std::result::Result<Vec<ScanEntry>, ScanFailure> { panic!("boom") },
        )
        .unwrap_err();
        assert_eq!(failure.kind, ScanFailureKind::Parse);
        assert!(
            failure.error.contains("parse panicked: boom"),
            "{}",
            failure.error
        );

        let ok = parse_guarded(&PathBuf::from("/nowhere/x.wav"), || {
            Ok(Vec::<ScanEntry>::new())
        })
        .unwrap();
        assert!(ok.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn follow_policy_duplicates_per_link_and_loops_land_in_the_ledger() {
        use std::os::unix::fs::symlink;
        let follow = ScanOptions {
            symlinks: SymlinkPolicy::Follow,
            ..ScanOptions::default()
        };

        // one entity through N links yields N entries (independent keys)
        let fx = ScanFixture::new("follow-dup");
        fx.wav("real/song.wav", 100);
        symlink(fx.path("real/song.wav"), fx.path("alias.wav")).unwrap();
        symlink(fx.path("real/song.wav"), fx.path("alias2.wav")).unwrap();
        let (entries, summary) = run_scan(&fx.root, &follow, &ScanState::default());
        assert_eq!(summary.stats.discovered, 3);
        assert_eq!(entries.len(), 3);

        // a looping directory link comes back as a walker error in the
        // ledger, without hanging or aborting the scan
        let fx2 = ScanFixture::new("follow-loop");
        fx2.wav("real/song.wav", 100);
        symlink(fx2.path("real"), fx2.path("real/loop")).unwrap();
        let (entries, summary) = run_scan(&fx2.root, &follow, &ScanState::default());
        assert_eq!(summary.stats.failed, 1);
        assert_eq!(summary.stats.discovered, 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_path, fx2.path("real/song.wav"));
    }

    #[test]
    fn subtree_additions_are_invisible_without_force() {
        let fx = ScanFixture::new("subtree-blind");
        fx.wav("album/old.wav", 100);

        let (first, summary0) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(summary0.stats.discovered, 1);
        let mut state = fold_entries(&first);
        state.apply_summary(&fx.root, &summary0);

        // A file appears inside an EXISTING subdirectory: the subdirectory's
        // mtime moves, the root's does not, so the signature still matches
        // and the fast path skips the whole tree. Documented limitation,
        // same root-level scope as upstream (mod.rs:200-217).
        fx.wav("album/new.wav", 100);
        let (stale, stale_summary) = run_scan(&fx.root, &ScanOptions::default(), &state);
        assert_eq!(stale_summary.stats.reused, 1);
        assert_eq!(stale_summary.stats.parsed, 0);
        assert_eq!(stale.len(), 1);

        // Hosts without a recursive watcher must force periodically (or diff
        // the tree themselves) to escape this blind spot.
        let forced = ScanOptions {
            force: true,
            ..ScanOptions::default()
        };
        let (fresh, fresh_summary) = run_scan(&fx.root, &forced, &state);
        assert_eq!(fresh_summary.stats.parsed, 1);
        assert_eq!(fresh.len(), 2);
    }

    #[test]
    #[cfg(unix)]
    fn apply_summary_keys_signatures_on_canonical_roots() {
        use std::os::unix::fs::symlink;
        let fx = ScanFixture::new("canon-key");
        fx.wav("music/song.wav", 100);
        let link_root = fx.path("canon-link");
        symlink(fx.path("music"), &link_root).unwrap();

        // Scan through the symlinked root; the engine canonicalizes it.
        let (first, summary0) =
            run_scan(&link_root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(summary0.stats.discovered, 1);
        let mut state = fold_entries(&first);
        state.apply_summary(&link_root, &summary0);

        // Unchanged tree: the fast path must hit through the same symlinked
        // root. discovered == 0 is the discriminator: the fast path never
        // stats any file, a full walk would report discovered == 1.
        let (second, summary1) = run_scan(&link_root, &ScanOptions::default(), &state);
        assert_eq!(summary1.stats.discovered, 0);
        assert_eq!(summary1.stats.parsed, 0);
        assert_eq!(summary1.stats.reused, 1);
        assert_eq!(second.len(), 1);
    }

    #[test]
    fn fast_path_cancel_counts_partially_emitted_groups() {
        let fx = ScanFixture::new("fast-cancel-partial");
        fx.wav("scan album.wav", CUE_AUDIO_MS);
        fx.write_text("album.cue", SCAN_SHEET);

        let (first, summary0) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(first.len(), 3);
        let mut state = fold_entries(&first);
        state.apply_summary(&fx.root, &summary0);

        // Path-sorted groups: album.cue's two tracks are emitted first; the
        // token flips inside the first entry of the group.
        let token = CancelToken::new();
        let mut entries_seen = 0;
        let mut control = ScanControl {
            cancel: Some(&token),
            ..ScanControl::default()
        };
        let summary = scan_stream(
            &fx.root,
            &ScanOptions::default(),
            &state,
            &mut |event| {
                if let ScanEvent::Entry(_) = event {
                    entries_seen += 1;
                    token.cancel();
                }
            },
            &mut control,
        );

        assert!(summary.cancelled);
        assert_eq!(entries_seen, 1);
        // a cancelled multi-track group still counts exactly what was emitted
        assert_eq!(summary.stats.reused, entries_seen);
    }

    // ───────────────── diagnostics (s5) ─────────────────

    /// Hand-built entry for the pure diagnostics tests (no file system).
    fn diag_entry(
        title: Option<&str>,
        artist: Option<&str>,
        album: Option<&str>,
        duration_ms: u64,
        source: &str,
    ) -> ScanEntry {
        ScanEntry {
            source_path: PathBuf::from(source),
            play_path: PathBuf::from(source),
            container_track: None,
            start_ms: 0,
            metadata: LocalMetadata {
                title: title.map(str::to_string),
                artist: artist.map(str::to_string),
                album: album.map(str::to_string),
                duration_ms,
                format: None,
                audio: None,
            },
            fingerprint: FileFingerprint {
                size: 0,
                modified_unix_nanos: 0,
            },
            reused: false,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn duplicate_groups_fold_case_and_whitespace_variants() {
        let entries = vec![
            diag_entry(
                Some("My Song"),
                Some("Artist"),
                Some("Album"),
                2_000,
                "/m/1.wav",
            ),
            diag_entry(
                Some("MY  SONG"),
                Some("artist"),
                Some("ALBUM"),
                2_000,
                "/m/2.wav",
            ),
            diag_entry(
                Some("my song"),
                Some("ARTIST"),
                Some("album"),
                2_000,
                "/n/3.wav",
            ),
            // same names but a different measured duration: not a duplicate
            diag_entry(
                Some("My Song"),
                Some("Artist"),
                Some("Album"),
                3_000,
                "/m/4.wav",
            ),
            // same duration but a different title: a singleton, dropped
            diag_entry(
                Some("Your Song"),
                Some("Artist"),
                Some("Album"),
                2_000,
                "/m/5.wav",
            ),
        ];
        let groups = duplicate_groups(&entries);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].key, "my song\u{1f}artist\u{1f}album\u{1f}2000");
        let paths: Vec<_> = groups[0]
            .entries
            .iter()
            .map(|entry| entry.source_path.clone())
            .collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("/m/1.wav"),
                PathBuf::from("/m/2.wav"),
                PathBuf::from("/n/3.wav")
            ]
        );

        // the same fold over a ScanState works too
        let state = fold_entries(&entries);
        assert_eq!(duplicate_groups(state.entries.values().flatten()).len(), 1);
    }

    #[test]
    fn duplicate_groups_keep_punctuation_unlike_the_matcher() {
        // The matcher normalization (src/matcher.rs:98-103) strips every
        // non-alphanumeric character and would fold "song a" and "song-a"
        // together; the scanner duplicate key does not (two normalizers,
        // two semantics, LOCAL_ENHANCEMENT_DESIGN.md §10).
        let entries = vec![
            diag_entry(
                Some("song a"),
                Some("Artist"),
                Some("Album"),
                2_000,
                "/m/1.wav",
            ),
            diag_entry(
                Some("song-a"),
                Some("Artist"),
                Some("Album"),
                2_000,
                "/m/2.wav",
            ),
            // whitespace runs and case DO fold
            diag_entry(
                Some("song  A"),
                Some("Artist"),
                Some("Album"),
                2_000,
                "/m/3.wav",
            ),
        ];
        let groups = duplicate_groups(&entries);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].entries.len(), 2);
        assert_eq!(
            groups[0].entries[0].metadata.title.as_deref(),
            Some("song a")
        );

        // untagged fields normalize to the empty string: two same-duration
        // untagged entries land in one group
        let untagged = vec![
            diag_entry(None, None, None, 1_500, "/m/4.wav"),
            diag_entry(None, None, None, 1_500, "/m/5.wav"),
        ];
        assert_eq!(duplicate_groups(&untagged).len(), 1);
    }

    #[test]
    fn duplicate_groups_over_a_real_scan() {
        let fx = ScanFixture::new("duplicates");
        for (name, duration) in [
            ("one.wav", 2_000u64),
            ("two.wav", 2_000),
            ("three.wav", 2_000),
            ("longer.wav", 3_000),
            ("other.wav", 2_000),
        ] {
            fx.wav(name, duration);
        }
        let tags = [
            ("one.wav", "My Song", "Dedup Artist", "Dedup Album"),
            ("two.wav", "my   song", "dedup artist", "DEDUP ALBUM"),
            ("three.wav", "MY SONG", "Dedup Artist", "Dedup Album"),
            // same normalized names but a different measured duration
            ("longer.wav", "My Song", "Dedup Artist", "Dedup Album"),
            // same duration but a different title
            ("other.wav", "Your Song", "Dedup Artist", "Dedup Album"),
        ];
        for (name, title, artist, album) in tags {
            crate::tag_write::write_metadata(
                &fx.path(name),
                &crate::tag_write::MetadataEdit {
                    title: Some(title.to_string()),
                    artist: Some(artist.to_string()),
                    album: Some(album.to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(summary.stats.parsed, 5);
        let groups = duplicate_groups(&entries);
        assert_eq!(groups.len(), 1, "only the case/whitespace variants group");
        assert_eq!(
            groups[0].key,
            "my song\u{1f}dedup artist\u{1f}dedup album\u{1f}2000"
        );
        let paths: Vec<_> = groups[0]
            .entries
            .iter()
            .map(|entry| entry.source_path.clone())
            .collect();
        assert_eq!(
            paths,
            [fx.path("one.wav"), fx.path("three.wav"), fx.path("two.wav")]
        );
    }

    #[test]
    fn missing_entries_reports_deleted_and_skips_present_files() {
        let fx = ScanFixture::new("missing-basic");
        fx.wav("a.wav", 1_000);
        fx.wav("b.wav", 1_000);
        fx.wav("keep/c.wav", 1_000);
        let (first, _) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        let previous = fold_entries(&first);
        assert_eq!(previous.entries.len(), 3);

        // Option-change shape: the scan result no longer contains a.wav but
        // the file still exists on disk, so nothing is reported
        // (voicefox `mod.rs:263-268` keeps hidden files out of the report).
        let without_a: Vec<ScanEntry> = first
            .iter()
            .filter(|entry| entry.source_path != fx.path("a.wav"))
            .cloned()
            .collect();
        assert_eq!(without_a.len(), 2);
        assert!(missing_entries(&[], &previous, &without_a, &fx.root).is_empty());

        // Deletion: a.wav is gone from the scan result AND from the disk.
        fs::remove_file(fx.path("a.wav")).unwrap();
        let records = missing_entries(&[], &previous, &without_a, &fx.root);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, fx.path("a.wav"));
        assert_eq!(records[0].track.metadata.duration_ms, 1_000);

        // A real rescan of the same tree reports the same single record and
        // nothing for the files that are still there.
        let (second, _) = run_scan(&fx.root, &ScanOptions::default(), &previous);
        assert_eq!(second.len(), 2);
        let records = missing_entries(&[], &previous, &second, &fx.root);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path, fx.path("a.wav"));
    }

    #[test]
    fn missing_entries_collapse_a_vanished_cue_to_one_record() {
        let fx = ScanFixture::new("missing-cue");
        fx.wav("scan album.wav", CUE_AUDIO_MS);
        fx.write_text("album.cue", SCAN_SHEET);
        fx.wav("plain.wav", 1_000);
        let (first, _) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        // two CUE tracks + the referenced audio as its own plain entry + plain.wav
        assert_eq!(first.len(), 4);
        let previous = fold_entries(&first);

        fs::remove_file(fx.path("album.cue")).unwrap();
        let (second, _) = run_scan(&fx.root, &ScanOptions::default(), &previous);
        assert_eq!(second.len(), 2);

        let records = missing_entries(&[], &previous, &second, &fx.root);
        assert_eq!(records.len(), 1, "one vanished CUE source, one record");
        assert_eq!(records[0].path, fx.path("album.cue"));
        assert_eq!(
            records[0].track.container_track.as_deref(),
            Some(cue_track_id(&fx.path("album.cue"), "01").as_str()),
            "the representative is the smallest container identity"
        );
    }

    #[test]
    fn missing_entries_retain_until_the_path_returns() {
        let fx = ScanFixture::new("missing-return");
        fx.wav("a.wav", 1_000);
        fx.wav("b.wav", 1_000);
        let (first, _) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        let state1 = fold_entries(&first);

        fs::remove_file(fx.path("a.wav")).unwrap();
        let (second, _) = run_scan(&fx.root, &ScanOptions::default(), &state1);
        let state2 = fold_entries(&second);
        let round2 = missing_entries(&[], &state1, &second, &fx.root);
        assert_eq!(round2.len(), 1);
        assert_eq!(round2[0].path, fx.path("a.wav"));

        // A pass that never saw the path keeps the record: upstream retains
        // the missing list across scans and only drops present paths
        // (mod.rs:636-641).
        let kept = missing_entries(&round2, &state2, &[], &fx.root.join("elsewhere"));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, fx.path("a.wav"));

        // The file returns and is scanned again: the record clears.
        fx.wav("a.wav", 1_000);
        let (third, _) = run_scan(&fx.root, &ScanOptions::default(), &state2);
        let cleared = missing_entries(&round2, &state2, &third, &fx.root);
        assert!(cleared.is_empty());

        // Folding the clear round into the list stays clear on a rescan.
        let state3 = fold_entries(&third);
        let settled = missing_entries(&cleared, &state3, &third, &fx.root);
        assert!(settled.is_empty());
    }

    #[test]
    fn corrupt_files_is_the_ledger_view() {
        let fx = ScanFixture::new("corrupt-view");
        fx.wav("good.wav", 1_000);
        fx.write("broken.mp3", b"not audio at all");
        let (_, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert!(!summary.failures.is_empty());
        // The view returns the ledger as-is: same order, no filtering, no
        // dedup (upstream `corrupt_files` is a verbatim alias, mod.rs:467-470).
        assert_eq!(corrupt_files(&summary.failures), summary.failures);
        assert!(corrupt_files(&[]).is_empty());
    }

    // ───────────────── s6: backfill and normalization ─────────────────

    /// Plain entries finalize between the raw read and entry assembly
    /// (§3.9): an untagged file carries the stem title and the unknown
    /// defaults while the measured spec stays filled (parity L15).
    #[test]
    fn scan_entries_backfill_untagged_audio() {
        let fx = ScanFixture::new("backfill-scan");
        fx.wav("First Track.wav", 1_000);
        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(summary.stats.parsed, 1);
        assert_eq!(summary.failures.len(), 0);
        let entry = &entries[0];
        assert_eq!(entry.metadata.title.as_deref(), Some("First Track"));
        assert_eq!(entry.metadata.artist.as_deref(), Some("未知艺术家"));
        assert_eq!(entry.metadata.album.as_deref(), Some("未知专辑"));
        assert!(entry.metadata.audio.is_some());
    }

    /// The CUE face wins over the referenced audio's own tags (container
    /// layer, upstream `scanner.rs:351-378`): the sheet's track table fills
    /// title/artist/album even though the audio carries different tags,
    /// the sheet fields get the same whitespace collapse as the plain read
    /// layer (s6 item b), and the audio spec still comes from the tagged
    /// file through the raw container read.
    #[test]
    fn cue_face_wins_over_the_backfilled_audio_tags() {
        let fx = ScanFixture::new("cue-face-priority");
        fx.wav("tagged audio.wav", CUE_AUDIO_MS);
        crate::tag_write::write_metadata(
            &fx.path("tagged audio.wav"),
            &crate::tag_write::MetadataEdit {
                title: Some("Tag Title".to_string()),
                artist: Some("Tag Artist".to_string()),
                album: Some("Tag Album".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        fx.write_text(
            "album.cue",
            "TITLE \"  Cue  Album  \"\r\n\
             PERFORMER \"  Cue  Artist  \"\r\n\
             FILE \"tagged audio.wav\" WAVE\r\n\
             TRACK 01 AUDIO\r\n\
             TITLE \"  Cue  Track  \"\r\n\
             INDEX 01 00:00:00\r\n",
        );
        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        // One CUE source plus the referenced audio as its own plain entry.
        assert_eq!(summary.stats.parsed, 2);
        let entry = entries
            .iter()
            .find(|entry| entry.container_track.is_some())
            .expect("the sheet produces one CUE entry");
        // Sheet values, collapsed; neither the tag title nor the file
        // stem leaks through.
        assert_eq!(entry.metadata.title.as_deref(), Some("Cue Track"));
        assert_eq!(entry.metadata.artist.as_deref(), Some("Cue Artist"));
        assert_eq!(entry.metadata.album.as_deref(), Some("Cue Album"));
        assert!(
            entry
                .container_track
                .as_deref()
                .is_some_and(|track| track.ends_with("album.cue#01"))
        );
        // The measured spec survives the face priority (raw base read).
        assert_eq!(
            entry.metadata.audio,
            Some(AudioSpec {
                bitrate: Some(706),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                lossless: true,
            })
        );
    }

    /// The duplicate key folds fullwidth variants onto halfwidth ones (s6
    /// item b, comparison keys only) while the stored entries keep their
    /// own spelling verbatim.
    #[test]
    fn duplicate_keys_fold_fullwidth_variants_but_stored_values_keep_them() {
        let fx = ScanFixture::new("fullwidth-dup");
        fx.wav("wide.wav", 2_000);
        fx.wav("narrow.wav", 2_000);
        for (name, title) in [("wide.wav", "Ｍｙ　Ｓｏｎｇ"), ("narrow.wav", "my song")] {
            crate::tag_write::write_metadata(
                &fx.path(name),
                &crate::tag_write::MetadataEdit {
                    title: Some(title.to_string()),
                    artist: Some("Artist".to_string()),
                    album: Some("Album".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let (entries, _) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        // Stored values keep fullwidth letters and the collapsed ideographic
        // space; only the key folds.
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.source_path.ends_with("wide.wav"))
                .unwrap()
                .metadata
                .title
                .as_deref(),
            Some("Ｍｙ Ｓｏｎｇ")
        );
        let groups = duplicate_groups(&entries);
        assert_eq!(groups.len(), 1, "fullwidth and halfwidth variants group");
        assert_eq!(groups[0].key, "my song\u{1f}artist\u{1f}album\u{1f}2000");
    }

    // ───────────────── s7: lyrics scan integration ─────────────────

    /// s7 (LOCAL_ENHANCEMENT_DESIGN.md §3.10): the opt-in puts the embedded
    /// text under `extra[EXTRA_LYRICS]`, the default keeps the product
    /// lyric-free (the memory boundary), and the incremental reuse path
    /// carries the text over from the previous state.
    #[test]
    fn read_lyrics_option_lands_the_text_in_extra_and_defaults_keep_it_out() {
        let fx = ScanFixture::new("lyrics-scan");
        fx.wav("song.wav", 5_000);
        crate::tag_write::write_metadata(
            &fx.path("song.wav"),
            &crate::tag_write::MetadataEdit {
                title: Some("Lyric Song".to_string()),
                artist: None,
                album: None,
                cover: None,
                lyrics: Some("[00:01.00]扫描歌词".to_string()),
            },
        )
        .unwrap();

        // Default scan: no lyrics anywhere in the product.
        let (entries, summary) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(summary.stats.parsed, 1);
        assert!(!entries[0].extra.contains_key(EXTRA_LYRICS));

        // Opt-in: the embedded text lands under EXTRA_LYRICS.
        let options = ScanOptions {
            read_lyrics: true,
            ..ScanOptions::default()
        };
        let (entries, _) = run_scan(&fx.root, &options, &ScanState::default());
        assert_eq!(
            entries[0].extra.get(EXTRA_LYRICS).map(String::as_str),
            Some("[00:01.00]扫描歌词")
        );

        // Unchanged file, same option: the reuse path keeps the text.
        let state = fold_entries(&entries);
        let (entries, summary) = run_scan(&fx.root, &options, &state);
        assert_eq!(summary.stats.reused, 1);
        assert!(entries[0].reused);
        assert_eq!(
            entries[0].extra.get(EXTRA_LYRICS).map(String::as_str),
            Some("[00:01.00]扫描歌词")
        );
    }

    /// The scan integration reads through the same `local::read_lyrics`
    /// face as the standalone function, so a GB18030-encoded sidecar
    /// decodes in the scan product too (parity L18 wiring), while CUE
    /// entries stay lyric-free on purpose (§3.10: one sheet fans out to N
    /// tracks over one file-relative text).
    #[test]
    fn scan_lyrics_cover_the_sidecar_face_and_skip_cue_entries() {
        let fx = ScanFixture::new("lyrics-scan-sidecar");
        fx.wav("album.wav", 5_000);
        fx.write_text(
            "album.cue",
            "TITLE \"L\"\r\nPERFORMER \"A\"\r\nFILE \"album.wav\" WAVE\r\n  \
             TRACK 01 AUDIO\r\n    TITLE \"T\"\r\n    INDEX 01 00:00:00\r\n",
        );
        let (gb_bytes, _, _) = encoding_rs::GB18030.encode("[00:01.00]外挂歌词");
        fx.write("album.lrc", &gb_bytes);

        let options = ScanOptions {
            read_lyrics: true,
            ..ScanOptions::default()
        };
        let (entries, summary) = run_scan(&fx.root, &options, &ScanState::default());
        assert_eq!(summary.stats.discovered, 2);
        assert_eq!(entries.len(), 2);

        let wav = entries
            .iter()
            .find(|entry| entry.container_track.is_none())
            .unwrap();
        assert_eq!(
            wav.extra.get(EXTRA_LYRICS).map(String::as_str),
            Some("[00:01.00]外挂歌词")
        );

        let cue = entries
            .iter()
            .find(|entry| entry.container_track.is_some())
            .unwrap();
        assert!(!cue.extra.contains_key(EXTRA_LYRICS));
    }

    // ───────────────── s8: cover scan integration ─────────────────

    /// Synthetic PNG via the runtime `image` dependency (feature
    /// local-cover; same helper as the src/local_cover.rs tests).
    #[cfg(feature = "local-cover")]
    fn cover_png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    /// Committed ffmpeg-built MP3 fixture (tagged with the cover the
    /// extraction tests need; same precedent as src/local.rs tests).
    #[cfg(feature = "local-cover")]
    const MP3_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/local/spec_mp3_cbr128.mp3");

    #[cfg(feature = "local-cover")]
    struct CoverFixture {
        root: std::path::PathBuf,
        cache: std::path::PathBuf,
    }
    #[cfg(feature = "local-cover")]
    impl CoverFixture {
        /// Scan root plus the host-specified cover cache directory, both
        /// under temp and removed on drop (ScanFixture cannot be reused:
        /// its own Drop removes the shared root).
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("musid-dli-scan-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            let cache = std::env::temp_dir().join(format!(
                "musid-dli-scan-cover-cache-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&cache);
            fs::create_dir_all(&cache).unwrap();
            Self {
                root: fs::canonicalize(root).unwrap(),
                cache: fs::canonicalize(cache).unwrap(),
            }
        }
        fn path(&self, rel: &str) -> std::path::PathBuf {
            self.root.join(rel)
        }
        fn write(&self, rel: &str, bytes: &[u8]) {
            fs::write(self.path(rel), bytes).unwrap();
        }
        fn write_text(&self, rel: &str, text: &str) {
            self.write(rel, text.as_bytes());
        }
        fn options(&self, priority: crate::local_cover::CoverPriority) -> ScanOptions {
            ScanOptions {
                cover: crate::local_cover::CoverScanOptions {
                    cache_dir: Some(self.cache.clone()),
                    priority,
                },
                ..ScanOptions::default()
            }
        }
    }
    #[cfg(feature = "local-cover")]
    impl Drop for CoverFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.cache);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// s8 (LOCAL_ENHANCEMENT_DESIGN.md §3.11): the default scan product
    /// carries no cover key at all (the memory boundary); with the
    /// host-specified cache directory set, the embedded cover lands under
    /// `extra[EXTRA_COVER_CACHE_PATH]` as the validated cache path, an
    /// external-only file resolves to the same-directory name, and the
    /// reuse round keeps both. The scan face and the standalone
    /// `read_cover` face agree on the embedded cache path.
    #[test]
    #[cfg(feature = "local-cover")]
    fn cover_option_lands_cache_paths_and_defaults_keep_it_out() {
        use crate::local_cover::CoverPriority;

        let fx = CoverFixture::new("cover-scan");
        let mp3 = fx.path("song.mp3");
        fs::write(&mp3, MP3_FIXTURE).unwrap();
        crate::tag_write::write_metadata(
            &mp3,
            &crate::tag_write::MetadataEdit {
                cover: Some(cover_png_bytes(8, 4)),
                ..Default::default()
            },
        )
        .unwrap();
        let wav = fx.path("folder.wav");
        fs::write(&wav, wav_bytes(5_000)).unwrap();
        fs::write(fx.path("folder.jpg"), cover_png_bytes(5, 5)).unwrap();

        // Default scan: no cover key anywhere in the product.
        let (entries, _) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|entry| !entry.extra.contains_key(EXTRA_COVER_CACHE_PATH))
        );

        // Opt-in: the embedded path for the MP3, the directory file for the
        // WAV, and both are real paths on disk.
        let options = fx.options(CoverPriority::EmbeddedFirst);
        let (entries, _) = run_scan(&fx.root, &options, &ScanState::default());
        let mp3_entry = entries
            .iter()
            .find(|entry| entry.container_track.is_none() && entry.source_path == mp3)
            .unwrap();
        let wav_entry = entries
            .iter()
            .find(|entry| entry.source_path == wav)
            .unwrap();

        let mp3_cover = mp3_entry
            .extra
            .get(EXTRA_COVER_CACHE_PATH)
            .map(std::path::PathBuf::from)
            .expect("embedded cover path on the mp3 entry");
        assert!(mp3_cover.starts_with(&fx.cache), "{mp3_cover:?}");
        assert_eq!(
            crate::local_cover::probe_dimensions(&mp3_cover),
            Some((8, 4)),
            "the scan landed a decode-valid cache file"
        );
        let wav_cover = wav_entry
            .extra
            .get(EXTRA_COVER_CACHE_PATH)
            .map(std::path::PathBuf::from)
            .expect("external cover path on the wav entry");
        assert_eq!(wav_cover, fx.path("folder.jpg"));

        // The scan face and the standalone face produce the same path.
        assert_eq!(
            crate::local_cover::read_cover(&mp3, &fx.cache, CoverPriority::EmbeddedFirst),
            Some(mp3_cover.clone()),
            "scan and standalone faces share the cache path"
        );

        // Unchanged files, same options: the reuse round keeps the paths.
        let state = fold_entries(&entries);
        let (entries, summary) = run_scan(&fx.root, &options, &state);
        assert_eq!(summary.stats.reused, 2);
        assert!(entries.iter().all(|entry| entry.reused));
        let reused_mp3 = entries
            .iter()
            .find(|entry| entry.source_path == mp3)
            .unwrap();
        assert_eq!(
            reused_mp3
                .extra
                .get(EXTRA_COVER_CACHE_PATH)
                .map(std::path::PathBuf::from),
            Some(mp3_cover)
        );
    }

    /// s8 CUE wiring: both tracks of one sheet carry the same cover path
    /// (resolved once per distinct referenced audio; unlike the lyrics
    /// text, a cover path is a single path and per-track copies are
    /// exact), and the default keeps the key out (§3.11).
    #[test]
    #[cfg(feature = "local-cover")]
    fn cue_entries_share_one_cover_path_per_referenced_audio() {
        let fx = CoverFixture::new("cover-scan-cue");
        let wav = fx.path("album.wav");
        fs::write(&wav, wav_bytes(5_000)).unwrap();
        crate::tag_write::write_metadata(
            &wav,
            &crate::tag_write::MetadataEdit {
                cover: Some(cover_png_bytes(6, 6)),
                ..Default::default()
            },
        )
        .unwrap();
        fx.write_text(
            "album.cue",
            "TITLE \"L\"\r\nPERFORMER \"A\"\r\nFILE \"album.wav\" WAVE\r\n  \
             TRACK 01 AUDIO\r\n    TITLE \"T1\"\r\n    INDEX 01 00:00:00\r\n  \
             TRACK 02 AUDIO\r\n    TITLE \"T2\"\r\n    INDEX 01 00:02:00\r\n",
        );

        let options = fx.options(crate::local_cover::CoverPriority::EmbeddedFirst);
        let (entries, _) = run_scan(&fx.root, &options, &ScanState::default());
        assert_eq!(entries.len(), 3, "plain wav plus two sheet tracks");
        let cue_paths: Vec<&String> = entries
            .iter()
            .filter(|entry| entry.container_track.is_some())
            .map(|entry| {
                entry
                    .extra
                    .get(EXTRA_COVER_CACHE_PATH)
                    .expect("every track carries the sheet's cover path")
            })
            .collect();
        assert_eq!(cue_paths.len(), 2);
        assert_eq!(cue_paths[0], cue_paths[1], "one audio file, one cover path");
        let cover = std::path::PathBuf::from(cue_paths[0].as_str());
        assert!(cover.starts_with(&fx.cache));
        assert_eq!(crate::local_cover::probe_dimensions(&cover), Some((6, 6)));

        // Default options: the key stays out of the CUE product too.
        let (entries, _) = run_scan(&fx.root, &ScanOptions::default(), &ScanState::default());
        assert!(
            entries
                .iter()
                .all(|entry| !entry.extra.contains_key(EXTRA_COVER_CACHE_PATH))
        );
    }
}
