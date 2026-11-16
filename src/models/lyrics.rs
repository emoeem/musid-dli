use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LyricFormat {
    Lrc,
    Krc,
    Mrc,
    Qrc,
    Yrc,
    Ttml,
    Spotify,
    Musixmatch,
    PlainText,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricWord {
    pub text: String,
    pub start_ms: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    pub start_ms: u64,
    #[serde(default)]
    pub end_ms: Option<u64>,
    pub text: String,
    #[serde(default)]
    pub translations: BTreeMap<String, String>,
    #[serde(default)]
    pub romanization: Option<String>,
    #[serde(default)]
    pub words: Vec<LyricWord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricDocument {
    pub format: LyricFormat,
    pub original: String,
    #[serde(default)]
    pub translation: Option<String>,
    #[serde(default)]
    pub romanization: Option<String>,
    #[serde(default)]
    pub lines: Vec<LyricLine>,
}

/// Search candidate returned by an optional online lyrics backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricSearchResult {
    pub provider: String,
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub match_score: Option<u8>,
}
