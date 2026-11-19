//! Cover read face for local audio (M5 slice s8, parity L19,
//! docs/integration/LOCAL_ENHANCEMENT_DESIGN.md §3.11): embedded first-tag
//! cover extraction into a decode-validated cache, same-directory external
//! cover detection, and the [`read_cover`] composition with a configurable
//! embedded/external priority.
//!
//! Parity L19 (voicefox `source/src/local/metadata.rs:201-228`
//! `extract_cover` + `source/src/cover_cache.rs` embedded half) is the
//! embedded face: the first tag's first picture is keyed by
//! [`embedded_cover_path`] (audio path bytes + picture bytes, so re-embedding
//! a different picture under the same path yields a new key and the old
//! cover is not served), validated by a full [`probe_dimensions`] decode, and
//! stored atomically through the shared s3 primitive
//! [`crate::tag_write::atomic_replace`]; a corrupt cache file is removed and
//! rebuilt (upstream `cover_cache.rs:64-83` + `metadata.rs:214-222`).
//!
//! musid-only deltas, all deliberate and documented:
//!
//! - **The cache directory is host-specified at every call.** Upstream
//!   resolves one directory (`ProjectDirs`/`dirs`, `cover_cache.rs:35-44`),
//!   carries a 512-entry budget and sweeps; musid ships none of that (§9
//!   out of scope, directory policy belongs to the host): no call reads or
//!   writes anything outside the directory passed in.
//! - [`store_bytes`] stages through [`crate::tag_write::atomic_replace`]
//!   instead of upstream's own `.part.` temp naming: same-directory temp
//!   files, fsync-promoted rename, the s3 promote contract (a staged copy is
//!   never silently destroyed) and the symlink-target refusal apply. The
//!   decode check runs on the staged temp before the rename, so a corrupt
//!   write never lands in the cache.
//! - The hex encoding of the hash is written by hand (std-only) instead of
//!   pulling `hex`; the digest itself is sha256 exactly like upstream
//!   (`content_hash`, `cover_cache.rs:50-53`).
//! - The extraction tag is the musid read-layer convention
//!   `primary_tag().or_else(first_tag())` (the same tag the raw read behind
//!   [`crate::local::read_metadata`] picks) while upstream
//!   `extract_cover` uses `first_tag()` alone; the two agree whenever the
//!   file carries one tag, which is every file in practice.
//! - The external face and the priority are musid-only (upstream serves
//!   embedded covers only). [`EXTERNAL_COVER_NAMES`] is a fixed conventional
//!   list probed case-insensitively; external files are returned as their
//!   own paths and are never copied into the cache or decode-validated
//!   (detection only — hosts that need it run [`probe_dimensions`]).
//!
//! Decode scope: the `image` dependency is enabled for png and jpeg only,
//! so those are the formats that validate. An embedded or stored cover in
//! any other container (bmp, webp, ...) fails [`probe_dimensions`], is
//! never cached, and [`read_cover`] degrades to the external face — the
//! same degradation path a corrupt png takes. This is a documented
//! limitation of the slice's dependency budget, not a correctness bug.
//!
//! Memory and write boundaries: picture bytes are held transiently only
//! (long enough to hash and stage); no scan product or return value carries
//! them. The writes this unit performs are cache files inside the
//! host-specified cache directory and nothing else — user audio files are
//! never opened for writing (the tag-write face,
//! [`crate::tag_write::write_metadata`], remains the only way they change).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lofty::file::TaggedFileExt as _;
use lofty::probe::Probe;

/// Same-directory external cover candidates, in probe order (musid-only
/// convention; upstream has no external face). Each stem is tried with the
/// jpg/jpeg/png extensions before the next stem, and the probe is
/// case-insensitive, so `Cover.JPG` matches `cover.jpg`. The list is
/// deliberately exact-name based: no pattern matching, no recursion.
pub const EXTERNAL_COVER_NAMES: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "folder.jpg",
    "folder.jpeg",
    "folder.png",
    "front.jpg",
    "front.jpeg",
    "front.png",
    "album.jpg",
    "album.jpeg",
    "album.png",
    "albumart.jpg",
    "albumart.png",
];

/// Which source wins when a file has both an embedded cover and a
/// same-directory external one (musid-only; upstream is embedded-only).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CoverPriority {
    /// The embedded tag cover wins; the external face is the fallback.
    /// Matches the upstream behavior for files that have both (upstream
    /// never looks at the directory at all).
    #[default]
    EmbeddedFirst,
    /// The same-directory external cover wins; the embedded face is the
    /// fallback. Hosts that keep curated folder art use this.
    ExternalFirst,
}

/// Scan-side cover configuration (s8 opt-in, carried on
/// [`crate::local_scan::ScanOptions::cover`] and copied into every parse
/// job). The default resolves nothing: a scan without a host-specified
/// `cache_dir` does no cover work at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverScanOptions {
    /// Host-owned cover cache directory. `None` keeps the scan product
    /// free of cover paths and cache writes (the memory boundary of s8
    /// item c: the default scan product never carries image bytes).
    pub cache_dir: Option<PathBuf>,
    /// Embedded vs external priority, see [`CoverPriority`].
    pub priority: CoverPriority,
}

/// Sha256 of the key bytes, lowercase hex (parity with upstream
/// `content_hash`, `cover_cache.rs:50-53`). The hash is content-addressed:
/// the same key bytes always produce the same name, and any change to the
/// key (audio path or picture bytes) produces a different one.
pub fn content_hash(key: &[u8]) -> String {
    use sha2::Digest;
    to_hex(&sha2::Sha256::digest(key))
}

/// std-only lowercase hex; the upstream cover cache reaches the same
/// string via `hex::encode`, which musid does not pull for this face.
fn to_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(char::from_digit((byte >> 4).into(), 16).unwrap_or('0'));
        hex.push(char::from_digit((byte & 0x0f).into(), 16).unwrap_or('0'));
    }
    hex
}

/// Cache path for one embedded cover, under the **host-specified** cache
/// directory. The key recipe is the upstream `embedded_cover_path`
/// (`cover_cache.rs:106-114`) verbatim: sha256 of the audio path bytes
/// (as passed, `to_string_lossy`) followed by the picture bytes, named
/// `{hash}.jpg` — the extension is a placeholder like upstream, the real
/// format is sniffed from the content on decode. Re-embedding a different
/// picture under the same path changes the key, so a stale cache file is
/// never served after a re-tag; moving the file changes the key too.
pub fn embedded_cover_path(cache_dir: &Path, audio_path: &Path, picture: &[u8]) -> PathBuf {
    let mut key = audio_path.to_string_lossy().as_bytes().to_vec();
    key.extend_from_slice(picture);
    cache_dir.join(format!("{}.jpg", content_hash(&key)))
}

/// Reads the picture's pixel dimensions by fully decoding it (parity with
/// upstream `probe_dimensions`, `cover_cache.rs:61-70`): a file that is
/// missing, truncated, or decodes to a zero width or height is `None`.
/// Only png and jpeg decode (the enabled `image` features); other formats
/// return `None` and are treated as corrupt by the caching face.
pub fn probe_dimensions(path: impl AsRef<Path>) -> Option<(u32, u32)> {
    let image = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let (width, height) = (image.width(), image.height());
    (width > 0 && height > 0).then_some((width, height))
}

/// The first picture of the first tag (parity L19 extraction half, upstream
/// `extract_cover` `metadata.rs:201-207`, with the musid read-layer tag
/// convention `primary_tag().or_else(first_tag())` — see the module docs).
/// The tag lookup error surfaces like [`crate::local::read_metadata`] does;
/// a file without any tag or picture is `Ok(None)`.
pub fn embedded_cover_bytes(
    path: impl AsRef<Path>,
) -> std::result::Result<Option<Vec<u8>>, lofty::error::LoftyError> {
    let tagged = Probe::open(path.as_ref())?.read()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    Ok(tag
        .and_then(|tag| tag.pictures().first())
        .map(|picture| picture.data().to_vec()))
}

/// Stores the bytes at `target` after a full decode of the staged copy (the
/// upstream `store_bytes` semantics, `cover_cache.rs:81-97`, routed through
/// the shared s3 primitive [`crate::tag_write::atomic_replace`]): the bytes
/// are written to a same-directory temp file, [`probe_dimensions`] validates
/// the temp, and only then the rename lands it at `target`. Any failure
/// leaves no temp file behind and the target untouched, so a corrupt cache
/// file is never overwritten by a corrupt write and a half-written image
/// never lands in the cache. Missing parent directories are created (the
/// cache directory may not exist yet); the returned tuple is the decoded
/// `(width, height)`.
pub fn store_bytes(target: &Path, bytes: &[u8]) -> Result<(u32, u32), String> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create the cover cache directory: {error}"))?;
    }
    crate::tag_write::atomic_replace(target, |tmp| {
        std::fs::write(tmp, bytes)
            .map_err(|error| format!("failed to stage the cover: {error}"))?;
        probe_dimensions(tmp).ok_or_else(|| "cover image is corrupt".to_string())
    })
}

/// Finds a same-directory external cover for `audio_path` (musid-only
/// face): the parent directory is read once and
/// [`EXTERNAL_COVER_NAMES`] are matched case-insensitively in list order.
/// Directories are skipped; files and symlinks match (a symlinked cover is
/// a legitimate layout and this face only ever reads the path it returns).
/// Unreadable entries are skipped, not fatal. `None` when the audio has no
/// parent directory or nothing matches.
pub fn external_cover_path(audio_path: impl AsRef<Path>) -> Option<PathBuf> {
    let directory = audio_path.as_ref().parent()?;
    let mut candidates: HashMap<String, PathBuf> = HashMap::new();
    let entries = std::fs::read_dir(directory).ok()?;
    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            continue;
        }
        let lower = entry.file_name().to_string_lossy().to_lowercase();
        candidates.entry(lower).or_insert_with(|| entry.path());
    }
    EXTERNAL_COVER_NAMES
        .iter()
        .find_map(|name| candidates.get(*name).cloned())
}

/// Resolves one filesystem path to the cover of `audio_path` (s8
/// composition face; upstream serves the embedded face only,
/// `metadata.rs:201-228`). The result is a path the host can hand to an
/// image renderer — for the embedded face a file inside `cache_dir`
/// (created on demand), for the external face the existing same-directory
/// file itself. Image bytes are never returned.
///
/// With [`CoverPriority::EmbeddedFirst`] (the default):
///
/// 1. extract the embedded picture ([`embedded_cover_bytes`]); a file
///    without a readable tag/picture falls through to the external face;
/// 2. look the cache up by [`embedded_cover_path`]; a present and
///    decodable cache file is returned as-is (the hit path);
/// 3. a present but corrupt cache file is removed and rebuilt
///    (upstream `metadata.rs:214-222`);
/// 4. a missing cache file (or one that could not be removed) is stored
///    with [`store_bytes`], which validates before landing;
/// 5. a store failure degrades to the external face instead of returning
///    nothing — upstream returns `None` here because it has no external
///    face; musid prefers serving the folder art over serving nothing
///    (a failing cache directory is a documented degradation, see the
///    decode-scope note in the module docs).
///
/// With [`CoverPriority::ExternalFirst`] the external face is tried first
/// and steps 1-5 fill in when the directory offers nothing. `None` means
/// neither source produced a usable path.
pub fn read_cover(
    audio_path: impl AsRef<Path>,
    cache_dir: &Path,
    priority: CoverPriority,
) -> Option<PathBuf> {
    let audio_path = audio_path.as_ref();
    match priority {
        CoverPriority::EmbeddedFirst => {
            embedded_cached(audio_path, cache_dir).or_else(|| external_cover_path(audio_path))
        }
        CoverPriority::ExternalFirst => {
            external_cover_path(audio_path).or_else(|| embedded_cached(audio_path, cache_dir))
        }
    }
}

/// The embedded face of [`read_cover`]: cache hit, corrupt rebuild, or
/// validated store; `None` when the file has no embedded picture or the
/// cache write fails (the caller falls to the external face).
fn embedded_cached(audio_path: &Path, cache_dir: &Path) -> Option<PathBuf> {
    let picture = embedded_cover_bytes(audio_path).ok()??;
    let cache_path = embedded_cover_path(cache_dir, audio_path, &picture);
    if cache_path.exists() {
        if probe_dimensions(&cache_path).is_some() {
            return Some(cache_path);
        }
        // Corrupt cache file: remove and rebuild (upstream
        // `metadata.rs:214-222`). A failed removal is left to the store
        // below to overwrite via rename.
        let _ = std::fs::remove_file(&cache_path);
    }
    store_bytes(&cache_path, &picture).ok()?;
    Some(cache_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Committed ffmpeg-built MP3 fixture (same precedent as src/local.rs
    /// and src/tag_write.rs tests).
    const MP3_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/local/spec_mp3_cbr128.mp3");

    /// Minimal PCM WAV stub (the formats_cue/local_scan precedent; lofty
    /// parses it, and WAV has no cover slot so only the external face can
    /// serve it).
    fn wav_bytes() -> Vec<u8> {
        const SAMPLE_RATE: u32 = 44_100;
        const BYTES_PER_SECOND: u32 = SAMPLE_RATE * 2; // mono, 16-bit
        const DATA_LEN: u32 = BYTES_PER_SECOND; // 1 s
        let mut wav = Vec::with_capacity(44 + DATA_LEN as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + DATA_LEN).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&BYTES_PER_SECOND.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&DATA_LEN.to_le_bytes());
        wav.resize(wav.len() + DATA_LEN as usize, 0);
        wav
    }

    /// Synthetic PNG via the runtime `image` dependency (the upstream test
    /// helper `png_bytes`, cover_cache.rs:221-226, same shape).
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    struct TempDir {
        root: PathBuf,
    }
    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("musid-dli-cover-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self {
                root: fs::canonicalize(dir).unwrap(),
            }
        }
        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.root)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Tags the MP3 fixture with a cover through the library's own atomic
    /// write (the s3 face), so the extraction tests run on a real tagged
    /// container.
    fn write_cover(path: &Path, cover: &[u8]) {
        crate::tag_write::write_metadata(
            path,
            &crate::tag_write::MetadataEdit {
                cover: Some(cover.to_vec()),
                ..Default::default()
            },
        )
        .unwrap();
    }

    /// The cache key is content-addressed over audio path + picture bytes
    /// (upstream `remote_and_embedded_keys_are_content_addressed`,
    /// cover_cache.rs:277-298): stable per input, new key on any change.
    #[test]
    fn embedded_cover_path_keys_are_content_addressed() {
        let cache = TempDir::new("keys");
        let covers = cache.path("covers");
        let audio = Path::new("/music/a.flac");
        let first = embedded_cover_path(&covers, audio, b"pic-1");
        assert_eq!(
            first,
            embedded_cover_path(&covers, audio, b"pic-1"),
            "the same path and picture must map to one cache file"
        );
        assert_ne!(
            first,
            embedded_cover_path(&covers, audio, b"pic-2"),
            "a different picture must move the key"
        );
        assert_ne!(
            first,
            embedded_cover_path(&covers, Path::new("/music/b.flac"), b"pic-1"),
            "a different audio path must move the key"
        );
        assert!(first.starts_with(&covers));
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), 64 + 4, "{name}");
        assert!(name.ends_with(".jpg"), "{name}");
        assert!(
            name.strip_suffix(".jpg")
                .unwrap()
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    #[test]
    fn content_hash_is_stable_and_hex() {
        let hash = content_hash(b"https://example.com/x.jpg");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hash, content_hash(b"https://example.com/x.jpg"));
        assert_ne!(hash, content_hash(b"https://example.com/y.jpg"));
    }

    /// Extraction, cache fill, and the hit path (parity L19): the embedded
    /// picture lands in the host-specified cache directory under its
    /// content key, validates by decode, and the second call returns the
    /// same path without leaving any temp file behind.
    #[test]
    fn embedded_cover_caches_then_hits() {
        let dir = TempDir::new("embedded");
        let cache = TempDir::new("embedded-cache");
        let audio = dir.path("song.mp3");
        fs::write(&audio, MP3_FIXTURE).unwrap();
        write_cover(&audio, &png_bytes(8, 4));

        let picture = embedded_cover_bytes(&audio).unwrap().expect("tagged cover");
        let cover = read_cover(&audio, &cache.root, CoverPriority::EmbeddedFirst)
            .expect("embedded cover resolves");
        assert!(cover.starts_with(&cache.root), "{cover:?}");
        assert_eq!(
            cover,
            embedded_cover_path(&cache.root, &audio, &picture),
            "the served path is the content-keyed cache path"
        );
        assert_eq!(probe_dimensions(&cover), Some((8, 4)));
        assert_eq!(cache.names().len(), 1, "exactly the cache file, no temp");

        // Second call: the same path comes back from the cache.
        assert_eq!(
            read_cover(&audio, &cache.root, CoverPriority::EmbeddedFirst),
            Some(cover)
        );
        assert_eq!(cache.names().len(), 1);
    }

    /// A cache file corrupted after the fact is detected by the decode
    /// check, removed, and rebuilt from the embedded picture (upstream
    /// `embedded_cover_cache_is_validated_before_replacement`,
    /// metadata.rs:413-426).
    #[test]
    fn corrupt_cache_file_is_rebuilt_from_the_embedded_picture() {
        let dir = TempDir::new("rebuild");
        let cache = TempDir::new("rebuild-cache");
        let audio = dir.path("song.mp3");
        fs::write(&audio, MP3_FIXTURE).unwrap();
        write_cover(&audio, &png_bytes(6, 5));

        let cover = read_cover(&audio, &cache.root, CoverPriority::EmbeddedFirst).unwrap();
        fs::write(&cover, b"corrupt").unwrap();
        assert_eq!(probe_dimensions(&cover), None);

        let rebuilt = read_cover(&audio, &cache.root, CoverPriority::EmbeddedFirst).unwrap();
        assert_eq!(rebuilt, cover, "the key did not change, only the content");
        assert_eq!(probe_dimensions(&rebuilt), Some((6, 5)));
        assert_eq!(cache.names().len(), 1, "no temp file survives the rebuild");
    }

    /// Corrupt embedded picture bytes are never cached (upstream
    /// `corrupt_embedded_cover_data_is_not_cached`, metadata.rs:434-446):
    /// the store fails its decode check, the cache directory stays empty,
    /// and with no external file the composition yields nothing. The
    /// truncated bytes keep the PNG magic, which is exactly what lofty
    /// accepts into the tag (probed 2026-11-20 in the tag_write tests:
    /// lofty only sniffs the magic).
    #[test]
    fn corrupt_embedded_cover_data_is_not_cached() {
        let dir = TempDir::new("corrupt-bytes");
        let cache = TempDir::new("corrupt-bytes-cache");
        let audio = dir.path("song.mp3");
        fs::write(&audio, MP3_FIXTURE).unwrap();
        let mut bytes = png_bytes(8, 4);
        bytes.truncate(33);
        write_cover(&audio, &bytes);

        let cover = read_cover(&audio, &cache.root, CoverPriority::EmbeddedFirst);
        assert!(cover.is_none(), "{cover:?}");
        assert!(cache.names().is_empty(), "{:?}", cache.names());
    }

    /// The same-directory external face: conventional names are detected
    /// in list order, case-insensitively, and unlisted names are ignored
    /// (musid-only; no upstream counterpart).
    #[test]
    fn external_covers_are_detected_by_conventional_names() {
        let dir = TempDir::new("external");
        let wav = dir.path("song.wav");
        fs::write(&wav, wav_bytes()).unwrap();
        let cache = TempDir::new("external-cache");

        assert_eq!(external_cover_path(&wav), None, "nothing on disk yet");

        fs::write(dir.path("back.jpg"), png_bytes(2, 2)).unwrap();
        assert_eq!(
            external_cover_path(&wav),
            None,
            "back.jpg is not a candidate"
        );

        fs::write(dir.path("FOLDER.PNG"), png_bytes(4, 6)).unwrap();
        assert_eq!(
            external_cover_path(&wav),
            Some(dir.path("FOLDER.PNG")),
            "case-insensitive match keeps the on-disk name"
        );

        fs::write(dir.path("cover.jpg"), png_bytes(3, 3)).unwrap();
        assert_eq!(
            external_cover_path(&wav),
            Some(dir.path("cover.jpg")),
            "cover outranks folder in the candidate order"
        );
        assert_eq!(
            read_cover(&wav, &cache.root, CoverPriority::ExternalFirst),
            Some(dir.path("cover.jpg")),
            "the external face serves the directory file itself"
        );
        assert!(cache.names().is_empty(), "external files are never cached");
    }

    /// The priority is configurable and decides who wins when both sources
    /// exist; the losing source is the other one's fallback.
    #[test]
    fn cover_priority_decides_between_embedded_and_external() {
        let dir = TempDir::new("priority");
        let cache = TempDir::new("priority-cache");
        let audio = dir.path("song.mp3");
        fs::write(&audio, MP3_FIXTURE).unwrap();
        write_cover(&audio, &png_bytes(8, 4));
        fs::write(dir.path("folder.jpg"), png_bytes(5, 5)).unwrap();

        let embedded_first = read_cover(&audio, &cache.root, CoverPriority::EmbeddedFirst).unwrap();
        assert!(
            embedded_first.starts_with(&cache.root),
            "embedded wins under the default priority"
        );
        assert_eq!(
            external_cover_path(&audio),
            Some(dir.path("folder.jpg")),
            "the external file is untouched by the embedded resolution"
        );

        let external_first = read_cover(&audio, &cache.root, CoverPriority::ExternalFirst).unwrap();
        assert_eq!(
            external_first,
            dir.path("folder.jpg"),
            "the folder art wins under ExternalFirst"
        );
        assert_eq!(cache.names().len(), 1, "no new cache entry was written");
    }

    /// A failing cache directory (POSIX EACCES) degrades the embedded face
    /// to the external one instead of losing the cover entirely (the
    /// documented store-failure fallback; upstream returns None because it
    /// has no external face).
    #[test]
    #[cfg(unix)]
    fn a_failing_cache_dir_degrades_to_the_external_face() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("cache-fail");
        let cache = TempDir::new("cache-fail-dir");
        let audio = dir.path("song.mp3");
        fs::write(&audio, MP3_FIXTURE).unwrap();
        write_cover(&audio, &png_bytes(8, 4));
        fs::write(dir.path("cover.png"), png_bytes(3, 3)).unwrap();
        fs::set_permissions(&cache.root, fs::Permissions::from_mode(0o500)).unwrap();

        let cover = read_cover(&audio, &cache.root, CoverPriority::EmbeddedFirst).unwrap();
        assert_eq!(cover, dir.path("cover.png"), "external fallback served");

        // Without an external file the same failure yields nothing.
        fs::remove_file(dir.path("cover.png")).unwrap();
        assert_eq!(
            read_cover(&audio, &cache.root, CoverPriority::EmbeddedFirst),
            None
        );
        fs::set_permissions(&cache.root, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The store half validates before landing (upstream
    /// `a_corrupt_image_is_rejected_and_leaves_nothing_behind`,
    /// cover_cache.rs:248-258) and never overwrites a good cache file with
    /// a corrupt write (`a_failed_write_does_not_truncate_the_existing_cache`,
    /// cover_cache.rs:260-273).
    #[test]
    fn store_bytes_validates_before_landing() {
        let dir = TempDir::new("store");
        let target = dir.path("cover.jpg");
        let mut corrupt = png_bytes(20, 10);
        corrupt.truncate(33);

        let error = store_bytes(&target, &corrupt).unwrap_err();
        assert!(error.contains("corrupt"), "{error}");
        assert!(!target.exists());
        assert!(dir.names().is_empty(), "corrupt data leaves no temp file");

        // A failed store leaves the existing (valid) target untouched.
        store_bytes(&target, &png_bytes(4, 4)).unwrap();
        assert_eq!(
            store_bytes(&target, b"not an image").unwrap_err(),
            "cover image is corrupt"
        );
        assert_eq!(probe_dimensions(&target), Some((4, 4)));
        assert_eq!(dir.names(), vec!["cover.jpg".to_string()]);
    }

    /// store_bytes is the atomic_replace primitive: the cache target
    /// symlink refusal applies here too (T34 family; unix-gated).
    #[test]
    #[cfg(unix)]
    fn store_bytes_refuses_a_symbolic_link_target() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new("store-link");
        let real = dir.path("real.jpg");
        fs::write(&real, png_bytes(2, 2)).unwrap();
        let link = dir.path("link.jpg");
        symlink(&real, &link).unwrap();

        let error = store_bytes(&link, &png_bytes(4, 4)).unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(probe_dimensions(&real), Some((2, 2)), "target untouched");
    }
}
