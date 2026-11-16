//! Minimal LRC timestamp-line parser shared by all providers.
use crate::models::LyricLine;
use std::collections::BTreeMap;

/// Parse standard `[mm:ss.xx]text` lines; metadata tags (e.g. `[ti:...]`) are skipped.
// Not every enabled feature combination ships an LRC-consuming provider.
#[allow(dead_code)]
pub(crate) fn parse_lrc(raw: &str) -> Vec<LyricLine> {
    raw.lines()
        .filter_map(|line| {
            let end = line.find(']')?;
            let tag = line.get(1..end)?;
            let (m, s) = tag.split_once(':')?;
            let minutes: u64 = m.parse().ok()?;
            let seconds: f64 = s.parse().ok()?;
            Some(LyricLine {
                start_ms: minutes
                    .saturating_mul(60_000)
                    .saturating_add((seconds * 1000.0) as u64),
                end_ms: None,
                text: line.get(end + 1..)?.to_owned(),
                translations: BTreeMap::new(),
                romanization: None,
                words: vec![],
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc_and_skips_metadata() {
        let l = parse_lrc("[ti:demo]\n[01:02.50]hello\n[00:00.00]start");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].start_ms, 62500);
        assert_eq!(l[0].text, "hello");
        assert_eq!(l[1].start_ms, 0);
    }

    #[test]
    fn malformed_lines_are_dropped() {
        assert!(parse_lrc("no tags here\n[xx:yy]bad\n[[01:00]weird").is_empty());
    }
}
