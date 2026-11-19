//! Local library metadata and scanning are separate from online providers.
//!
//! The scan is the record producer of the measured audio specs
//! (`models::track::AudioSpec`, MODEL_ALIGNMENT_DESIGN.md §3.3): online
//! providers declare quality tiers, only real files expose real
//! measurements, so `read_metadata` fills `LocalMetadata.audio` from lofty
//! on every parsed file and hosts copy it into `Track.audio`.
//!
//! The read layer also closes parity L15 (LOCAL_ENHANCEMENT_DESIGN.md
//! §3.9): every read normalizes title/artist/album (trim plus whitespace
//! collapse) and backfills the fields the tags leave empty, title from the
//! file stem and artist/album from the unknown defaults, under an explicit
//! [`MetadataReadPolicy`] whose default is `TagFirst`.
//!
//! The lyrics face (s7, LOCAL_ENHANCEMENT_DESIGN.md §3.10, parity L17/L18)
//! is read-only: [`read_lyrics`] is the combined sidecar-first lookup with
//! the GB18030 decode fallback and [`read_embedded_lyrics`] reads the
//! embedded `ItemKey::Lyrics` tags; the write direction lives in
//! [`crate::tag_write`] (M5 slice s3).
use crate::models::AudioSpec;
use crate::{Error, Result};
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::properties::FileProperties;
use lofty::tag::{Accessor, ItemKey, Tag};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Read-layer priority between embedded tags and the file name (s6 item c).
/// The CUE face has its own container-layer priority, upstream
/// `scanner.rs:351-378` / `formats_cue.rs:235-267`: a sheet's track table
/// overwrites the referenced audio's metadata, so this policy only ever
/// decides the plain-audio read.
///
/// The policy only decides **which source wins a field**; normalization
/// (trim plus whitespace collapse, [`normalize_text`]) applies to every
/// stored value either way, and case/fullwidth folding never reaches stored
/// values: those folds live only in comparison keys
/// (`local_scan::duplicate_normalize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MetadataReadPolicy {
    /// Embedded tags win. A field the tags leave absent, empty or
    /// whitespace-only falls back: title from the file stem, artist and
    /// album from 未知艺术家 / 未知专辑 (upstream `metadata.rs:86-112`,
    /// parity L15; the stem placeholder is upstream `file_stem`,
    /// `metadata.rs:193-198`). The default.
    #[default]
    TagFirst,
    /// The file stem wins over the embedded tag title (hosts that trust
    /// their own file naming over embedded tags). When no usable stem
    /// exists the tag title is kept, and artist/album have no file-name
    /// source so they keep the [`MetadataReadPolicy::TagFirst`] fallback
    /// chain.
    FilenameFirst,
}

/// Field normalization shared by the read layer and the duplicate key
/// (s6 item b): trim plus Unicode whitespace collapse
/// (`split_whitespace().join(" ")`, the exact collapse the scanner's
/// duplicate key applies), so normalizing a value never changes its
/// duplicate key. `None` and values that collapse to empty (an explicit
/// empty string, whitespace-only text) both normalize to `None`: that is
/// the empty-string filter the backfill keys on, stricter than upstream
/// which filters exact-empty strings only. Case folding and
/// fullwidth-to-halfwidth folding are deliberately NOT here: they exist
/// only in comparison keys, never in stored values.
pub(crate) fn normalize_text(value: Option<&str>) -> Option<String> {
    let collapsed = value?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

/// Applies [`normalize_text`] to title/artist/album and backfills the
/// fields the tags leave empty, upstream `read_metadata`
/// (`metadata.rs:86-112`, parity L15). `format`, `duration_ms` and the
/// measured [`AudioSpec`] pass through untouched. The landing point is
/// between the raw lofty read ([`read_metadata_inner`]) and entry assembly
/// (LOCAL_ENHANCEMENT_DESIGN.md §3.9), so the scan core finalizes here too.
pub(crate) fn finalize_metadata(
    metadata: LocalMetadata,
    path: &Path,
    policy: MetadataReadPolicy,
) -> LocalMetadata {
    let title = normalize_text(metadata.title.as_deref());
    let artist = normalize_text(metadata.artist.as_deref());
    let album = normalize_text(metadata.album.as_deref());
    let stem = normalize_text(path.file_stem().and_then(OsStr::to_str));
    let title = match policy {
        MetadataReadPolicy::TagFirst => title.or(stem),
        MetadataReadPolicy::FilenameFirst => stem.or(title),
    };
    LocalMetadata {
        title: Some(title.unwrap_or_else(|| "未知歌曲".to_string())),
        artist: Some(artist.unwrap_or_else(|| "未知艺术家".to_string())),
        album: Some(album.unwrap_or_else(|| "未知专辑".to_string())),
        duration_ms: metadata.duration_ms,
        format: metadata.format,
        audio: metadata.audio,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: u64,
    pub format: Option<String>,
    /// Measured encoding parameters read by lofty. `Some` for every file
    /// the reader parses (upstream parity: the local reader always fills
    /// the slot, voicefox `source/src/local/metadata.rs:126`; individual
    /// fields stay `None` when the container does not carry them).
    pub audio: Option<AudioSpec>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTrack {
    pub path: PathBuf,
    pub metadata: LocalMetadata,
}

/// Local library interface; deliberately does not inherit MusicProvider.
pub trait LocalLibraryProvider: Send + Sync {
    fn scan(&self, root: &Path, recursive: bool) -> Result<Vec<LocalTrack>>;
    fn read_track(&self, path: &Path) -> Result<LocalTrack>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FilesystemLibrary;
impl LocalLibraryProvider for FilesystemLibrary {
    fn scan(&self, root: &Path, recursive: bool) -> Result<Vec<LocalTrack>> {
        scan_audio_files(root, recursive)?
            .iter()
            .map(|path| self.read_track(path))
            .collect()
    }
    fn read_track(&self, path: &Path) -> Result<LocalTrack> {
        Ok(LocalTrack {
            path: path.to_path_buf(),
            metadata: read_metadata(path)?,
        })
    }
}

pub(crate) const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "m4a", "mp4", "aac", "ogg", "opus", "wav", "wma", "ape", "aiff", "aif",
];

pub fn scan_audio_files(root: impl AsRef<Path>, recursive: bool) -> Result<Vec<PathBuf>> {
    let depth = if recursive { usize::MAX } else { 1 };
    walkdir::WalkDir::new(root)
        .max_depth(depth)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_file() => Some(Ok(entry.into_path())),
            Ok(_) => None,
            Err(error) => Some(Err(Error::Provider {
                provider: "local".into(),
                message: error.to_string(),
            })),
        })
        .filter_map(|entry| match entry {
            Ok(path)
                if path
                    .extension()
                    .and_then(|s| s.to_str())
                    .is_some_and(|ext| {
                        AUDIO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
                    }) =>
            {
                Some(Ok(path))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

pub fn read_metadata(path: impl AsRef<Path>) -> Result<LocalMetadata> {
    read_metadata_with_policy(path, MetadataReadPolicy::default())
}

/// Same read as [`read_metadata`] with an explicit [`MetadataReadPolicy`]
/// (s6 item c): both branches are reachable so the field priority is
/// observable behavior, not just a documented default. Signature and
/// `Error::Provider` wrapping match [`read_metadata`].
pub fn read_metadata_with_policy(
    path: impl AsRef<Path>,
    policy: MetadataReadPolicy,
) -> Result<LocalMetadata> {
    let path = path.as_ref();
    read_metadata_inner(path)
        .map(|metadata| finalize_metadata(metadata, path, policy))
        .map_err(local_error)
}

/// The raw lofty read [`read_metadata`] and the scan core build on: keeps
/// the lofty error type, so the scan core can classify failures (permission
/// vs IO vs parse) instead of seeing them flattened into `Error::Provider`
/// (LOCAL_ENHANCEMENT_DESIGN.md §3.3), and returns the verbatim tag values
/// with no backfill and no normalization. s6 keeps the container adapter
/// face (`formats_cue`) on this raw read, while [`read_metadata`]
/// finalizes on top of it (LOCAL_ENHANCEMENT_DESIGN.md §3.9).
pub(crate) fn read_metadata_inner(
    path: &Path,
) -> std::result::Result<LocalMetadata, lofty::error::LoftyError> {
    let tagged = Probe::open(path)?.read()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let audio = audio_spec_for(&tagged, tagged.properties());
    Ok(LocalMetadata {
        title: tag.and_then(|tag| tag.title().map(|s| s.to_string())),
        artist: tag.and_then(|tag| tag.artist().map(|s| s.to_string())),
        album: tag.and_then(|tag| tag.album().map(|s| s.to_string())),
        duration_ms: tagged
            .properties()
            .duration()
            .as_millis()
            .min(u64::MAX as u128) as u64,
        format: path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase),
        audio: Some(audio),
    })
}

/// Lofty container properties to the measured [`AudioSpec`] (§3.3 producer;
/// port of the upstream spec read, voicefox
/// `source/src/local/metadata.rs:74-83`).
fn audio_spec_for(tagged: &TaggedFile, properties: &FileProperties) -> AudioSpec {
    audio_spec_from_parts(
        tagged.file_type(),
        properties.audio_bitrate(),
        properties.sample_rate(),
        properties.bit_depth(),
    )
}

/// Assembles the spec from raw lofty values: lofty reports `0` for fields it
/// did not resolve, and those are filtered to `None` (upstream filters each
/// field, `metadata.rs:78-80`). `lossless` is judged from the **unfiltered**
/// depth: for MP4 the presence of a parsed depth field is the ALAC/FLAC codec
/// criterion and a parsed zero still counts as present (upstream comment
/// `metadata.rs:81-82, 144-151`).
fn audio_spec_from_parts(
    file_type: FileType,
    bitrate: Option<u32>,
    sample_rate: Option<u32>,
    bit_depth: Option<u8>,
) -> AudioSpec {
    AudioSpec {
        bitrate: bitrate.filter(|bitrate| *bitrate != 0),
        sample_rate: sample_rate.filter(|rate| *rate != 0),
        bit_depth: bit_depth.filter(|depth| *depth != 0),
        lossless: is_lossless(file_type, bit_depth),
    }
}

/// Container-based losslessness approximation, upstream `is_lossless`
/// (`metadata.rs:138-151`). WavPack hybrid mode, format-flagged non-PCM and
/// IEEE-float WAV and compressed AIFC are actually lossy and stay judged
/// lossless here: the exact flag needs a per-type re-parse and upstream
/// keeps the approximation.
fn is_lossless(file_type: FileType, bit_depth: Option<u8>) -> bool {
    match file_type {
        FileType::Flac | FileType::Ape | FileType::WavPack | FileType::Wav | FileType::Aiff => true,
        // AAC, ALAC and FLAC share the MP4 container; lofty writes bit depth
        // only in the ALAC/FLAC parse branches (`metadata.rs:147-148`).
        FileType::Mp4 => bit_depth.is_some(),
        _ => false,
    }
}
fn local_error(error: impl std::fmt::Display) -> Error {
    Error::Provider {
        provider: "local".into(),
        message: error.to_string(),
    }
}

// ───────────────── lyrics read (s7, parity L17/L18) ─────────────────

/// Reads the lyrics of a local audio file, sidecar first (s7 combined face,
/// the port of the upstream provider lookup `read_local_lyric`, voicefox
/// `source/src/local/mod.rs:822-830`): `audio.with_extension("lrc")` wins
/// when it exists and is not blank, otherwise the embedded tag lyrics.
///
/// - The sidecar content is returned verbatim (only blank text falls
///   through to the embedded branch, exactly like upstream; upstream also
///   does not trim it).
/// - The embedded content is trimmed and blank-filtered
///   ([`read_embedded_lyrics`]).
/// - Decoding: UTF-8 first, and bytes that are not valid UTF-8 fall back to
///   the GB18030 decoder (upstream `read_text_file`, `mod.rs:838-846`; the
///   warn log upstream sends through `tracing` has no sink under the
///   `local` feature, so the fallback is only documented here).
/// - `None` when neither source has usable lyrics. Read errors are
///   swallowed like upstream (`read_embedded_lyric(...).ok().flatten()`),
///   so the function never fails on an unreadable or non-audio path; use
///   [`read_embedded_lyrics`] directly when the lofty error matters.
///
/// The scan's opt-in face (`ScanOptions::read_lyrics`) runs this same
/// function per plain audio file, so host-side and scan-side lyrics agree.
pub fn read_lyrics(path: impl AsRef<Path>) -> Option<String> {
    let path = path.as_ref();
    let lrc_path = path.with_extension("lrc");
    if let Some(content) = read_sidecar_lyrics(&lrc_path)
        && !content.trim().is_empty()
    {
        return Some(content);
    }
    read_embedded_lyrics(path).ok().flatten()
}

/// Reads the lyrics embedded in the audio tags (parity L17, port of the
/// upstream `read_embedded_lyric`, voicefox
/// `source/src/local/metadata.rs:180-190`). Lofty maps ID3v2 USLT, Vorbis
/// `LYRICS`, MP4 lyrics and the other container equivalents onto one
/// `ItemKey::Lyrics`, so no per-container parsing is needed: the first tag
/// carrying a non-blank value wins (primary tags first, then the rest) and
/// the value is trimmed. Errors surface as `Error::Provider` like
/// [`read_metadata`].
pub fn read_embedded_lyrics(path: impl AsRef<Path>) -> Result<Option<String>> {
    let tagged = Probe::open(path.as_ref())
        .and_then(|probe| probe.read())
        .map_err(local_error)?;
    Ok(tagged.tags().iter().find_map(embedded_lyric_from_tag))
}

/// The pure tag-level half of [`read_embedded_lyrics`] (upstream
/// `embedded_lyric_from_tag`, `metadata.rs:188-190`): the first non-blank
/// `ItemKey::Lyrics` string in this tag, trimmed.
fn embedded_lyric_from_tag(tag: &Tag) -> Option<String> {
    tag.get_strings(&ItemKey::Lyrics)
        .map(str::trim)
        .find(|lyric| !lyric.is_empty())
        .map(ToOwned::to_owned)
}

/// Sidecar `.lrc` read (upstream `read_text_file`, voicefox
/// `source/src/local/mod.rs:838-846`): UTF-8 first; bytes that are not
/// valid UTF-8 fall back to the GB18030 decoder (a superset of GBK/GB2312,
/// so Chinese sidecars saved in legacy encodings still read).
/// `encoding_rs`' decode never fails: bytes invalid in both encodings come
/// back with U+FFFD replacements. `None` only when the file cannot be read.
fn read_sidecar_lyrics(lrc_path: &Path) -> Option<String> {
    let bytes = std::fs::read(lrc_path).ok()?;
    match String::from_utf8(bytes) {
        Ok(content) => Some(content),
        Err(error) => {
            let (content, _, _) = encoding_rs::GB18030.decode(error.as_bytes());
            Some(content.into_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn audio_extensions_are_case_insensitive() {
        assert!(AUDIO_EXTENSIONS.contains(&"flac"));
        assert!(AUDIO_EXTENSIONS.contains(&"opus"));
    }

    /// Committed ffmpeg-built spec fixtures (tests/fixtures/local, see the
    /// fixture README for the build commands and the probe ground truth).
    const MP3_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/local/spec_mp3_cbr128.mp3");
    const FLAC_16_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/local/spec_flac_16_441.flac");
    const FLAC_24_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/local/spec_flac_24_96.flac");
    const M4A_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/local/spec_m4a_aac.m4a");

    /// Hosts fixture bytes under their real names in a fresh temp directory;
    /// lofty probes by path, same pattern as the CUE stubs. Removed on drop.
    struct FixtureDir {
        root: PathBuf,
    }
    impl FixtureDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("musid-dli-local-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self {
                root: fs::canonicalize(dir).unwrap(),
            }
        }
        fn write(&self, name: &str, bytes: &[u8]) {
            fs::write(self.root.join(name), bytes).unwrap();
        }
        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
    }
    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Minimal PCM WAV stub: 44100 Hz, mono, 16-bit, 1 s. lofty reports
    /// `{706, 44100, Some(16), lossless(Wav)}` (88200 B/s → 705.6 → 706).
    fn write_wav_stub(dir: &FixtureDir, name: &str) {
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
        dir.write(name, &wav);
    }

    #[test]
    fn mp4_losslessness_is_decided_by_the_parsed_bit_depth() {
        assert!(is_lossless(FileType::Mp4, Some(24)));
        assert!(is_lossless(FileType::Mp4, Some(0)));
        assert!(!is_lossless(FileType::Mp4, None));
    }

    #[test]
    fn losslessness_outside_mp4_follows_the_container() {
        for lossless in [
            FileType::Flac,
            FileType::Ape,
            FileType::WavPack,
            FileType::Wav,
            FileType::Aiff,
        ] {
            assert!(is_lossless(lossless, None));
            assert!(is_lossless(lossless, Some(16)));
        }
        for lossy in [
            FileType::Mpeg,
            FileType::Opus,
            FileType::Vorbis,
            FileType::Aac,
        ] {
            assert!(!is_lossless(lossy, None));
            assert!(!is_lossless(lossy, Some(16)));
        }
    }

    #[test]
    fn unresolved_zero_fields_filter_out_but_keep_the_container_losslessness() {
        // An MP4 whose ALAC/FLAC branch parsed a zero depth: every field
        // filters to None while the container rule still judges lossless
        // (the upstream unfiltered-depth quirk, metadata.rs:81-82).
        let mp4_zero_depth = audio_spec_from_parts(FileType::Mp4, Some(0), Some(0), Some(0));
        assert_eq!(
            mp4_zero_depth,
            AudioSpec {
                bitrate: None,
                sample_rate: None,
                bit_depth: None,
                lossless: true,
            }
        );
        let mp3 = audio_spec_from_parts(FileType::Mpeg, Some(128), Some(44100), None);
        assert_eq!(
            mp3,
            AudioSpec {
                bitrate: Some(128),
                sample_rate: Some(44100),
                bit_depth: None,
                lossless: false,
            }
        );
        assert_eq!(mp3.label().as_deref(), Some("128K"));
    }

    #[test]
    fn fixture_mp3_carries_the_measured_lossy_spec() {
        let dir = FixtureDir::new("mp3-spec");
        dir.write("spec_mp3_cbr128.mp3", MP3_FIXTURE);
        let metadata = read_metadata(dir.path("spec_mp3_cbr128.mp3")).unwrap();
        assert_eq!(
            metadata.audio,
            Some(AudioSpec {
                bitrate: Some(128),
                sample_rate: Some(44100),
                bit_depth: None,
                lossless: false,
            })
        );
        assert_eq!(metadata.audio.unwrap().label().as_deref(), Some("128K"));
        assert_eq!(metadata.format.as_deref(), Some("mp3"));
    }

    #[test]
    fn fixture_flac_specs_carry_depth_rate_and_losslessness() {
        let dir = FixtureDir::new("flac-spec");
        dir.write("spec_flac_16_441.flac", FLAC_16_FIXTURE);
        dir.write("spec_flac_24_96.flac", FLAC_24_FIXTURE);
        let cd = read_metadata(dir.path("spec_flac_16_441.flac")).unwrap();
        assert_eq!(
            cd.audio,
            Some(AudioSpec {
                bitrate: Some(130),
                sample_rate: Some(44100),
                bit_depth: Some(16),
                lossless: true,
            })
        );
        assert_eq!(cd.audio.unwrap().label().as_deref(), Some("16/44.1"));
        let hires = read_metadata(dir.path("spec_flac_24_96.flac")).unwrap();
        assert_eq!(
            hires.audio,
            Some(AudioSpec {
                bitrate: Some(269),
                sample_rate: Some(96000),
                bit_depth: Some(24),
                lossless: true,
            })
        );
        assert_eq!(hires.audio.unwrap().label().as_deref(), Some("24/96"));
    }

    #[test]
    fn fixture_m4a_aac_stays_lossy_under_the_mp4_depth_heuristic() {
        let dir = FixtureDir::new("m4a-spec");
        dir.write("spec_m4a_aac.m4a", M4A_FIXTURE);
        let metadata = read_metadata(dir.path("spec_m4a_aac.m4a")).unwrap();
        assert_eq!(
            metadata.audio,
            Some(AudioSpec {
                bitrate: Some(119),
                sample_rate: Some(44100),
                bit_depth: None,
                lossless: false,
            })
        );
        assert_eq!(metadata.audio.unwrap().label().as_deref(), Some("119K"));
        assert_eq!(metadata.format.as_deref(), Some("m4a"));
    }

    #[test]
    fn scan_carries_the_audio_spec_on_every_track() {
        let dir = FixtureDir::new("scan-spec");
        dir.write("song.mp3", MP3_FIXTURE);
        write_wav_stub(&dir, "song.wav");
        dir.write("notes.txt", b"not audio"); // extension filter keeps it out
        let library = FilesystemLibrary;
        let tracks = library.scan(&dir.root, false).unwrap();
        assert_eq!(tracks.len(), 2);
        assert!(
            tracks.iter().all(|track| track.metadata.audio.is_some()),
            "the scan producer fills every parsed audio file"
        );
        let wav = tracks
            .iter()
            .find(|track| track.path.extension().and_then(|s| s.to_str()) == Some("wav"))
            .expect("wav stub scanned");
        assert_eq!(
            wav.metadata.audio,
            Some(AudioSpec {
                bitrate: Some(706),
                sample_rate: Some(44100),
                bit_depth: Some(16),
                lossless: true,
            })
        );
        // The scan entry and a single-file read share the one producer.
        let reread = library.read_track(&wav.path).unwrap();
        assert_eq!(reread.metadata, wav.metadata);
    }

    // ───────────────── s6: backfill and normalization ─────────────────

    use crate::tag_write::{MetadataEdit, write_metadata};

    use lofty::tag::TagType;

    /// Tags a runtime-hosted MP3 fixture through the library's own atomic
    /// write, so tagged-read tests are deterministic regardless of the
    /// fixture's pre-existing tags (every field is set explicitly).
    fn write_tags(path: &Path, title: &str, artist: &str, album: &str) {
        write_metadata(
            path,
            &MetadataEdit {
                title: Some(title.to_string()),
                artist: Some(artist.to_string()),
                album: Some(album.to_string()),
                cover: None,
                lyrics: None,
            },
        )
        .unwrap();
    }

    /// Upstream `metadata.rs:106-112`: a file without tags reads with the
    /// file stem as title and the unknown defaults, while the measured
    /// spec and container format pass through (parity L15).
    #[test]
    fn untagged_files_backfill_the_stem_and_the_unknown_defaults() {
        let dir = FixtureDir::new("backfill-untagged");
        write_wav_stub(&dir, "My Song.wav");
        let metadata = read_metadata(dir.path("My Song.wav")).unwrap();
        assert_eq!(metadata.title.as_deref(), Some("My Song"));
        assert_eq!(metadata.artist.as_deref(), Some("未知艺术家"));
        assert_eq!(metadata.album.as_deref(), Some("未知专辑"));
        assert_eq!(metadata.format.as_deref(), Some("wav"));
        assert!(metadata.audio.is_some());
    }

    /// Upstream filters exact-empty strings (`metadata.rs:90, 96, 102`);
    /// musid normalizes first (s6 item b), so whitespace-only fields count
    /// as empty and fall back too.
    #[test]
    fn empty_and_whitespace_only_tag_fields_fall_back() {
        let dir = FixtureDir::new("backfill-empty");
        dir.write("song.mp3", MP3_FIXTURE);
        write_tags(&dir.path("song.mp3"), "   ", " \t ", "");
        let metadata = read_metadata(dir.path("song.mp3")).unwrap();
        assert_eq!(metadata.title.as_deref(), Some("song"));
        assert_eq!(metadata.artist.as_deref(), Some("未知艺术家"));
        assert_eq!(metadata.album.as_deref(), Some("未知专辑"));
    }

    /// The policy's two branches are observable on one tagged file (s6
    /// item c): TagFirst keeps the tag title, FilenameFirst takes the
    /// stem; artist/album have no file-name source and agree.
    #[test]
    fn read_policy_decides_whether_the_stem_or_the_tag_wins_the_title() {
        let dir = FixtureDir::new("backfill-policy");
        dir.write("Tagged Song.mp3", MP3_FIXTURE);
        let path = dir.path("Tagged Song.mp3");
        write_tags(&path, "Tag Title", "Tag Artist", "Tag Album");

        let tag_first = read_metadata(&path).unwrap();
        assert_eq!(tag_first.title.as_deref(), Some("Tag Title"));
        assert_eq!(tag_first.artist.as_deref(), Some("Tag Artist"));
        assert_eq!(tag_first.album.as_deref(), Some("Tag Album"));

        let filename_first =
            read_metadata_with_policy(&path, MetadataReadPolicy::FilenameFirst).unwrap();
        assert_eq!(filename_first.title.as_deref(), Some("Tagged Song"));
        assert_eq!(filename_first.artist.as_deref(), Some("Tag Artist"));
        assert_eq!(filename_first.album.as_deref(), Some("Tag Album"));
    }

    /// Stored values get the duplicate-key whitespace collapse and nothing
    /// else (s6 item b): case and fullwidth letters survive the read.
    #[test]
    fn stored_fields_collapse_whitespace_but_never_fold_case_or_fullwidth() {
        let dir = FixtureDir::new("backfill-normalize");
        dir.write("song.mp3", MP3_FIXTURE);
        // The ideographic space folds into the stored value (it is
        // whitespace); the fullwidth letters do not.
        write_tags(
            &dir.path("song.mp3"),
            "  Ｍｙ　Ｓｏｎｇ  ",
            "Ａｒｔｉｓｔ",
            "B",
        );
        let metadata = read_metadata(dir.path("song.mp3")).unwrap();
        assert_eq!(metadata.title.as_deref(), Some("Ｍｙ Ｓｏｎｇ"));
        assert_eq!(metadata.artist.as_deref(), Some("Ａｒｔｉｓｔ"));
        assert_eq!(metadata.album.as_deref(), Some("B"));
    }

    #[test]
    fn normalize_text_collapses_and_filters_empty_values() {
        assert_eq!(normalize_text(None), None);
        assert_eq!(normalize_text(Some("")), None);
        assert_eq!(normalize_text(Some("  \t\n ")), None);
        assert_eq!(
            normalize_text(Some("  a \t b\nc ")),
            Some("a b c".to_string())
        );
        assert_eq!(normalize_text(Some("Ａ　Ｂ")), Some("Ａ Ｂ".to_string()));
    }

    /// Both policy branches over a stem-less path (`..` has no file stem),
    /// no file system involved.
    #[test]
    fn finalize_metadata_policy_branches_without_io() {
        let empty = LocalMetadata {
            title: None,
            artist: None,
            album: None,
            duration_ms: 0,
            format: None,
            audio: None,
        };
        // TagFirst: the stem fills an absent title.
        let filled = finalize_metadata(
            empty.clone(),
            Path::new("dir/My Song.flac"),
            MetadataReadPolicy::TagFirst,
        );
        assert_eq!(filled.title.as_deref(), Some("My Song"));
        assert_eq!(filled.artist.as_deref(), Some("未知艺术家"));
        assert_eq!(filled.album.as_deref(), Some("未知专辑"));
        // FilenameFirst with no usable stem: the tag title survives.
        let tagged = LocalMetadata {
            title: Some("Tag".to_string()),
            ..empty.clone()
        };
        let no_stem = finalize_metadata(tagged, Path::new(".."), MetadataReadPolicy::FilenameFirst);
        assert_eq!(no_stem.title.as_deref(), Some("Tag"));
        assert_eq!(no_stem.artist.as_deref(), Some("未知艺术家"));
        // Neither source exists: the placeholder closes the chain
        // (upstream `file_stem`'s 未知歌曲 fallback, metadata.rs:196).
        let bare = finalize_metadata(empty, Path::new(".."), MetadataReadPolicy::FilenameFirst);
        assert_eq!(bare.title.as_deref(), Some("未知歌曲"));
    }

    // ───────────────── s7: lyrics read (parity L17/L18) ─────────────────

    /// Writes only the embedded lyrics through the library's own atomic
    /// write (the s3 face the read tests round trip against).
    fn write_lyrics(path: &Path, lyrics: &str) {
        write_metadata(
            path,
            &MetadataEdit {
                title: None,
                artist: None,
                album: None,
                cover: None,
                lyrics: Some(lyrics.to_string()),
            },
        )
        .unwrap();
    }

    /// Upstream `metadata.rs:363-372`: the tag-level half trims the value
    /// and skips blank ones without any file I/O.
    #[test]
    fn embedded_lyric_from_tag_trims_and_skips_blank_values() {
        let mut tag = Tag::new(TagType::Id3v2);
        tag.insert_text(ItemKey::Lyrics, " \n[00:01.00]歌词\n ".to_string());
        assert_eq!(
            embedded_lyric_from_tag(&tag).as_deref(),
            Some("[00:01.00]歌词")
        );

        let mut blank = Tag::new(TagType::Id3v2);
        blank.insert_text(ItemKey::Lyrics, "  \t\n ".to_string());
        assert_eq!(embedded_lyric_from_tag(&blank), None);
        assert_eq!(embedded_lyric_from_tag(&Tag::new(TagType::Id3v2)), None);
    }

    /// Embedded lyrics round trip through the two tag systems the write
    /// side lands in: ID3v2 USLT (MP3 fixture) and Vorbis comments (FLAC
    /// fixture). Parity L17 (upstream pins the tag-level half only; this
    /// pins the lofty read through real files), plus the combined face
    /// returning the embedded text when no sidecar exists.
    #[test]
    fn reads_embedded_lyrics_from_id3v2_and_vorbis_tags() {
        let dir = FixtureDir::new("lyrics-embedded");
        let mp3 = dir.path("song.mp3");
        dir.write("song.mp3", MP3_FIXTURE);
        write_lyrics(&mp3, "[00:01.00]内嵌歌词");
        assert_eq!(
            read_embedded_lyrics(&mp3).unwrap().as_deref(),
            Some("[00:01.00]内嵌歌词")
        );
        assert_eq!(read_lyrics(&mp3).as_deref(), Some("[00:01.00]内嵌歌词"));

        let flac = dir.path("song.flac");
        dir.write("song.flac", FLAC_16_FIXTURE);
        write_lyrics(&flac, "[00:02.00]无损歌词");
        assert_eq!(
            read_embedded_lyrics(&flac).unwrap().as_deref(),
            Some("[00:02.00]无损歌词")
        );
    }

    /// Priority pins the upstream order (`read_local_lyric`,
    /// `mod.rs:822-830`): a non-blank sidecar wins verbatim (no trim), a
    /// blank sidecar falls through to the embedded text, and neither
    /// source leaves `None`.
    #[test]
    fn sidecar_lyrics_win_over_embedded_and_blank_sidecars_fall_through() {
        let dir = FixtureDir::new("lyrics-priority");
        let mp3 = dir.path("song.mp3");
        dir.write("song.mp3", MP3_FIXTURE);
        write_lyrics(&mp3, "[00:01.00]内嵌歌词");
        let lrc = dir.path("song.lrc");

        fs::write(&lrc, "[00:01.00]外挂歌词\n[00:02.00]第二行\n").unwrap();
        assert_eq!(
            read_lyrics(&mp3).as_deref(),
            Some("[00:01.00]外挂歌词\n[00:02.00]第二行\n")
        );

        fs::write(&lrc, "  \n\t ").unwrap();
        assert_eq!(read_lyrics(&mp3).as_deref(), Some("[00:01.00]内嵌歌词"));

        fs::remove_file(&lrc).unwrap();
        let bare = dir.path("bare.wav");
        write_wav_stub(&dir, "bare.wav");
        assert_eq!(read_lyrics(&bare), None);
    }

    /// Sidecar decode fallback (parity L18, upstream `read_text_file`,
    /// `mod.rs:838-846`): UTF-8 content decodes as UTF-8 (the GB18030
    /// reinterpretation of the same bytes would be mojibake, so the exact
    /// round trip proves the order), non-UTF-8 GB18030 bytes decode
    /// through the fallback, and the Chinese text survives both paths.
    #[test]
    fn sidecar_lrc_decodes_utf8_first_and_falls_back_to_gb18030() {
        let dir = FixtureDir::new("lyrics-sidecar");
        let wav = dir.path("song.wav");
        write_wav_stub(&dir, "song.wav");
        let lrc = dir.path("song.lrc");

        let utf8_text = "[00:01.00]月光之歌";
        fs::write(&lrc, utf8_text.as_bytes()).unwrap();
        assert_eq!(read_lyrics(&wav).as_deref(), Some(utf8_text));

        let (gb_bytes, _, _) = encoding_rs::GB18030.encode("[00:01.00]月光之歌");
        assert!(String::from_utf8(gb_bytes.to_vec()).is_err());
        fs::write(&lrc, &gb_bytes).unwrap();
        assert_eq!(read_lyrics(&wav).as_deref(), Some("[00:01.00]月光之歌"));
    }
}
