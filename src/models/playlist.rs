use super::track::{Track, TrackId};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: TrackId,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub track_count: Option<u32>,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistCategory {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub hot: bool,
}
