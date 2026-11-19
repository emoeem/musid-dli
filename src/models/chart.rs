use super::track::{Track, TrackId};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chart {
    pub id: TrackId,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Update cadence text as the platform states it ("每日更新",
    /// voicefox `LeaderboardInfo.update`, leaderboard.rs:9). Separate field,
    /// not a reuse of `description`: description stays free text, the update
    /// cadence stays structured (MODEL_ALIGNMENT_DESIGN.md §5.1).
    #[serde(default)]
    pub updated_description: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChartEntry {
    pub rank: u32,
    pub track: Track,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_str, from_value, to_value};

    /// T1 (§7): pre-M3 Chart JSON without `updated_description` loads.
    #[test]
    fn legacy_json_without_new_fields_loads_with_documented_defaults() {
        let legacy = r#"{"id": {"provider": "qqmusic", "id": "4"}, "title": "巅峰榜"}"#;
        let parsed: Chart = from_str(legacy).expect("legacy json loads");
        assert_eq!(parsed.updated_description, None);
        assert_eq!(parsed.description, None);
    }

    /// T2 (§7): fully populated Chart round trip.
    #[test]
    fn chart_round_trip_preserves_every_field() {
        let chart = Chart {
            id: TrackId::new("qqmusic", "4"),
            title: "巅峰榜".into(),
            description: Some("QQ 音乐巅峰榜".into()),
            updated_description: Some("每日更新".into()),
        };
        let json = to_value(&chart).expect("serialize");
        let rebuilt: Chart = from_value(json).expect("deserialize");
        assert_eq!(rebuilt, chart);
    }
}
