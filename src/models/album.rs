use super::track::TrackId;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Album {
    pub id: TrackId,
    pub title: String,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub track_count: Option<u32>,
}
