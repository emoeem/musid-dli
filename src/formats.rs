//! Local container/format handlers (e.g. NCM) are not online music providers.
use crate::{Result, models::AudioFormat};
use std::{io::Write, path::Path, path::PathBuf};

#[derive(Debug, Clone, Default)]
pub struct ContainerMetadata {
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub audio_format: AudioFormat,
    pub artwork: Option<Vec<u8>>,
}

/// One logical track inside a multi-track container such as a CUE sheet.
///
/// Upstream CUE splitting is metadata-only: a track is a start offset into its
/// referenced audio file, so players seek to `start_ms` and the track length
/// is informational (no playback is cut at track boundaries).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerTrack {
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    /// Offset the player must seek to inside `audio_path` (CUE INDEX 01).
    pub start_ms: u64,
    /// Track length; derived from the next INDEX 01 or the audio duration.
    pub duration_ms: u64,
    /// Audio file that backs this track (the source mapping).
    pub audio_path: PathBuf,
    /// CUE track identifier: positional two-digit numbering for
    /// standards-compliant sheets, the verbatim TRACK token for the legacy
    /// fallback parser (matches upstream identity semantics).
    pub track_number: String,
}

/// Implementations inspect/decrypt local containers and stream extracted audio
/// to a caller-owned writer; they do not download or play the resulting audio.
pub trait ContainerFormatHandler: Send + Sync {
    fn id(&self) -> &str;
    fn extensions(&self) -> &[&str];
    fn inspect(&self, path: &Path) -> Result<ContainerMetadata>;
    fn extract_audio_to(&self, path: &Path, output: &mut dyn Write) -> Result<ContainerMetadata>;

    /// Logical tracks of a multi-track container (CUE sheets), one entry per
    /// track with its start offset and referenced audio file. Containers
    /// backed by a single audio stream report no entries; their metadata is
    /// described by `inspect`.
    fn inspect_tracks(&self, _path: &Path) -> Result<Vec<ContainerTrack>> {
        Ok(Vec::new())
    }
}
