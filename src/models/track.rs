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
/// Measured audio encoding parameters (MODEL_ALIGNMENT_DESIGN.md §3.3, the
/// G5 gap). Field names and wire keys match voicefox `AudioProperties`
/// (`core/src/model/source.rs:256-266`) key for key, so a host can move a
/// serialized `audio` object across without a key map. Producers: local
/// sources only (real files expose real specs; online providers declare
/// tiers, not measurements).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AudioSpec {
    /// Audio stream bitrate, kb/s.
    pub bitrate: Option<u32>,
    /// Sample rate, Hz.
    pub sample_rate: Option<u32>,
    /// Bit depth, bits.
    pub bit_depth: Option<u8>,
    /// Lossless encoding. Lossless bitrate does not describe a spec, so the
    /// label comes from depth and rate instead (see [`AudioSpec::label`]).
    #[serde(default)]
    pub lossless: bool,
}
impl AudioSpec {
    /// Measured spec label, port of voicefox `AudioProperties::label`
    /// (`source.rs:269-281`): lossless encodings label by depth and rate
    /// ("16/44.1"), lossy encodings by bitrate ("320K"). `None` when the
    /// present fields cannot produce a label.
    pub fn label(&self) -> Option<String> {
        if self.lossless {
            return match (self.bit_depth, self.sample_rate) {
                (Some(depth), Some(rate)) => Some(format!("{depth}/{}", format_khz(rate))),
                (None, Some(rate)) => Some(format!("{}kHz", format_khz(rate))),
                _ => None,
            };
        }
        self.bitrate.map(|bitrate| format!("{bitrate}K"))
    }
}

/// Sample rate to kHz display: whole thousands drop the fraction
/// (voicefox `format_khz`, `source.rs:285-291`).
fn format_khz(hz: u32) -> String {
    if hz.is_multiple_of(1000) {
        (hz / 1000).to_string()
    } else {
        format!("{:.1}", hz as f64 / 1000.0)
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
    /// Reserve slot for the cross-source canonical identity. No musid-dli
    /// provider fills it today (MODEL_ALIGNMENT_DESIGN.md §3.5); hosts may
    /// inject it, and the matcher honors it through its ISRC gate.
    #[serde(default)]
    pub isrc: Option<String>,
    #[serde(default)]
    pub available_qualities: Vec<StreamQuality>,
    /// Measured encoding parameters; `None` when unknown (all online
    /// providers today). Wire key matches voicefox `SongInfo.audio`.
    #[serde(default)]
    pub audio: Option<AudioSpec>,
    #[serde(default)]
    pub extra: std::collections::BTreeMap<String, String>,
}
impl Track {
    /// Canonical shape of `available_qualities` (§3.1, the
    /// `BTreeSet<Quality>` conversion contract): deduplicated, tier
    /// ascending, Unknown last (the tier ordering of `StreamQuality`).
    pub fn sorted_qualities(&self) -> Vec<StreamQuality> {
        let mut qualities = self.available_qualities.clone();
        qualities.sort();
        qualities.dedup();
        qualities
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_str, from_value, to_value};
    use std::collections::BTreeMap;

    fn track() -> Track {
        Track {
            id: TrackId::new("netease", "1"),
            title: "song".into(),
            artists: vec!["A".into(), "B".into()],
            album: Some("album".into()),
            album_id: Some("9".into()),
            duration: Some(std::time::Duration::from_secs(200)),
            artwork_url: Some("https://cover.example/1.jpg".into()),
            isrc: None,
            available_qualities: vec![StreamQuality::Lossless, StreamQuality::Low],
            audio: None,
            extra: BTreeMap::new(),
        }
    }

    /// T1 (§7): pre-M3 Track JSON without `audio` loads; the legacy shape of
    /// every other field is unchanged.
    #[test]
    fn legacy_json_without_new_fields_loads_with_documented_defaults() {
        let legacy = r#"{
            "id": {"provider": "netease", "id": "1"},
            "title": "song",
            "artists": ["A"],
            "album": "album",
            "duration": {"secs": 200, "nanos": 0},
            "available_qualities": ["Lossless"]
        }"#;
        let parsed: Track = from_str(legacy).expect("legacy json loads");
        assert_eq!(parsed.audio, None);
        assert_eq!(parsed.title, "song");
        assert_eq!(parsed.available_qualities, vec![StreamQuality::Lossless]);
    }

    /// T2 (§7): fully populated round trip including a complete `AudioSpec`.
    #[test]
    fn round_trip_preserves_every_field() {
        let mut track = track();
        track.audio = Some(AudioSpec {
            bitrate: Some(320),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            lossless: true,
        });
        track.isrc = Some("CNW000000000".into());
        let json = to_value(&track).expect("serialize");
        let rebuilt: Track = from_value(json).expect("deserialize");
        assert_eq!(rebuilt, track);
    }

    /// T6 (§7): normalization contract for the voicefox BTreeSet conversion.
    #[test]
    fn sorted_qualities_dedups_and_sorts_unknown_last() {
        let mut track = track();
        track.available_qualities = vec![
            StreamQuality::Unknown,
            StreamQuality::High,
            StreamQuality::Low,
            StreamQuality::High,
        ];
        assert_eq!(
            track.sorted_qualities(),
            vec![
                StreamQuality::Low,
                StreamQuality::High,
                StreamQuality::Unknown,
            ]
        );
    }

    /// T7 (§7): the label pipeline port of voicefox `source.rs:269-291`.
    #[test]
    fn audio_spec_label_matches_upstream_pipeline() {
        // Lossless with depth + rate: "depth/rate_kHz".
        assert_eq!(
            AudioSpec {
                bitrate: Some(1000),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                lossless: true,
            }
            .label()
            .as_deref(),
            Some("16/44.1")
        );
        // Whole thousands drop the fraction.
        assert_eq!(
            AudioSpec {
                bitrate: None,
                sample_rate: Some(48_000),
                bit_depth: Some(24),
                lossless: true,
            }
            .label()
            .as_deref(),
            Some("24/48")
        );
        // Fractional kHz keeps one decimal.
        assert_eq!(
            AudioSpec {
                bitrate: None,
                sample_rate: Some(44_100),
                bit_depth: None,
                lossless: true,
            }
            .label()
            .as_deref(),
            Some("44.1kHz")
        );
        // Lossless without rate has no label even with a bitrate.
        assert_eq!(
            AudioSpec {
                bitrate: Some(900),
                sample_rate: None,
                bit_depth: Some(16),
                lossless: true,
            }
            .label(),
            None
        );
        // Lossy: bitrate is the spec.
        assert_eq!(
            AudioSpec {
                bitrate: Some(320),
                sample_rate: Some(44_100),
                bit_depth: None,
                lossless: false,
            }
            .label()
            .as_deref(),
            Some("320K")
        );
        // Lossy without bitrate: nothing to say.
        assert_eq!(
            AudioSpec {
                bitrate: None,
                sample_rate: Some(44_100),
                bit_depth: None,
                lossless: false,
            }
            .label(),
            None
        );
    }
}
