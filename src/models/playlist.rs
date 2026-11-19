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
    /// Play count as declared by the platform (MODEL_ALIGNMENT_DESIGN.md
    /// §5.1, voicefox `playlist.rs:16`).
    #[serde(default)]
    pub play_count: Option<u64>,
    /// Playlist creator; only some platforms return one.
    #[serde(default)]
    pub creator: Option<String>,
    /// Platform web link, for link resolution and "open in browser".
    #[serde(default)]
    pub link: Option<String>,
    /// Platform-specific routing/detail data (e.g. detail-page type marks).
    /// Generic slot on purpose (R6): private shapes stay in the providers.
    #[serde(default)]
    pub extra: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistCategory {
    pub id: String,
    pub name: String,
    /// Owning provider namespace (voicefox carries `source: SourceId`,
    /// `playlist.rs:53`); `None` = not declared. Strings map through
    /// `models::namespace` (§3.2).
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub hot: bool,
    #[serde(default)]
    pub extra: std::collections::BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_str, from_value, to_value};

    fn playlist() -> Playlist {
        Playlist {
            id: TrackId::new("netease", "1"),
            title: "playlist".into(),
            description: Some("d".into()),
            artwork_url: Some("https://cover.example/1.jpg".into()),
            track_count: Some(3),
            tracks: vec![],
            play_count: Some(7),
            creator: Some("creator".into()),
            link: Some("https://music.163.com/playlist?id=1".into()),
            extra: [("hot".to_string(), "1".to_string())].into_iter().collect(),
        }
    }

    /// T1 (§7): pre-M3 Playlist JSON (no collection detail fields) loads.
    #[test]
    fn legacy_json_without_new_fields_loads_with_documented_defaults() {
        let legacy = r#"{
            "id": {"provider": "kuwo", "id": "2"},
            "title": "legacy",
            "track_count": 12
        }"#;
        let parsed: Playlist = from_str(legacy).expect("legacy json loads");
        assert_eq!(parsed.play_count, None);
        assert_eq!(parsed.creator, None);
        assert_eq!(parsed.link, None);
        assert!(parsed.extra.is_empty());
        assert_eq!(parsed.track_count, Some(12));
    }

    /// T2 (§7): fully populated Playlist round trip.
    #[test]
    fn playlist_round_trip_preserves_every_field() {
        let playlist = playlist();
        let json = to_value(&playlist).expect("serialize");
        let rebuilt: Playlist = from_value(json).expect("deserialize");
        assert_eq!(rebuilt, playlist);
    }

    /// T1/T2 (§7) for PlaylistCategory, including the new provider slot.
    #[test]
    fn category_legacy_loads_and_round_trips() {
        let legacy = r#"{"id": "10000", "name": "热歌", "group": "hot", "count": 30, "hot": true}"#;
        let parsed: PlaylistCategory = from_str(legacy).expect("legacy json loads");
        assert_eq!(parsed.provider, None);
        assert!(parsed.extra.is_empty());
        assert!(parsed.hot);

        let category = PlaylistCategory {
            id: "10000".into(),
            name: "热歌".into(),
            provider: Some("netease".into()),
            group: Some("hot".into()),
            count: 30,
            hot: true,
            extra: [("route".to_string(), "detail".to_string())]
                .into_iter()
                .collect(),
        };
        let json = to_value(&category).expect("serialize");
        let rebuilt: PlaylistCategory = from_value(json).expect("deserialize");
        assert_eq!(rebuilt, category);
    }
}
