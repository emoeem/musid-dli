//! CUE sheet container adapter (local feature).
//!
//! A CUE sheet is a metadata description that splits one or more audio files
//! into logical tracks by `INDEX 01` time offsets. The parsing behavior
//! mirrors the upstream VoiceFox local scanner (voicefox
//! `source/src/local/scanner.rs`, the sole source of truth for this slice):
//!
//! * the sheet is read as text, UTF-8 first, with a GB18030 fallback decode
//!   when the bytes are not valid UTF-8 (which also handles UTF-16 BOMs
//!   through `encoding_rs` BOM sniffing); undecodable bytes are an error;
//! * `cue-rw` parses standards-compliant sheets; a small permissive legacy
//!   line parser covers sheets the crate rejects (missing global
//!   TITLE/PERFORMER, non-standard indentation);
//! * a track starts at its INDEX 01 and ends at the next INDEX 01 inside the
//!   same FILE, otherwise at the referenced audio's duration. INDEX times use
//!   the strict CUE grammar (seconds < 60, 75 frames per second);
//! * every track keeps the mapping to the audio file its FILE command names.
//!
//! There is no byte-level track splitting upstream: playback seeks into the
//! referenced audio by the track's start offset, and the track length is
//! metadata only.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use cue_rw::CUEFile;

use crate::Result;
use crate::error::Error;
use crate::formats::{ContainerFormatHandler, ContainerMetadata, ContainerTrack};
use crate::local::read_metadata_inner;
use crate::models::AudioFormat;

/// CUE container handler.
///
/// Upstream keeps CUE splitting purely at the metadata level: playback seeks
/// into the referenced audio by the track's start offset and the track length
/// is informational. There is no byte-level track cutting, so
/// `extract_audio_to` streams the referenced audio file verbatim and callers
/// apply the offsets reported by `inspect_tracks`.
#[derive(Debug, Default, Clone, Copy)]
pub struct CueHandler;

impl ContainerFormatHandler for CueHandler {
    fn id(&self) -> &str {
        "cue"
    }

    fn extensions(&self) -> &[&str] {
        &["cue"]
    }

    /// Metadata of the audio file named by the sheet's first FILE command
    /// (the sheet itself carries no audio). The per-track table with time
    /// offsets and source mapping is reported by `inspect_tracks`.
    fn inspect(&self, path: &Path) -> Result<ContainerMetadata> {
        let audio = first_referenced_audio(path)?;
        audio_metadata(&audio)
    }

    /// Streams the sheet's first referenced audio file verbatim; per-track
    /// offsets are applied by the caller (upstream has no byte-level split).
    fn extract_audio_to(&self, path: &Path, output: &mut dyn Write) -> Result<ContainerMetadata> {
        let audio = first_referenced_audio(path)?;
        let metadata = audio_metadata(&audio)?;
        let mut input = fs::File::open(&audio)?;
        io::copy(&mut input, output)?;
        Ok(metadata)
    }

    /// One entry per logical track: INDEX 01 offsets, per-track lengths and
    /// the mapping to each track's audio file. Sheets without a usable track
    /// are an error, matching upstream.
    fn inspect_tracks(&self, path: &Path) -> Result<Vec<ContainerTrack>> {
        read_cue_tracks(path)
    }
}

/// Resolves the audio file named by the sheet's first FILE command.
fn first_referenced_audio(cue_path: &Path) -> Result<PathBuf> {
    let content = read_cue_text(cue_path)?;
    let name = match CUEFile::try_from(content.as_str()) {
        Ok(cue) => cue.files.first().map(|file| decode_cue_text(file)),
        Err(_) => first_legacy_file(&content),
    };
    let Some(name) = name else {
        return Err(cue_error("CUE 未包含 FILE 指令"));
    };
    let referenced = cue_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(name);
    Ok(referenced.canonicalize().unwrap_or(referenced))
}

/// First FILE command of a sheet the crate parser rejected.
fn first_legacy_file(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let (command, rest) = line.trim().split_once(char::is_whitespace)?;
        if command.eq_ignore_ascii_case("FILE") {
            cue_value(rest.trim())
        } else {
            None
        }
    })
}

/// Maps the referenced audio's own metadata into the container view. The
/// adapter face stays on the raw read (`read_metadata_inner`): the s6
/// backfill and normalization live in the read-entry layer
/// (`local::read_metadata`, `local_scan` entry assembly), so this view
/// still reports the verbatim tag values (pinned by the `inspect` test:
/// an untagged stub reads with `title == None`).
fn audio_metadata(audio: &Path) -> Result<ContainerMetadata> {
    let base = read_metadata_inner(audio).map_err(|error| cue_audio_error(audio, &error))?;
    Ok(ContainerMetadata {
        title: base.title,
        artists: base.artist.map(|artist| vec![artist]).unwrap_or_default(),
        album: base.album,
        duration_ms: Some(base.duration_ms),
        audio_format: audio_format_for(audio),
        artwork: None,
    })
}

/// Maps an audio file extension onto the shared audio format enum.
fn audio_format_for(path: &Path) -> AudioFormat {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp3") => AudioFormat::Mp3,
        Some("flac") => AudioFormat::Flac,
        Some("aac") => AudioFormat::Aac,
        Some("m4a" | "mp4") => AudioFormat::M4a,
        Some("opus") => AudioFormat::Opus,
        Some("ogg") => AudioFormat::Ogg,
        Some("wav") => AudioFormat::Wav,
        _ => AudioFormat::Unknown,
    }
}

/// Reads the sheet as text. Some full-album CUEs are GBK-encoded; when the
/// bytes are not valid UTF-8 they are re-decoded as GB18030 so a sheet is not
/// rejected for its encoding alone (`encoding_rs` BOM sniffing also handles
/// UTF-16 encoded sheets).
fn read_cue_text(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|error| cue_read_error(path, &error))?;
    match String::from_utf8(bytes) {
        Ok(content) => Ok(content),
        Err(error) => {
            // `FromUtf8Error::as_bytes` returns the full original bytes.
            let (content, _, had_errors) = encoding_rs::GB18030.decode(error.as_bytes());
            if had_errors {
                return Err(cue_error(format!(
                    "读取 CUE 失败: {}: 既不是 UTF-8 也无法按 GB18030 解码",
                    path.display()
                )));
            }
            Ok(content.into_owned())
        }
    }
}

/// Splits the sheet into its logical tracks. Unrecognized commands are
/// ignored by the legacy parser to stay compatible with extra REM metadata.
fn read_cue_tracks(path: &Path) -> Result<Vec<ContainerTrack>> {
    let content = read_cue_text(path)?;

    // `cue-rw` handles quoted paths, multiple FILE sections, comments, track
    // flags and the 75-frames-per-second timestamp grammar. The fallback
    // parser below keeps compatibility with older sheets that omit optional
    // album fields or use non-standard indentation rejected by the crate.
    if let Ok(cue) = CUEFile::try_from(content.as_str()) {
        return read_cue_tracks_with_crate(path, &cue);
    }

    read_cue_tracks_legacy(path, &content)
}

fn read_cue_tracks_with_crate(cue_path: &Path, cue: &CUEFile) -> Result<Vec<ContainerTrack>> {
    if cue.tracks.is_empty() {
        return Err(cue_error("CUE 未包含曲目"));
    }

    // Resolve and read every referenced audio up front: upstream fails the
    // whole sheet when any referenced file is unreadable (no partial split).
    let mut file_metadata = Vec::with_capacity(cue.files.len());
    for file in &cue.files {
        let referenced = cue_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(decode_cue_text(file));
        let referenced = referenced.canonicalize().unwrap_or(referenced);
        let base = read_metadata_inner(&referenced)
            .map_err(|error| cue_audio_error(&referenced, &error))?;
        file_metadata.push((referenced, base));
    }

    // CUEFile intentionally does not retain the numeric TRACK token. Track
    // order is the canonical identity for a sheet, and numbering is normally
    // sequential; use two digits to match conventional CUE identifiers.
    let mut result = Vec::with_capacity(cue.tracks.len());
    for (index, (file_id, track)) in cue.tracks.iter().enumerate() {
        let Some((referenced, base)) = file_metadata.get(*file_id) else {
            return Err(cue_error(format!(
                "CUE 曲目引用了不存在的 FILE #{}",
                file_id + 1
            )));
        };
        let Some((_, start_stamp)) = track.indices.iter().find(|(number, _)| *number == 1) else {
            continue;
        };
        let start = parse_cue_time(&start_stamp.to_string())
            .ok_or_else(|| cue_error(format!("CUE 曲目 {} 的 INDEX 01 无效", index + 1)))?;

        // A track ends at the next INDEX 01 inside the same FILE. If the next
        // track belongs to another file, use this file's duration.
        let end = cue
            .tracks
            .iter()
            .skip(index + 1)
            .find_map(|(next_file_id, next_track)| {
                (*next_file_id == *file_id).then(|| {
                    next_track
                        .indices
                        .iter()
                        .find(|(number, _)| *number == 1)
                        .and_then(|(_, stamp)| parse_cue_time(&stamp.to_string()))
                })
            })
            .flatten()
            .unwrap_or(Duration::from_millis(base.duration_ms));

        let number = format!("{:02}", index + 1);
        let title = decode_cue_text(&track.title);
        let performer = track
            .performer
            .as_deref()
            .map(decode_cue_text)
            .unwrap_or_else(|| decode_cue_text(&cue.performer));
        let album = decode_cue_text(&cue.title);
        let album = if album.trim().is_empty() {
            cue_path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("未知专辑")
                .to_string()
        } else {
            album
        };
        result.push(ContainerTrack {
            title: Some(if title.trim().is_empty() {
                format!("Track {number}")
            } else {
                title
            }),
            artists: vec![if performer.trim().is_empty() {
                "未知艺术家".to_string()
            } else {
                performer
            }],
            album: Some(album),
            start_ms: duration_ms(start),
            duration_ms: duration_ms(end.saturating_sub(start)),
            audio_path: referenced.clone(),
            track_number: number,
        });
    }

    if result.is_empty() {
        return Err(cue_error("CUE 没有带 INDEX 01 的曲目"));
    }
    Ok(result)
}

/// One track block of a sheet handled by the legacy parser.
#[derive(Default)]
struct LegacyCueTrack {
    number: String,
    title: Option<String>,
    performer: Option<String>,
    start: Option<Duration>,
    /// Index into the FILE entries declared before this track.
    file_index: Option<usize>,
}

/// Compatibility parser for permissive sheets which `cue-rw` intentionally
/// rejects (for example sheets without global TITLE/PERFORMER). Kept isolated
/// so the standards-compliant path remains delegated to the crate.
fn read_cue_tracks_legacy(path: &Path, content: &str) -> Result<Vec<ContainerTrack>> {
    let mut referenced = Vec::new();
    let mut current_file_index = None;
    let mut album = None;
    let mut album_performer = None;
    let mut tracks = Vec::new();
    let mut current: Option<LegacyCueTrack> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("REM ") {
            continue;
        }
        let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let value = rest.trim();
        match command.to_ascii_uppercase().as_str() {
            "FILE" => {
                if let Some(name) = cue_value(value) {
                    let candidate = path.parent().unwrap_or_else(|| Path::new(".")).join(name);
                    referenced.push(candidate);
                    current_file_index = Some(referenced.len() - 1);
                }
            }
            "TITLE" => {
                if let Some(track) = current.as_mut() {
                    track.title = cue_value(value);
                } else {
                    album = cue_value(value);
                }
            }
            "PERFORMER" => {
                if let Some(track) = current.as_mut() {
                    track.performer = cue_value(value);
                } else {
                    album_performer = cue_value(value);
                }
            }
            "TRACK" => {
                if let Some(track) = current.take() {
                    tracks.push(track);
                }
                let number = value.split_whitespace().next().unwrap_or("00");
                current = Some(LegacyCueTrack {
                    number: number.to_string(),
                    file_index: current_file_index,
                    ..Default::default()
                });
            }
            "INDEX" if current.is_some() => {
                let mut parts = value.split_whitespace();
                if parts.next().and_then(|number| number.parse::<u8>().ok()) == Some(1)
                    && let Some(time) = parts.next()
                {
                    current.as_mut().unwrap().start = parse_cue_time(time);
                }
            }
            _ => {}
        }
    }
    if let Some(track) = current {
        tracks.push(track);
    }
    if referenced.is_empty() {
        return Err(cue_error("CUE 未包含 FILE 指令"));
    }
    if tracks.is_empty() {
        return Err(cue_error("CUE 未包含 TRACK 指令"));
    }

    // Read each referenced file once. Multi-file sheets are common for cues
    // generated by ripping tools; each track keeps the FILE it belongs to.
    let file_metadata = referenced
        .into_iter()
        .map(|referenced| {
            let referenced = referenced.canonicalize().unwrap_or(referenced);
            let base = read_metadata_inner(&referenced)
                .map_err(|error| cue_audio_error(&referenced, &error))?;
            Ok::<_, Error>((referenced, base))
        })
        .collect::<Result<Vec<_>>>()?;
    let album = album.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("未知专辑")
            .to_string()
    });
    let performer = album_performer.unwrap_or_else(|| "未知艺术家".to_string());

    let mut result = Vec::new();
    for (index, track) in tracks.iter().enumerate() {
        let Some(start) = track.start else { continue };
        let Some(file_index) = track.file_index else {
            // TRACK before FILE is invalid CUE. Ignore that block while
            // retaining any subsequent valid tracks.
            continue;
        };
        let Some((referenced, base)) = file_metadata.get(file_index) else {
            return Err(cue_error(format!(
                "CUE 曲目 {} 引用了不存在的 FILE #{}",
                track.number,
                file_index + 1
            )));
        };
        // A track ends at the next INDEX 01 in the same FILE. If the next
        // track belongs to another file, use this file's duration.
        let end = tracks
            .iter()
            .skip(index + 1)
            .find(|next| next.file_index == Some(file_index))
            .and_then(|next| next.start)
            .unwrap_or(Duration::from_millis(base.duration_ms));
        result.push(ContainerTrack {
            title: Some(
                track
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("Track {}", track.number)),
            ),
            artists: vec![track.performer.clone().unwrap_or_else(|| performer.clone())],
            album: Some(album.clone()),
            start_ms: duration_ms(start),
            duration_ms: duration_ms(end.saturating_sub(start)),
            audio_path: referenced.clone(),
            track_number: track.number.clone(),
        });
    }
    if result.is_empty() {
        return Err(cue_error("CUE 未包含带 INDEX 01 的曲目"));
    }
    Ok(result)
}

/// Quoted CUE value: the two permitted escapes stay encoded (decoded by
/// `decode_cue_text`); an unterminated quote yields `None`.
fn cue_value(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(body) = value.strip_prefix('"') {
        let mut escaped = false;
        for (index, ch) in body.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                return Some(decode_cue_text(&body[..index]));
            }
        }
        return None;
    }
    (!value.is_empty()).then(|| value.split_whitespace().next().unwrap_or(value).to_string())
}

/// Decode the two escapes permitted in quoted CUE strings (`\"` and `\\`).
/// `cue-rw` intentionally preserves escapes in its owned strings, so this
/// adapter keeps the values it returns human-readable.
fn decode_cue_text(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('"') => decoded.push('"'),
                Some('\\') => decoded.push('\\'),
                Some(other) => {
                    decoded.push('\\');
                    decoded.push(other);
                }
                None => decoded.push('\\'),
            }
        } else {
            decoded.push(ch);
        }
    }
    decoded
}

/// INDEX times are `MM:SS:FF` with 75 frames per second; the frame count is
/// truncated to whole nanoseconds (`frames * 1e9 / 75`). Stricter than
/// `cue-rw` (which only requires each field < 100), matching upstream.
fn parse_cue_time(value: &str) -> Option<Duration> {
    let mut parts = value.split(':');
    let minutes = parts.next()?.parse::<u64>().ok()?;
    let seconds = parts.next()?.parse::<u64>().ok()?;
    let frames = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    (seconds < 60 && frames < 75).then(|| {
        Duration::from_secs(minutes * 60 + seconds)
            + Duration::from_nanos(frames * 1_000_000_000 / 75)
    })
}

/// `Duration::as_millis` narrowed to `u64` (mirrors `local::read_metadata`).
fn duration_ms(value: Duration) -> u64 {
    value.as_millis().min(u64::MAX as u128) as u64
}

fn cue_error(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: "local".into(),
        message: message.into(),
    }
}

fn cue_read_error(path: &Path, error: impl std::fmt::Display) -> Error {
    cue_error(format!("读取 CUE 失败: {}: {error}", path.display()))
}

fn cue_audio_error(path: &Path, error: impl std::fmt::Display) -> Error {
    cue_error(format!("读取 CUE 音频失败 ({}): {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const UTF8_SHEET: &str = include_str!("../tests/fixtures/cue/utf8_basic.cue");
    const GBK_SHEET: &[u8] = include_bytes!("../tests/fixtures/cue/gbk_sheet.cue");
    const LEGACY_SHEET: &str = include_str!("../tests/fixtures/cue/legacy_sheet.cue");
    const MULTI_FILE_SHEET: &str = include_str!("../tests/fixtures/cue/multi_file.cue");
    const CATALOG_OK: &str = include_str!("../tests/fixtures/cue/catalog_ok.cue");
    const CATALOG_SHORT: &str = include_str!("../tests/fixtures/cue/catalog_short.cue");
    const INDEX_OUT_OF_RANGE: &str =
        include_str!("../tests/fixtures/cue/index_seconds_out_of_range.cue");
    const MISSING_AUDIO: &str = include_str!("../tests/fixtures/cue/missing_audio.cue");

    /// Minimal PCM WAV stub with a byte-exact length so `lofty` reports the
    /// duration the audit vectors used as their stub value (1000 Hz, mono,
    /// 16-bit → 2000 bytes per second).
    fn write_wav(dir: &Path, name: &str, duration_ms: u64) {
        const BYTES_PER_SECOND: u32 = 2_000;
        let data_len = (duration_ms * u64::from(BYTES_PER_SECOND) / 1_000) as u32;
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
        fs::write(dir.join(name), &wav).unwrap();
    }

    /// Hosts a sheet and its synthesized audio stubs in a fresh temp
    /// directory; the directory is removed when dropped.
    struct CueFixture {
        root: PathBuf,
    }

    impl CueFixture {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("musid-dli-cue-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self {
                root: fs::canonicalize(dir).unwrap(),
            }
        }

        /// Writes an audio stub with the given duration next to the sheet.
        fn audio(&self, name: &str, duration_ms: u64) {
            write_wav(&self.root, name, duration_ms);
        }

        fn write_cue(&self, name: &str, content: &str) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, content).unwrap();
            path
        }

        fn write_cue_bytes(&self, name: &str, content: &[u8]) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for CueFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn utf8_sheet_splits_tracks_by_index_01() {
        let fx = CueFixture::new("utf8-basic");
        fx.audio("album one.wav", 400_000);
        let cue = fx.write_cue("utf8_basic.cue", UTF8_SHEET);

        let tracks = CueHandler.inspect_tracks(&cue).unwrap();

        assert_eq!(tracks.len(), 3);
        // Vector V1: start_ms 0/62493/210986, dur_ms 62493/148493/189013 —
        // truncation happens on the Duration difference, not the endpoints.
        assert_eq!(tracks[0].start_ms, 0);
        assert_eq!(tracks[0].duration_ms, 62_493);
        assert_eq!(tracks[1].start_ms, 62_493);
        assert_eq!(tracks[1].duration_ms, 148_493);
        assert_eq!(tracks[2].start_ms, 210_986);
        assert_eq!(tracks[2].duration_ms, 189_013);
        // cue-rw preserves the two permitted escapes; the adapter decodes them.
        assert_eq!(tracks[0].title.as_deref(), Some("First \"quoted\" title"));
        assert_eq!(tracks[1].title.as_deref(), Some("Back\\slash path"));
        assert_eq!(tracks[2].title.as_deref(), Some("Track 03"));
        // track performer wins; the album performer is the fallback
        assert_eq!(tracks[0].artists, ["Various"]);
        assert_eq!(tracks[1].artists, ["Track Artist"]);
        assert_eq!(tracks[2].artists, ["Various"]);
        assert_eq!(tracks[0].album.as_deref(), Some("Escape Test Album"));
        // cue-rw drops the TRACK token; numbering is positional, two digits
        assert_eq!(tracks[0].track_number, "01");
        assert_eq!(tracks[1].track_number, "02");
        assert_eq!(tracks[2].track_number, "03");
        // every track maps back to the audio file its FILE names
        for track in &tracks {
            assert_eq!(track.audio_path, fx.root.join("album one.wav"));
        }
    }

    #[test]
    fn gbk_sheet_decodes_through_gb18030_fallback() {
        let fx = CueFixture::new("gbk");
        fx.audio("album gbk.wav", 300_000);
        let cue = fx.write_cue_bytes("gbk_sheet.cue", GBK_SHEET);

        let tracks = CueHandler.inspect_tracks(&cue).unwrap();

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title.as_deref(), Some("第一軌"));
        assert_eq!(tracks[1].title.as_deref(), Some("第二軌"));
        assert_eq!(tracks[0].album.as_deref(), Some("天賦專輯"));
        assert_eq!(tracks[1].artists, ["測試歌手"]);
        // Vector V2: 02:00:37 → 120493 ms, remainder of the 300000 ms audio
        assert_eq!(tracks[1].start_ms, 120_493);
        assert_eq!(tracks[1].duration_ms, 179_506);
    }

    #[test]
    fn undecodable_bytes_fail_instead_of_faking_a_decode() {
        let fx = CueFixture::new("garbage");
        // Vector V2b: not UTF-8, and the trailing byte breaks the UTF-16 unit
        // the BOM sniffing starts, so GB18030 reports errors as well.
        let cue = fx.write_cue_bytes("garbage.cue", &[0xFF, 0xFE, 0x81, 0x39, 0x80]);

        let error = CueHandler.inspect_tracks(&cue).unwrap_err().to_string();

        assert!(
            error.contains("既不是 UTF-8 也无法按 GB18030 解码"),
            "{error}"
        );
    }

    #[test]
    fn utf16le_sheet_decodes_via_bom_sniffing() {
        let fx = CueFixture::new("utf16le");
        fx.audio("album gbk.wav", 300_000);
        // Vector V9: the same sheet encoded as UTF-16LE with a BOM; the
        // GB18030 fallback decoder sniffs the BOM and decodes it correctly.
        let (text, _, had_errors) = encoding_rs::GB18030.decode(GBK_SHEET);
        assert!(!had_errors);
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let cue = fx.write_cue_bytes("utf16le.cue", &bytes);

        let tracks = CueHandler.inspect_tracks(&cue).unwrap();

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title.as_deref(), Some("第一軌"));
    }

    #[test]
    fn legacy_sheet_falls_back_to_the_line_parser() {
        let fx = CueFixture::new("legacy");
        fx.audio("legacy album.wav", 600_000);
        let cue = fx.write_cue("legacy_sheet.cue", LEGACY_SHEET);

        let tracks = CueHandler.inspect_tracks(&cue).unwrap();

        assert_eq!(tracks.len(), 2);
        // Vector V3: INDEX 00 is ignored; 00:00:37 = 37 frames = 493 ms and
        // 04:12:10 = 252133 ms; lengths truncate on the Duration difference.
        assert_eq!(tracks[0].start_ms, 493);
        assert_eq!(tracks[0].duration_ms, 251_640);
        assert_eq!(tracks[1].start_ms, 252_133);
        assert_eq!(tracks[1].duration_ms, 347_866);
        // the legacy parser keeps the declared TRACK tokens verbatim
        assert_eq!(tracks[0].track_number, "01");
        assert_eq!(tracks[1].track_number, "02");
        // no global TITLE/PERFORMER: "Track NN" titles, unknown-artist
        // fallback, album falls back to the CUE file stem
        assert_eq!(tracks[0].title.as_deref(), Some("Track 01"));
        assert_eq!(tracks[1].title.as_deref(), Some("Track 02"));
        assert_eq!(tracks[0].artists, ["未知艺术家"]);
        assert_eq!(tracks[0].album.as_deref(), Some("legacy_sheet"));
    }

    #[test]
    fn multi_file_sheet_maps_each_track_to_its_file() {
        let fx = CueFixture::new("multi-file");
        fx.audio("side-a.wav", 400_000);
        fx.audio("side-b.wav", 200_000);
        let cue = fx.write_cue("multi_file.cue", MULTI_FILE_SHEET);

        let tracks = CueHandler.inspect_tracks(&cue).unwrap();

        assert_eq!(tracks.len(), 4);
        // Vector V6: every track belongs to the FILE it was declared under.
        assert_eq!(tracks[0].audio_path, fx.root.join("side-a.wav"));
        assert_eq!(tracks[1].audio_path, fx.root.join("side-a.wav"));
        assert_eq!(tracks[2].audio_path, fx.root.join("side-b.wav"));
        assert_eq!(tracks[3].audio_path, fx.root.join("side-b.wav"));
        assert_eq!(tracks[0].track_number, "01");
        assert_eq!(tracks[3].track_number, "04");
        assert_eq!(tracks[0].start_ms, 0);
        assert_eq!(tracks[0].duration_ms, 150_000);
        // A Two ends at side-a's own duration (400000 ms), not at B One's
        // INDEX across the FILE boundary.
        assert_eq!(tracks[1].start_ms, 150_000);
        assert_eq!(tracks[1].duration_ms, 250_000);
        // 00:00:10 = 10 frames = 133 ms.
        assert_eq!(tracks[2].start_ms, 133);
        assert_eq!(tracks[2].duration_ms, 60_360);
        assert_eq!(tracks[3].start_ms, 60_493);
        assert_eq!(tracks[3].duration_ms, 139_506);
    }

    #[test]
    fn catalog_validation_selects_the_parser_path() {
        // Vector V7: a 13-digit CATALOG is accepted → cue-rw path →
        // positional numbering replaces the declared TRACK 07 token.
        let fx = CueFixture::new("catalog-ok");
        fx.audio("catalog album.wav", 120_000);
        let ok = fx.write_cue("catalog_ok.cue", CATALOG_OK);
        let tracks = CueHandler.inspect_tracks(&ok).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_number, "01");
        assert_eq!(tracks[0].title.as_deref(), Some("Only Track"));
        assert_eq!(tracks[0].start_ms, 60_000);
        assert_eq!(tracks[0].duration_ms, 60_000);

        // A 5-digit CATALOG is rejected by cue-rw → legacy parser → the
        // TRACK token survives verbatim.
        let fx = CueFixture::new("catalog-short");
        fx.audio("catalog album.wav", 120_000);
        let short = fx.write_cue("catalog_short.cue", CATALOG_SHORT);
        let tracks = CueHandler.inspect_tracks(&short).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_number, "07");
        assert_eq!(tracks[0].title.as_deref(), Some("Only Track"));
    }

    #[test]
    fn index_seconds_out_of_range_fail_without_legacy_fallback() {
        // Vector V4b: cue-rw accepts 99 seconds (<100) but the strict CUE
        // time parser rejects them; the sheet fails as a whole instead of
        // silently re-parsing through the permissive legacy path.
        let fx = CueFixture::new("index-range");
        fx.audio("album one.wav", 400_000);
        let cue = fx.write_cue("index_seconds_out_of_range.cue", INDEX_OUT_OF_RANGE);

        let error = CueHandler.inspect_tracks(&cue).unwrap_err().to_string();

        assert!(error.contains("INDEX 01 无效"), "{error}");
    }

    #[test]
    fn legacy_path_drops_tracks_with_invalid_index_times() {
        // cue-rw rejects sheets whose tracks lack TITLE, so an out-of-range
        // INDEX in such a sheet reaches the legacy parser, which silently
        // drops the unparseable track (upstream behavior) instead of failing
        // the sheet — the counterpart of the V4b crate-path strictness, which
        // is pinned by index_seconds_out_of_range_fail_without_legacy_fallback.
        let fx = CueFixture::new("legacy-invalid-index");
        fx.audio("album one.wav", 400_000);
        let cue = fx.write_cue(
            "legacy_invalid_index.cue",
            concat!(
                "TITLE \"Escape Test Album\"\n",
                "PERFORMER \"Various\"\n",
                "FILE \"album one.wav\" WAVE\n",
                "TRACK 01 AUDIO\n",
                "INDEX 01 00:00:00\n",
                "TRACK 02 AUDIO\n",
                "INDEX 01 01:99:00\n",
                "TRACK 03 AUDIO\n",
                "INDEX 01 03:30:74\n",
            ),
        );

        let tracks = CueHandler.inspect_tracks(&cue).unwrap();

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].track_number, "01");
        // the next same-FILE track has no usable start, so the audio duration
        // bounds track 1 (upstream end-computation semantics)
        assert_eq!(tracks[0].duration_ms, 400_000);
        assert_eq!(tracks[1].track_number, "03");
        assert_eq!(tracks[1].start_ms, 210_986);
        assert_eq!(tracks[1].duration_ms, 189_013);
    }

    #[test]
    fn missing_audio_fails_the_whole_sheet() {
        // Vector V8: a missing referenced audio fails the entire sheet —
        // there is no partial track splitting.
        let fx = CueFixture::new("missing-audio");
        let cue = fx.write_cue("missing_audio.cue", MISSING_AUDIO);

        let error = CueHandler.inspect_tracks(&cue).unwrap_err().to_string();

        assert!(error.contains("读取 CUE 音频失败"), "{error}");
    }

    #[test]
    fn tracks_without_index_01_are_skipped_or_rejected() {
        let fx = CueFixture::new("partial-index");
        fx.audio("album one.wav", 400_000);
        let partial = concat!(
            "TITLE \"Partial Index\"\n",
            "PERFORMER \"Various\"\n",
            "FILE \"album one.wav\" WAVE\n",
            "  TRACK 01 AUDIO\n",
            "    TITLE \"Has Index\"\n",
            "    INDEX 01 00:00:00\n",
            "  TRACK 02 AUDIO\n",
            "    TITLE \"No Index One\"\n",
            "    INDEX 00 00:01:00\n",
        );
        let none = concat!(
            "TITLE \"No Index\"\n",
            "PERFORMER \"Various\"\n",
            "FILE \"album one.wav\" WAVE\n",
            "  TRACK 01 AUDIO\n",
            "    TITLE \"Only Pregap\"\n",
            "    INDEX 00 00:01:00\n",
        );

        // A track without INDEX 01 is silently skipped (upstream behavior).
        let tracks = CueHandler
            .inspect_tracks(&fx.write_cue("partial.cue", partial))
            .unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_number, "01");
        assert_eq!(tracks[0].title.as_deref(), Some("Has Index"));

        // If no track carries INDEX 01 the whole sheet errors.
        let error = CueHandler
            .inspect_tracks(&fx.write_cue("none.cue", none))
            .unwrap_err()
            .to_string();
        assert!(error.contains("CUE 没有带 INDEX 01 的曲目"), "{error}");
    }

    #[test]
    fn inspect_and_extract_pass_through_the_referenced_audio() {
        let fx = CueFixture::new("inspect");
        fx.audio("album one.wav", 400_000);
        let cue = fx.write_cue("utf8_basic.cue", UTF8_SHEET);
        let handler = CueHandler;

        // `inspect` reports the audio behind the sheet's first FILE (the stub
        // carries no tags), not the sheet's track table.
        let metadata = handler.inspect(&cue).unwrap();
        assert_eq!(metadata.duration_ms, Some(400_000));
        assert_eq!(metadata.audio_format, AudioFormat::Wav);
        assert_eq!(metadata.title, None);

        // Upstream has no byte-level track splitting: extraction streams the
        // referenced audio verbatim and callers apply the per-track offsets.
        let mut bytes = Vec::new();
        let extracted = handler.extract_audio_to(&cue, &mut bytes).unwrap();
        assert_eq!(extracted.duration_ms, Some(400_000));
        let audio = fs::read(fx.root.join("album one.wav")).unwrap();
        assert_eq!(bytes, audio);
    }

    // Ports of the upstream scanner unit tests (voicefox scanner.rs:630-646).
    #[test]
    fn cue_time_uses_seventy_five_frames_per_second() {
        assert_eq!(
            parse_cue_time("01:02:37"),
            Some(Duration::from_secs(62) + Duration::from_nanos(493_333_333))
        );
        assert!(parse_cue_time("01:60:00").is_none());
        assert!(parse_cue_time("01:02:03:04").is_none());
        assert!(parse_cue_time("bad").is_none());
    }

    #[test]
    fn cue_quoted_escapes_are_decoded() {
        assert_eq!(
            decode_cue_text(r#"A \"quote\" and \\ path"#),
            r#"A "quote" and \ path"#
        );
    }
}
