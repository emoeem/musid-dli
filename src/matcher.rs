//! Cross-provider track matching for source fallback.
use crate::models::Track;

#[derive(Debug, Clone)]
pub struct MatchCandidate {
    pub track: Track,
    pub score: u8,
}

/// Rank likely versions of the same recording. ISRC conflicts are a hard
/// rejection; metadata-only matches use title, artist, duration and album.
pub fn rank_alternatives(
    target: &Track,
    candidates: &[Track],
    minimum_score: u8,
) -> Vec<MatchCandidate> {
    let mut matches = candidates
        .iter()
        .filter(|candidate| candidate.id != target.id)
        .filter_map(|candidate| {
            let score = match_score(target, candidate);
            (score >= minimum_score).then(|| MatchCandidate {
                track: candidate.clone(),
                score,
            })
        })
        .collect::<Vec<_>>();
    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.track.id.provider.cmp(&b.track.id.provider))
            .then_with(|| a.track.id.id.cmp(&b.track.id.id))
    });
    matches
}

pub fn match_score(left: &Track, right: &Track) -> u8 {
    if let (Some(a), Some(b)) = (&left.isrc, &right.isrc)
        && !a.is_empty()
        && !b.is_empty()
    {
        return if a.eq_ignore_ascii_case(b) { 100 } else { 0 };
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
    title_score
        .saturating_add(artist_score)
        .saturating_add(duration_score)
        .saturating_add(album_score)
        .min(100)
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
}
