//! Cross-provider track matching for source fallback.
//!
//! M6 explainable scoring (RESOURCE_DESIGN.md §3): every match carries a
//! per-part breakdown, version-token penalties (live/remix/cover/
//! instrumental/伴奏 and friends, asymmetric only) and an explicit
//! confidence grade, so a low-trust alternative can never masquerade as a
//! confident one. The legacy `match_score`/`rank_alternatives` signatures
//! are unchanged; scores are preserved unless a newly detected version or
//! multi-artist asymmetry fires a penalty.
use crate::models::Track;
use serde::{Deserialize, Serialize};

/// A ranked alternative with its full explanation (RESOURCE_DESIGN.md §3.4).
/// `confidence` is derived from the score and the penalty flags, never
/// caller-supplied: a low-trust match always says so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchCandidate {
    pub track: Track,
    pub score: u8,
    #[serde(default)]
    pub breakdown: ScoreBreakdown,
    #[serde(default)]
    pub confidence: MatchConfidence,
    #[serde(default)]
    pub flags: MatchFlags,
}

/// The five legacy score parts plus the M6 penalties (RESOURCE_DESIGN.md
/// §3.2). `total()` reproduces the legacy score exactly when both penalties
/// are zero and the ISRC gate is absent, which is the compatibility pin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub title: u8,
    pub artist: u8,
    pub duration: u8,
    pub album: u8,
    /// Asymmetric version-token penalty, capped at [`VERSION_PENALTY_CAP`].
    pub version_penalty: u8,
    /// Multi-artist asymmetry penalty ([`MULTI_ARTIST_PENALTY`]).
    pub artist_penalty: u8,
    /// ISRC gate outcome: `Some(true)` = identical ISRC (hard 100),
    /// `Some(false)` = conflicting ISRC (hard 0), `None` = no gate.
    pub isrc: Option<bool>,
    /// Detection flags behind the two penalties (explanability surface).
    pub flags: MatchFlags,
}
impl ScoreBreakdown {
    /// The composite score: the ISRC gate short-circuits, otherwise the
    /// legacy sum minus both penalties, capped at 100.
    pub fn total(&self) -> u8 {
        match self.isrc {
            Some(true) => return 100,
            Some(false) => return 0,
            None => {}
        }
        self.title
            .saturating_add(self.artist)
            .saturating_add(self.duration)
            .saturating_add(self.album)
            .saturating_sub(self.version_penalty)
            .saturating_sub(self.artist_penalty)
            .min(100)
    }
}

/// Explicit confidence grade of a match (RESOURCE_DESIGN.md §3.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MatchConfidence {
    #[default]
    Low,
    Medium,
    High,
}

/// What the scorer detected about a pair (RESOURCE_DESIGN.md §3.3). Pure
/// detection: the penalties are already folded into the breakdown, the flags
/// exist so hosts can explain a rejection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchFlags {
    /// One side carries version tokens the other lacks.
    pub version_token_mismatch: bool,
    /// Both sides are multi-artist and neither artist set is a subset of
    /// the other (collaboration recorded differently).
    pub multi_artist_mismatch: bool,
    /// At least one side has no duration; the duration part scored 0 for
    /// lack of data, not because the durations disagree.
    pub duration_unknown: bool,
}

/// Confidence thresholds (RESOURCE_DESIGN.md §3.4). A penalized pair can
/// never be `High` even when its raw total clears the threshold.
pub const HIGH_CONFIDENCE_SCORE: u8 = 85;
pub const MEDIUM_CONFIDENCE_SCORE: u8 = 60;
/// Penalty per asymmetric version-token class, capped at
/// [`VERSION_PENALTY_CAP`].
pub const VERSION_PENALTY_PER_TOKEN: u8 = 15;
pub const VERSION_PENALTY_CAP: u8 = 30;
/// Penalty for a multi-artist asymmetry (both sides 2+ artists, neither a
/// subset of the other).
pub const MULTI_ARTIST_PENALTY: u8 = 10;

/// ASCII version tokens, matched against whole separator-delimited segments
/// of the lowercased title ("song title (live)" splits to a `live` segment).
const ASCII_VERSION_TOKENS: &[&str] = &[
    "live",
    "remix",
    "cover",
    "instrumental",
    "karaoke",
    "acoustic",
    "demo",
];
/// CJK version tokens, matched by substring (CJK carries no separators).
const CJK_VERSION_TOKENS: &[&str] = &["伴奏", "纯音乐", "翻唱", "现场", "改编"];

/// Version tokens present in a title (RESOURCE_DESIGN.md §3.3). Detection is
/// symmetric; only asymmetry between a pair is penalized.
pub fn version_tokens(title: &str) -> Vec<&'static str> {
    let lowered = title.to_lowercase();
    let segments: Vec<&str> = lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    let mut found = Vec::new();
    for token in ASCII_VERSION_TOKENS {
        if segments.iter().any(|segment| segment == token) {
            found.push(*token);
        }
    }
    for token in CJK_VERSION_TOKENS {
        if lowered.contains(token) {
            found.push(*token);
        }
    }
    found
}

/// Penalty for asymmetric version tokens between the two titles: 15 per
/// class present on exactly one side, capped at 30. Symmetric tokens
/// (both sides carry the same class) penalize nothing.
fn version_penalty_for(left: &str, right: &str) -> u8 {
    let left_tokens = version_tokens(left);
    let right_tokens = version_tokens(right);
    let mut penalty = 0u32;
    for token in left_tokens.iter().chain(right_tokens.iter()) {
        let only_left = left_tokens.contains(token) && !right_tokens.contains(token);
        let only_right = !left_tokens.contains(token) && right_tokens.contains(token);
        if only_left || only_right {
            penalty += u32::from(VERSION_PENALTY_PER_TOKEN);
        }
    }
    penalty.min(u32::from(VERSION_PENALTY_CAP)) as u8
}

/// Multi-artist asymmetry penalty (RESOURCE_DESIGN.md §3.3): both sides
/// carry more than one artist and neither set is a subset of the other.
/// Single-artist pairs are governed by the unchanged Jaccard rule.
fn artist_mismatch_penalty(left: &[String], right: &[String]) -> u8 {
    if left.len() <= 1 || right.len() <= 1 {
        return 0;
    }
    let normalize_set = |artists: &[String]| -> std::collections::HashSet<String> {
        artists
            .iter()
            .map(|a| normalize(a))
            .filter(|s| !s.is_empty())
            .collect()
    };
    let left_set = normalize_set(left);
    let right_set = normalize_set(right);
    if left_set.is_empty() || right_set.is_empty() {
        return 0;
    }
    let left_subset = left_set.iter().all(|a| right_set.contains(a));
    let right_subset = right_set.iter().all(|a| left_set.contains(a));
    if left_subset || right_subset {
        0
    } else {
        MULTI_ARTIST_PENALTY
    }
}

/// Rank likely versions of the same recording. ISRC conflicts are a hard
/// rejection; metadata-only matches use title, artist, duration and album,
/// penalized by version-token and multi-artist asymmetry, each annotated
/// with breakdown, flags and confidence.
///
/// Ordering: score descending, confidence descending as the tie-break,
/// then provider/id. Two properties hold (RESOURCE_DESIGN.md §3.4):
///
/// 1. a penalized pair is **never `High`** (`confidence_of` requires zero
///    penalties at or above the 85 floor);
/// 2. at an equal score the confidence tie-break puts the clean pair
///    ahead, which matters because normalized-equal titles can carry
///    divergent raw tokenization (`Song (Live)` and `SongLive` normalize
///    to the same string, so both score 40 on title, and the asymmetric
///    `live` token penalizes exactly one side down to 85 Medium while a
///    clean pair reaches 85 High).
///
/// There is deliberately NO claim that a penalized score stays below the
/// `High` floor in general: a normalized-equal-title pair still reaches
/// 85, and near-superset multi-artist sets (large casts sharing most
/// members) score 86+ after the penalty. A penalized `Medium` can
/// therefore legitimately outrank a clean `High` under score-primary
/// ordering. Take-first callers MUST inspect [`MatchCandidate::confidence`]
/// and [`MatchCandidate::flags`] before presenting the top candidate as
/// the recording.
pub fn rank_alternatives(
    target: &Track,
    candidates: &[Track],
    minimum_score: u8,
) -> Vec<MatchCandidate> {
    let mut matches = candidates
        .iter()
        .filter(|candidate| candidate.id != target.id)
        .filter_map(|candidate| {
            let breakdown = match_breakdown(target, candidate);
            let score = breakdown.total();
            (score >= minimum_score).then(|| {
                let confidence = confidence_of(score, &breakdown);
                MatchCandidate {
                    track: candidate.clone(),
                    score,
                    breakdown,
                    confidence,
                    flags: breakdown.flags,
                }
            })
        })
        .collect::<Vec<_>>();
    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.confidence.cmp(&a.confidence))
            .then_with(|| a.track.id.provider.cmp(&b.track.id.provider))
            .then_with(|| a.track.id.id.cmp(&b.track.id.id))
    });
    matches
}

/// The legacy entry point, kept byte-for-byte: returns
/// [`match_breakdown`]`(...).total()` (RESOURCE_DESIGN.md §3.2).
pub fn match_score(left: &Track, right: &Track) -> u8 {
    match_breakdown(left, right).total()
}

/// Explainable scoring: the five legacy parts, the ISRC gate outcome, the
/// version/multi-artist penalties and the detection flags.
pub fn match_breakdown(left: &Track, right: &Track) -> ScoreBreakdown {
    if let (Some(a), Some(b)) = (&left.isrc, &right.isrc)
        && !a.is_empty()
        && !b.is_empty()
    {
        return ScoreBreakdown {
            isrc: Some(a.eq_ignore_ascii_case(b)),
            flags: MatchFlags {
                duration_unknown: left.duration.is_none() || right.duration.is_none(),
                ..MatchFlags::default()
            },
            ..ScoreBreakdown::default()
        };
    }
    let title_a = normalize(&left.title);
    let title_b = normalize(&right.title);
    let title_score = if title_a.is_empty() || title_b.is_empty() {
        0
    } else if title_a == title_b {
        40
    } else if title_a.contains(&title_b) || title_b.contains(&title_a) {
        30
    } else {
        (bigram_similarity(&title_a, &title_b) * 30.0).round() as u8
    };
    let artist_score = artist_similarity(&left.artists, &right.artists);
    let duration_score = match (left.duration, right.duration) {
        (Some(a), Some(b)) => match a.abs_diff(b).as_secs() {
            0..=2 => 25,
            3..=5 => 18,
            6..=10 => 10,
            11..=20 => 4,
            _ => 0,
        },
        _ => 0,
    };
    let album_score = match (&left.album, &right.album) {
        (Some(a), Some(b)) if normalize(a) == normalize(b) && !a.is_empty() => 5,
        _ => 0,
    };
    let version_penalty = version_penalty_for(&left.title, &right.title);
    let artist_penalty = artist_mismatch_penalty(&left.artists, &right.artists);
    let flags = MatchFlags {
        version_token_mismatch: version_penalty > 0,
        multi_artist_mismatch: artist_penalty > 0,
        duration_unknown: left.duration.is_none() || right.duration.is_none(),
    };
    ScoreBreakdown {
        title: title_score,
        artist: artist_score,
        duration: duration_score,
        album: album_score,
        version_penalty,
        artist_penalty,
        isrc: None,
        flags,
    }
}

/// Confidence grade of a total score plus its breakdown (RESOURCE_DESIGN.md
/// §3.4): `High` requires the score threshold AND zero penalties, so a
/// live-version match can never present as confident; `Medium` from 60;
/// `Low` below.
pub fn confidence_of(score: u8, breakdown: &ScoreBreakdown) -> MatchConfidence {
    let penalized = breakdown.version_penalty > 0 || breakdown.artist_penalty > 0;
    if score >= HIGH_CONFIDENCE_SCORE && !penalized {
        MatchConfidence::High
    } else if score >= MEDIUM_CONFIDENCE_SCORE {
        MatchConfidence::Medium
    } else {
        MatchConfidence::Low
    }
}

fn artist_similarity(left: &[String], right: &[String]) -> u8 {
    if left.is_empty() || right.is_empty() {
        return 0;
    }
    let a = left
        .iter()
        .map(|s| normalize(s))
        .filter(|s| !s.is_empty())
        .collect::<std::collections::HashSet<_>>();
    let b = right
        .iter()
        .map(|s| normalize(s))
        .filter(|s| !s.is_empty())
        .collect::<std::collections::HashSet<_>>();
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let overlap = a.intersection(&b).count() as f32;
    let union = a.union(&b).count() as f32;
    (30.0 * overlap / union).round() as u8
}
fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}
fn bigram_similarity(a: &str, b: &str) -> f32 {
    fn grams(value: &str) -> std::collections::HashSet<String> {
        let chars = value.chars().collect::<Vec<_>>();
        if chars.len() < 2 {
            return [value.to_owned()].into_iter().collect();
        }
        chars.windows(2).map(|pair| pair.iter().collect()).collect()
    }
    let a = grams(a);
    let b = grams(b);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.intersection(&b).count() as f32 / a.union(&b).count() as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn track(provider: &str, title: &str) -> Track {
        Track {
            id: crate::TrackId::new(provider, title),
            title: title.into(),
            artists: vec!["Example Artist".into()],
            album: Some("Example Album".into()),
            album_id: None,
            duration: Some(Duration::from_secs(180)),
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: Default::default(),
        }
    }
    #[test]
    fn exact_recording_matches_across_providers() {
        let target = track("a", "Song Title");
        let candidate = track("b", "Song Title");
        assert!(match_score(&target, &candidate) >= 95);
        assert_eq!(rank_alternatives(&target, &[candidate], 90).len(), 1);
    }
    #[test]
    fn conflicting_isrc_is_rejected() {
        let mut target = track("a", "Song Title");
        let mut candidate = track("b", "Song Title");
        target.isrc = Some("USAAA0000001".into());
        candidate.isrc = Some("USBBB0000002".into());
        assert_eq!(match_score(&target, &candidate), 0);
    }

    // ---- M6: explainable scoring (RESOURCE_DESIGN.md §3.2/§3.4) ----

    #[test]
    fn existing_exact_and_isrc_scores_are_unchanged_and_annotated() {
        // Compatibility pin: the legacy totals are reproduced exactly when
        // no penalty flag fires, and the annotations agree with them.
        let target = track("a", "Song Title");
        let candidate = track("b", "Song Title");
        let breakdown = match_breakdown(&target, &candidate);
        assert_eq!(breakdown.total(), match_score(&target, &candidate));
        assert_eq!(breakdown.title, 40);
        assert_eq!(breakdown.artist, 30);
        assert_eq!(breakdown.duration, 25);
        assert_eq!(breakdown.album, 5);
        assert_eq!(breakdown.version_penalty, 0);
        assert_eq!(breakdown.artist_penalty, 0);
        assert_eq!(breakdown.isrc, None);
        assert_eq!(breakdown.flags, MatchFlags::default());
        let ranked = rank_alternatives(&target, &[candidate], 90);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].confidence, MatchConfidence::High);
        assert_eq!(ranked[0].flags, MatchFlags::default());
        // ISRC gate shapes the breakdown directly.
        let mut left = track("a", "Song Title");
        let mut right = track("b", "Different");
        left.isrc = Some("USAAA0000001".into());
        right.isrc = Some("usaaa0000001".into());
        let breakdown = match_breakdown(&left, &right);
        assert_eq!(breakdown.isrc, Some(true));
        assert_eq!(breakdown.total(), 100);
        assert_eq!(confidence_of(100, &breakdown), MatchConfidence::High);
        left.isrc = Some("USAAA0000001".into());
        right.isrc = Some("USBBB0000002".into());
        let breakdown = match_breakdown(&left, &right);
        assert_eq!(breakdown.isrc, Some(false));
        assert_eq!(breakdown.total(), 0);
        assert_eq!(confidence_of(0, &breakdown), MatchConfidence::Low);
    }

    #[test]
    fn legacy_match_candidate_payloads_deserialize_with_defaults() {
        // Compatibility pin (RESOURCE_DESIGN.md §3.2): breakdown/confidence/
        // flags are `#[serde(default)]`, so a pre-M6 payload carrying only
        // the track and the total score still deserializes.
        let target = track("a", "Song Title");
        let fresh = rank_alternatives(&target, &[track("b", "Song Title")], 90);
        let mut legacy = serde_json::to_value(&fresh[0]).expect("serializes");
        let object = legacy.as_object_mut().expect("an object");
        for absent in ["breakdown", "confidence", "flags"] {
            assert!(object.remove(absent).is_some(), "{absent} was present");
        }
        let parsed: MatchCandidate = serde_json::from_value(legacy).expect("legacy payload parses");
        assert_eq!(parsed.score, fresh[0].score);
        assert_eq!(parsed.track.id, fresh[0].track.id);
        assert_eq!(parsed.breakdown, ScoreBreakdown::default());
        assert_eq!(parsed.confidence, MatchConfidence::default());
        assert_eq!(parsed.flags, MatchFlags::default());
        // A full payload round-trips with every annotation intact.
        let round: MatchCandidate =
            serde_json::from_value(serde_json::to_value(&fresh[0]).expect("serializes"))
                .expect("full payload parses");
        assert_eq!(round, fresh[0]);
    }

    #[test]
    fn penalized_pairs_never_reach_high_confidence() {
        // A live-version pair whose raw total clears 85 must still present
        // as Medium: explicit low-confidence carry (RESOURCE_DESIGN.md §3.4).
        let target = track("a", "Song Title");
        let mut candidate = track("b", "Song Title (Live)");
        candidate.artists = vec!["Example Artist".into()];
        candidate.album = Some("Example Album".into());
        candidate.duration = Some(Duration::from_secs(180));
        let breakdown = match_breakdown(&target, &candidate);
        assert!(breakdown.version_penalty >= VERSION_PENALTY_PER_TOKEN);
        assert!(breakdown.flags.version_token_mismatch);
        let score = breakdown.total();
        assert!(score < 90, "penalty must bite: {score}");
        assert_eq!(confidence_of(score, &breakdown), MatchConfidence::Medium);
        // Threshold constants are the documented ones.
        assert_eq!(HIGH_CONFIDENCE_SCORE, 85);
        assert_eq!(MEDIUM_CONFIDENCE_SCORE, 60);
    }

    #[test]
    fn symmetric_version_tokens_penalize_nothing() {
        let target = track("a", "Song Title (Live)");
        let candidate = track("b", "Song Title (Live)");
        let breakdown = match_breakdown(&target, &candidate);
        assert_eq!(breakdown.version_penalty, 0);
        assert!(!breakdown.flags.version_token_mismatch);
        // title 40 + artist 30 + duration 25 + album 5: the exact-match
        // total, untouched by symmetric tokens.
        assert_eq!(breakdown.total(), 100);
        assert_eq!(confidence_of(100, &breakdown), MatchConfidence::High);
    }

    #[test]
    fn version_token_penalty_is_capped() {
        // Left has two asymmetric classes, right has one more: 3 classes x
        // 15 = 45, capped at 30.
        let breakdown_score = version_penalty_for("Song (Live) (Remix)", "Song (Cover)");
        assert_eq!(breakdown_score, VERSION_PENALTY_CAP);
    }

    /// Review P2 (constructed examples, NOT a global bound): the
    /// small-cast worst cases for the two single-penalty shapes stay
    /// Medium, never High. These are illustrative pairs, not a proof that
    /// penalized scores are bounded below the High floor in general: a
    /// normalized-equal-title pair still reaches 85, and near-superset
    /// multi-artist sets (large casts sharing most members) score 86+.
    /// The true global properties are the `confidence_of` penalty gate
    /// (a penalized pair is never High) and the confidence tie-break, see
    /// `clean_high_wins_the_score_tie_against_a_penalized_variant`.
    /// Version-asymmetric contains-title: 30 + 30 + 25 + 5 - 15 = 75;
    /// multi-artist asymmetry with a 3-common-of-5-union Jaccard (18):
    /// 40 + 18 + 25 + 5 - 10 = 78.
    #[test]
    fn penalized_examples_stay_medium() {
        // Version-token example.
        let target = track("a", "Song Title");
        let mut candidate = track("b", "Song Title (Live)");
        candidate.duration = Some(Duration::from_secs(180));
        let breakdown = match_breakdown(&target, &candidate);
        assert_eq!(breakdown.total(), 75);
        assert_eq!(confidence_of(75, &breakdown), MatchConfidence::Medium);
        // Multi-artist example.
        let mut target_multi = track("a", "Song Title");
        target_multi.artists = vec!["A".into(), "B".into(), "C".into(), "X".into()];
        let mut candidate_multi = track("b", "Song Title");
        candidate_multi.artists = vec!["A".into(), "B".into(), "C".into(), "Y".into()];
        candidate_multi.album = Some("Example Album".into());
        candidate_multi.duration = Some(Duration::from_secs(180));
        let breakdown = match_breakdown(&target_multi, &candidate_multi);
        assert!(breakdown.flags.multi_artist_mismatch);
        assert_eq!(breakdown.artist, 18);
        assert_eq!(breakdown.total(), 78);
        assert_eq!(confidence_of(78, &breakdown), MatchConfidence::Medium);
        // The one global property that DOES hold: penalized pairs can never
        // be High (confidence_of gates on zero penalties), whatever their
        // score. Demonstrate at a score above the High floor.
        let mut near_superset = track("b", "Song Title");
        near_superset.album = Some("Example Album".into());
        near_superset.duration = Some(Duration::from_secs(180));
        let mut big_cast_target = track("a", "Song Title");
        let shared: Vec<String> = (0..12).map(|i| format!("artist{i:02}")).collect();
        big_cast_target.artists = {
            let mut a = shared.clone();
            a.push("onlyA".into());
            a
        };
        near_superset.artists = {
            let mut a = shared;
            a.push("onlyB".into());
            a
        };
        let breakdown = match_breakdown(&big_cast_target, &near_superset);
        assert!(breakdown.flags.multi_artist_mismatch);
        assert!(
            breakdown.total() >= HIGH_CONFIDENCE_SCORE,
            "near-superset casts clear the floor: {}",
            breakdown.total()
        );
        assert_eq!(
            confidence_of(breakdown.total(), &breakdown),
            MatchConfidence::Medium,
            "the penalty gate holds above the floor"
        );
    }

    /// Review P2: at an equal score, the confidence tie-break must put the
    /// clean High ahead of the penalized Medium variant, so a take-first
    /// caller never receives the variant match from a tie. The tie is real:
    /// `Song (Live)` and `Song (live)` normalize equal (title 40), the
    /// clean pair loses 15 duration points for a 7 s drift, and the
    /// penalized pair keeps full duration but pays the asymmetric token
    /// penalty.
    #[test]
    fn clean_high_wins_the_score_tie_against_a_penalized_variant() {
        let target = track("a", "Song (Live)");
        // Penalized variant: title 40 + artist 30 + duration 25 + album 5
        // = 100, minus the asymmetric `live` token (present on the target,
        // absent as a segment in `SongLive`) = 85 -> Medium.
        let mut variant = track("b", "SongLive");
        variant.duration = Some(Duration::from_secs(180));
        // Clean pair: same title spelling family (tokens symmetric), 7 s
        // duration drift: 40 + 30 + 10 + 5 = 85 -> High.
        let mut clean = track("c", "Song (live)");
        clean.duration = Some(Duration::from_secs(187));
        let ranked = rank_alternatives(&target, &[variant, clean], 85);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].score, 85);
        assert_eq!(ranked[1].score, 85);
        assert_eq!(ranked[0].confidence, MatchConfidence::High);
        assert_eq!(ranked[1].confidence, MatchConfidence::Medium);
        assert_eq!(ranked[0].track.id.provider, "c");
        assert_eq!(ranked[1].track.id.provider, "b");
    }

    // ---- M6: version-difference fixture matrix (RESOURCE_DESIGN.md §3.5) ----

    /// Simulated (non-live-capture) version matrix, tasks.md §15.4
    /// separation documented in tests/fixtures/matcher/README.md.
    #[derive(Deserialize)]
    struct FixtureCase {
        name: String,
        target: FixtureTrack,
        candidate: FixtureTrack,
        expect_score_min: u8,
        expect_score_max: u8,
        expect_confidence: String,
        expect_flags: Vec<String>,
    }
    #[derive(Deserialize)]
    struct FixtureTrack {
        provider: String,
        id: String,
        title: String,
        artists: Vec<String>,
        album: Option<String>,
        duration_secs: Option<u64>,
        isrc: Option<String>,
    }
    impl FixtureTrack {
        fn to_track(&self) -> Track {
            Track {
                id: crate::TrackId::new(&self.provider, &self.id),
                title: self.title.clone(),
                artists: self.artists.clone(),
                album: self.album.clone(),
                album_id: None,
                duration: self.duration_secs.map(Duration::from_secs),
                artwork_url: None,
                isrc: self.isrc.clone(),
                available_qualities: vec![],
                audio: None,
                extra: Default::default(),
            }
        }
    }
    fn parse_flags(names: &[String]) -> MatchFlags {
        for name in names {
            assert!(
                matches!(
                    name.as_str(),
                    "version_token_mismatch" | "multi_artist_mismatch" | "duration_unknown"
                ),
                "unknown flag name {name}"
            );
        }
        MatchFlags {
            version_token_mismatch: names.iter().any(|n| n == "version_token_mismatch"),
            multi_artist_mismatch: names.iter().any(|n| n == "multi_artist_mismatch"),
            duration_unknown: names.iter().any(|n| n == "duration_unknown"),
        }
    }
    fn parse_confidence(name: &str) -> MatchConfidence {
        match name {
            "Low" => MatchConfidence::Low,
            "Medium" => MatchConfidence::Medium,
            "High" => MatchConfidence::High,
            other => panic!("unknown confidence {other}"),
        }
    }
    #[test]
    fn version_matrix_fixture_is_pinned() {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/matcher/version_matrix.json"
        ));
        let fixture: std::collections::BTreeMap<String, Vec<FixtureCase>> =
            serde_json::from_str(raw).expect("fixture parses");
        let cases = fixture.get("cases").expect("cases key");
        assert!(cases.len() >= 10, "the matrix must not shrink");
        for case in cases {
            let left = case.target.to_track();
            let right = case.candidate.to_track();
            let breakdown = match_breakdown(&left, &right);
            let score = breakdown.total();
            assert!(
                score >= case.expect_score_min && score <= case.expect_score_max,
                "case {}: score {score} outside [{}, {}]",
                case.name,
                case.expect_score_min,
                case.expect_score_max
            );
            assert_eq!(
                confidence_of(score, &breakdown),
                parse_confidence(&case.expect_confidence),
                "case {}: confidence",
                case.name
            );
            assert_eq!(
                breakdown.flags,
                parse_flags(&case.expect_flags),
                "case {}: flags",
                case.name
            );
            // The legacy entry point agrees with the breakdown everywhere.
            assert_eq!(match_score(&left, &right), score, "case {}", case.name);
        }
    }
}
