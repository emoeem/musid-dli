use super::track::{Track, TrackId};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chart {
    pub id: TrackId,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChartEntry {
    pub rank: u32,
    pub track: Track,
}
