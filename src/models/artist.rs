use super::track::TrackId;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artist {
    pub id: TrackId,
    pub name: String,
    #[serde(default)]
    pub artwork_url: Option<String>,
}
