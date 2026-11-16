use serde::{Deserialize, Serialize};
/// Tiers are ordered Low < Standard < High < Lossless < HiRes; Unknown sorts
/// last so fallback selection never treats it as a real tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum StreamQuality {
    Low,
    Standard,
    High,
    Lossless,
    HiRes,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AudioFormat {
    Mp3,
    Flac,
    Aac,
    M4a,
    Opus,
    Ogg,
    Wav,
    #[default]
    Unknown,
}
/// Half-open byte range [start, end) required by segmented/DASH resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

/// Resolved URL and transport metadata for a host application's player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub url: String,
    pub quality: StreamQuality,
    #[serde(default)]
    pub format: AudioFormat,
    #[serde(default)]
    pub duration: Option<std::time::Duration>,
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
    #[serde(default)]
    pub content_length: Option<u64>,
    #[serde(default)]
    pub checksum: Option<String>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub fallback_urls: Vec<String>,
    #[serde(default)]
    pub index_range: Option<ByteRange>,
    #[serde(default)]
    pub init_range: Option<ByteRange>,
    #[serde(default)]
    pub max_chunk_size: Option<u64>,
}
