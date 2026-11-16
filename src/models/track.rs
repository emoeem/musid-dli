use super::stream::StreamQuality;
use serde::{Deserialize, Serialize};

/// Provider-scoped ID: platform IDs are not globally unique.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackId {
    pub provider: String,
    pub id: String,
}
impl TrackId {
    pub fn new(provider: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            id: id.into(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub title: String,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub duration: Option<std::time::Duration>,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub isrc: Option<String>,
    #[serde(default)]
    pub available_qualities: Vec<StreamQuality>,
    #[serde(default)]
    pub extra: std::collections::BTreeMap<String, String>,
}
