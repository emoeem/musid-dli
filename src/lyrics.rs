//! Lyrics parsing/decryption facade. Online search is a separate Cargo feature.
use crate::{
    Error, Result,
    models::{LyricDocument, LyricFormat, LyricLine, LyricWord},
};
use std::collections::BTreeMap;

pub fn supported_formats() -> &'static [&'static str] {
    &[
        "LRC",
        "KRC",
        "QRC",
        "YRC",
        "TTML",
        "Spotify JSON",
        "Musixmatch JSON",
    ]
}

/// Parse a supported format into musid-dli's stable line/word model.
pub fn parse_document(input: &str) -> Result<LyricDocument> {
    let parsed = lyrics_helper::parse_auto(input)
        .ok_or_else(|| Error::InvalidInput("unsupported or malformed lyric payload".into()))?;
    let format = parsed
        .file
        .as_ref()
        .map(|file| match file.lyrics_type {
            lyrics_helper::LyricsTypes::Lrc => LyricFormat::Lrc,
            lyrics_helper::LyricsTypes::Krc => LyricFormat::Krc,
            lyrics_helper::LyricsTypes::Qrc => LyricFormat::Qrc,
            lyrics_helper::LyricsTypes::Yrc => LyricFormat::Yrc,
            lyrics_helper::LyricsTypes::Ttml => LyricFormat::Ttml,
            lyrics_helper::LyricsTypes::Spotify => LyricFormat::Spotify,
            lyrics_helper::LyricsTypes::Musixmatch => LyricFormat::Musixmatch,
            _ => LyricFormat::Unknown,
        })
        .unwrap_or(LyricFormat::Unknown);
    let lines = parsed
        .lines
        .unwrap_or_default()
        .into_iter()
        .map(|line| {
            let translations = line
                .translations()
                .map(|values| {
                    values
                        .iter()
                        .map(|(language, text)| (language.clone(), text.clone()))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            let words = line
                .syllables()
                .map(|syllables| {
                    syllables
                        .iter()
                        .map(|word| LyricWord {
                            text: word.text(),
                            start_ms: word.start_time().max(0) as u64,
                            duration_ms: word.duration().max(0) as u64,
                        })
                        .collect()
                })
                .unwrap_or_default();
            LyricLine {
                start_ms: line.start_time().unwrap_or_default().max(0) as u64,
                end_ms: line.end_time().map(|time| time.max(0) as u64),
                text: line.text_from_any(),
                translations,
                romanization: line.pronunciation().map(str::to_owned),
                words,
            }
        })
        .collect();
    Ok(LyricDocument {
        format,
        original: input.to_owned(),
        translation: None,
        romanization: None,
        lines,
    })
}

/// Alignment tolerance for document-level translation/romanization layers
/// (RESOURCE_DESIGN.md §7.2/§7.3): a document-level payload line whose
/// `start_ms` differs from the original line by at most this value is
/// merged into it. Producers whose payloads align exactly need no headroom;
/// 500 ms absorbs rounding across LRC variants.
pub const LAYER_ALIGNMENT_TOLERANCE_MS: u64 = 500;

/// Parse an original payload plus optional document-level translation /
/// romanization payloads (the netease `tlyric`/`rlyric` shape) into one
/// `LyricDocument` whose line maps carry the merged view while the
/// document-level raw slots stay verbatim (RESOURCE_DESIGN.md §7.3). Blank
/// or absent layer payloads are no-ops; an unparsable layer is the same
/// `Error::InvalidInput` as an unparsable original (no silent drops).
pub fn parse_document_layers(
    original: &str,
    translation: Option<&str>,
    romanization: Option<&str>,
) -> Result<LyricDocument> {
    let mut document = parse_document(original)?;
    // The document-level raw slots carry the payloads verbatim (serde-
    // completeness): parse_document builds a fresh document whose slots are
    // `None`, so the passed payloads are stored here, before the line-level
    // materialization below. Blank payloads carry nothing and stay `None`.
    if let Some(payload) = translation.filter(|payload| !payload.trim().is_empty()) {
        document.translation = Some(payload.to_owned());
    }
    if let Some(payload) = romanization.filter(|payload| !payload.trim().is_empty()) {
        document.romanization = Some(payload.to_owned());
    }
    let translation = match translation {
        Some(payload) if !payload.trim().is_empty() => Some(parse_document(payload)?),
        _ => None,
    };
    let romanization = match romanization {
        Some(payload) if !payload.trim().is_empty() => Some(parse_document(payload)?),
        _ => None,
    };
    document.materialize_layers(
        translation.as_ref(),
        romanization.as_ref(),
        LAYER_ALIGNMENT_TOLERANCE_MS,
    );
    Ok(document)
}

/// Normalize any supported payload to line-based LRC using lyrics-helper.
pub fn normalize_to_lrc(input: &str) -> Result<String> {
    let parsed = lyrics_helper::parse_auto(input)
        .ok_or_else(|| Error::InvalidInput("unsupported or malformed lyric payload".into()))?;
    lyrics_helper::generate_string(&parsed, lyrics_helper::LyricsTypes::Lrc)
        .ok_or_else(|| Error::InvalidInput("lyric conversion to LRC failed".into()))
}
pub fn decrypt_krc(input: &str) -> Option<String> {
    lyrics_helper::decrypt_krc(input)
}
pub fn decrypt_qrc(input: &str) -> Option<String> {
    lyrics_helper::decrypt_qrc(input)
}

#[cfg(feature = "lyrics-search")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricsSearchSource {
    Netease,
    QQMusic,
    Kugou,
    SodaMusic,
    LRCLIB,
    AmllTtmlDb,
}

#[cfg(feature = "lyrics-search")]
impl LyricsSearchSource {
    pub fn id(self) -> &'static str {
        match self {
            Self::Netease => "netease",
            Self::QQMusic => "qq",
            Self::Kugou => "kugou",
            Self::SodaMusic => "soda",
            Self::LRCLIB => "lrclib",
            Self::AmllTtmlDb => "amll-ttml-db",
        }
    }
}

#[cfg(feature = "lyrics-search")]
fn to_search_metadata(track: &crate::models::Track) -> lyrics_helper::TrackMetadata {
    lyrics_helper::TrackMetadata {
        title: Some(track.title.clone()),
        artist: Some(track.artists.join(", ")),
        album: track.album.clone(),
        duration_ms: track
            .duration
            .map(|duration| duration.as_millis().min(i32::MAX as u128) as i32),
        isrc: track.isrc.clone(),
        artists: Some(track.artists.clone()),
        ..Default::default()
    }
}

#[cfg(feature = "lyrics-search")]
async fn search_one<S: lyrics_helper::search::searchers::searcher::Searcher>(
    searcher: &S,
    provider_id: &str,
    metadata: &lyrics_helper::TrackMetadata,
    full_search: bool,
) -> Result<Vec<crate::models::LyricSearchResult>> {
    let results =
        lyrics_helper::search::searchers::search_with_refinement(searcher, metadata, full_search)
            .await
            .map_err(|error| Error::Provider {
                provider: searcher.name().into(),
                message: error.to_string(),
            })?;
    Ok(results
        .into_iter()
        .map(|result| crate::models::LyricSearchResult {
            provider: provider_id.to_owned(),
            id: result.id,
            title: result.title,
            artists: result.artists,
            album: (!result.album.is_empty()).then_some(result.album),
            duration_ms: result
                .duration_ms
                .filter(|duration| *duration >= 0)
                .map(|duration| duration as u64),
            match_score: result.match_type.map(|score| (score as i32).max(0) as u8),
        })
        .collect())
}

#[cfg(feature = "lyrics-search")]
async fn search_source(
    source: LyricsSearchSource,
    metadata: &lyrics_helper::TrackMetadata,
    full_search: bool,
) -> Result<Vec<crate::models::LyricSearchResult>> {
    use lyrics_helper::search::searchers::{
        amll_ttml_db::AmllTtmlDbSearcher, kugou::KugouSearcher, lrclib::LRCLIBSearcher,
        netease::NeteaseSearcher, qq_music::QQMusicSearcher, soda_music::SodaMusicSearcher,
    };
    match source {
        LyricsSearchSource::Netease => {
            search_one(&NeteaseSearcher, source.id(), metadata, full_search).await
        }
        LyricsSearchSource::QQMusic => {
            search_one(&QQMusicSearcher, source.id(), metadata, full_search).await
        }
        LyricsSearchSource::Kugou => {
            search_one(&KugouSearcher, source.id(), metadata, full_search).await
        }
        LyricsSearchSource::SodaMusic => {
            search_one(&SodaMusicSearcher, source.id(), metadata, full_search).await
        }
        LyricsSearchSource::LRCLIB => {
            search_one(&LRCLIBSearcher, source.id(), metadata, full_search).await
        }
        LyricsSearchSource::AmllTtmlDb => {
            search_one(&AmllTtmlDbSearcher, source.id(), metadata, full_search).await
        }
    }
}

/// Search multiple lyrics services concurrently. Per-source failures/timeouts
/// are isolated so a single blocked service does not suppress healthy results.
/// A thin delegation to [`search_many_typed`] with no health monitor and no
/// cancel token: the legacy tuple face rides the same engine (failures are
/// rendered as `(source id, message)`), so it benefits from any engine-level
/// behavior without a divergent copy of the loop.
#[cfg(feature = "lyrics-search")]
pub async fn search_many(
    sources: &[LyricsSearchSource],
    track: &crate::models::Track,
    full_search: bool,
    per_source_timeout: std::time::Duration,
) -> (Vec<crate::models::LyricSearchResult>, Vec<(String, String)>) {
    let (results, failures) =
        search_many_typed(sources, track, full_search, per_source_timeout, None, None).await;
    (
        results,
        failures
            .into_iter()
            .map(|failure| (failure.source.to_owned(), failure.message))
            .collect(),
    )
}

/// Typed per-source failure of the lyrics search (M6, RESOURCE_DESIGN.md
/// §8.3). Category honesty: `Timeout` and `Cancelled` are classified
/// natively by this face; lyrics-helper searcher errors arrive as the
/// legacy `Error::Provider` catch-all (the dependency bypasses musid's
/// error classification and transport, DEPENDENCY_AUDIT.md 2026-11-20
/// entry), so their category is `Provider` until a searcher replacement
/// lands (RESOURCE_DESIGN.md §11). Do not rely on this category for
/// searcher failures beyond the legacy catch-all semantics.
#[cfg(feature = "lyrics-search")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsSourceFailure {
    pub source: &'static str,
    pub category: crate::ErrorCategory,
    pub message: String,
}

/// Health monitor key for one lyrics source (RESOURCE_DESIGN.md §8.3).
/// Lyrics source ids share names with stream provider ids (`netease`,
/// `kugou` collide verbatim; `qq` does not even match the `qqmusic`
/// provider id), so lyrics health records under a namespaced key: a
/// monitor shared between the lyrics and stream faces can never let a
/// lyrics failure open a stream provider's breaker, and hosts query
/// `lyrics_health_key(source.id())` for the lyrics-side circuit state.
#[cfg(feature = "lyrics-search")]
pub fn lyrics_health_key(source: &str) -> String {
    format!("lyrics-source:{source}")
}

/// The engine of [`search_many`] with two optional collaborators
/// (RESOURCE_DESIGN.md §8.3): a provider health monitor receiving
/// category-classified successes/failures per source under the namespaced
/// [`lyrics_health_key`], and a cooperative cancel token checked before
/// each source job. `search_many` delegates here with both set to `None`.
///
/// A cancelled run reports one `Cancelled` failure per source instead of a
/// silent empty success: callers (the lyrics cache in particular) must be
/// able to tell "every source answered with nothing" from "the run never
/// happened", or a cancelled query would be cached as a full-freshness
/// absence (RESOURCE_DESIGN.md §8.3).
#[cfg(feature = "lyrics-search")]
pub async fn search_many_typed(
    sources: &[LyricsSearchSource],
    track: &crate::models::Track,
    full_search: bool,
    per_source_timeout: std::time::Duration,
    health: Option<&crate::health::ProviderHealthMonitor>,
    cancel: Option<&crate::search::SearchCancelToken>,
) -> (
    Vec<crate::models::LyricSearchResult>,
    Vec<LyricsSourceFailure>,
) {
    use futures::future::join_all;
    if let Some(cancel) = cancel
        && cancel.is_cancelled()
    {
        let failures = sources
            .iter()
            .map(|source| LyricsSourceFailure {
                source: source.id(),
                category: crate::ErrorCategory::Cancelled,
                message: "lyrics search cancelled".into(),
            })
            .collect();
        return (Vec::new(), failures);
    }
    let metadata = to_search_metadata(track);
    let jobs = sources.iter().copied().map(|source| {
        let metadata = metadata.clone();
        async move {
            if let Some(cancel) = cancel
                && cancel.is_cancelled()
            {
                return (
                    source,
                    Err(LyricsSourceFailure {
                        source: source.id(),
                        category: crate::ErrorCategory::Cancelled,
                        message: "lyrics search cancelled".into(),
                    }),
                );
            }
            let result = tokio::time::timeout(
                per_source_timeout,
                search_source(source, &metadata, full_search),
            )
            .await;
            match result {
                Ok(Ok(items)) => (source, Ok(items)),
                Ok(Err(error)) => (
                    source,
                    Err(LyricsSourceFailure {
                        source: source.id(),
                        category: error.category(),
                        message: error.to_string(),
                    }),
                ),
                Err(_) => (
                    source,
                    Err(LyricsSourceFailure {
                        source: source.id(),
                        category: crate::ErrorCategory::Timeout,
                        message: "lyrics search timed out".into(),
                    }),
                ),
            }
        }
    });
    let mut results = Vec::new();
    let mut failures = Vec::new();
    for (source, result) in join_all(jobs).await {
        match result {
            Ok(items) => {
                // Per-source success accounting (RESOURCE_DESIGN.md §8.3):
                // a healthy source must be able to recover the namespaced
                // breaker no matter what the other sources did, exactly
                // like the search aggregation records per successful page.
                if let Some(health) = health {
                    health.record_success(&lyrics_health_key(source.id()));
                }
                results.extend(items);
            }
            Err(failure) => {
                // A caller-cancelled job is not a provider failure: skip
                // the health record for it (Cancelled maps to Unknown and
                // would pollute the counters).
                if let Some(health) = health
                    && failure.category != crate::ErrorCategory::Cancelled
                {
                    health.record_failure_with_category(
                        &lyrics_health_key(failure.source),
                        failure.category,
                    );
                }
                failures.push(failure);
            }
        }
    }
    results.sort_by(|a, b| {
        b.match_score
            .unwrap_or_default()
            .cmp(&a.match_score.unwrap_or_default())
    });
    (results, failures)
}

/// [`search_many`] plus optional health classification and cancellation
/// (RESOURCE_DESIGN.md §8.3). Legacy tuple face: failures are rendered as
/// `(source id, message)` exactly like the pre-M6 shape. Health receives
/// the M2 category per failure (timeout maps to `Timeout`) and successes
/// are recorded per participating source.
#[cfg(feature = "lyrics-search")]
pub async fn search_many_with_health(
    sources: &[LyricsSearchSource],
    track: &crate::models::Track,
    full_search: bool,
    per_source_timeout: std::time::Duration,
    health: Option<&crate::health::ProviderHealthMonitor>,
    cancel: Option<&crate::search::SearchCancelToken>,
) -> (Vec<crate::models::LyricSearchResult>, Vec<(String, String)>) {
    let (results, failures) = search_many_typed(
        sources,
        track,
        full_search,
        per_source_timeout,
        health,
        cancel,
    )
    .await;
    (
        results,
        failures
            .into_iter()
            .map(|failure| (failure.source.to_owned(), failure.message))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lrc_is_normalized_without_network() {
        let input = "[00:01.00]Hello World";
        let output = normalize_to_lrc(input).unwrap();
        assert!(output.contains("Hello World"));
        let parsed = parse_document(input).unwrap();
        assert_eq!(parsed.format, LyricFormat::Lrc);
        assert_eq!(parsed.lines.first().unwrap().text, "Hello World");
        assert_eq!(parsed.lines.first().unwrap().start_ms, 1_000);
    }

    /// M6 §7.3 (R11): the netease producer shape (document-level raw
    /// translation payload, bare lines) normalizes into a document whose
    /// lines carry the merged translation map while the document-level raw
    /// slot stays verbatim. Milliseconds pass through unconverted (the
    /// lyrics-core i32 ms contract, lyrics.rs:59-67 pass-through).
    #[test]
    fn parse_document_layers_merges_document_payload_into_lines() {
        let original = "[00:01.00]Hello World\n[00:06.00]Second Line";
        let tlyric = "[00:01.00]你好世界\n[00:06.00]第二行";
        let document = parse_document_layers(original, Some(tlyric), None).unwrap();
        // Line maps are filled from the document-level payload.
        assert_eq!(
            document.lines[0].translations.get("").map(String::as_str),
            Some("你好世界")
        );
        assert_eq!(
            document.lines[1].translations.get("").map(String::as_str),
            Some("第二行")
        );
        // The document-level raw slot stays verbatim (serde-complete).
        assert_eq!(document.translation.as_deref(), Some(tlyric));
        assert_eq!(document.romanization, None);
        // No word timing: the payloads are line-timed only.
        assert!(!document.has_word_timing());
        // The merged view agrees with the materialized maps and records
        // that the data is line-level now (materialized): the provenance
        // contract (RESOURCE_DESIGN.md §7.2) survives materialization
        // through the UNSPECIFIED_LANGUAGE marker, so the view still tells
        // aligned-document translation from producer-supplied line data.
        let view = document.layered_lines(None, None, LAYER_ALIGNMENT_TOLERANCE_MS);
        assert_eq!(
            view[0].translation.as_ref().map(|t| t.text.as_str()),
            Some("你好世界")
        );
        assert_eq!(
            view[0].translation_source,
            Some(crate::models::LyricLayer::Document)
        );
        // Out-of-tolerance document payloads do not invent translations.
        let far = parse_document_layers(original, Some("[01:00.00]远离"), None).unwrap();
        assert!(far.lines[0].translations.is_empty());
        assert_eq!(far.translation.as_deref(), Some("[01:00.00]远离"));
        // Blank layer payloads are no-ops, not parse failures.
        let blank = parse_document_layers(original, Some("   "), None).unwrap();
        assert!(blank.lines.iter().all(|line| line.translations.is_empty()));
        assert_eq!(blank.translation, None);
    }

    /// M6 §7.1 ms semantics (R11 companion): the lyrics-core contract is
    /// line/word times in i32 milliseconds (lyrics-core-0.5.1
    /// line_info.rs:19-37, syllable_info.rs:5-27), so a word-timed payload
    /// must arrive in musid as the exact same integers, unit-untouched.
    /// YRC line-header times are not kept by the parser; line times fall
    /// back to the first/last syllable, and `end_ms` comes from that
    /// payload-derived fallback, never from an invented duration.
    #[test]
    fn parse_document_pins_i32_ms_word_timing_pass_through() {
        let payload = "[1000,500](1000,200,0)He(1200,300,0)llo\n[2000,300](2000,300,0)World";
        let parsed = parse_document(payload).unwrap();
        assert_eq!(parsed.format, LyricFormat::Yrc);
        assert!(parsed.has_word_timing());
        // Word times are the payload's own integers: start/duration in ms,
        // no seconds conversion, no scaling.
        let words: Vec<_> = parsed.lines[0]
            .words
            .iter()
            .map(|word| (word.text.as_str(), word.start_ms, word.duration_ms))
            .collect();
        assert_eq!(
            words,
            vec![("He", 1_000, 200), ("llo", 1_200, 300)],
            "word start/duration must equal the payload integers in ms"
        );
        // Line times fall back to the first/last syllable of the SAME
        // payload (lyrics-core line_info.rs:30-37), not fabricated values.
        assert_eq!(parsed.lines[0].start_ms, 1_000);
        assert_eq!(parsed.lines[0].end_ms, Some(1_500));
        assert_eq!(parsed.lines[1].start_ms, 2_000);
        assert_eq!(parsed.lines[1].end_ms, Some(2_300));
    }

    /// M6 §7.1 ms semantics: line-timed LRC keeps `end_ms` absent (the
    /// Option contract, models/lyrics.rs:27-29: no duration is ever
    /// fabricated from start + a guessed length), and fractional LRC
    /// timestamps arrive as scaled milliseconds, not seconds or ticks.
    #[test]
    fn parse_document_keeps_end_ms_optional_for_line_timed_payloads() {
        let parsed = parse_document("[00:01.50]Hello\n[00:06.00]Second").unwrap();
        assert_eq!(parsed.format, LyricFormat::Lrc);
        assert_eq!(
            parsed.lines[0].start_ms, 1_500,
            "fractional seconds scale to exact milliseconds"
        );
        assert_eq!(parsed.lines[1].start_ms, 6_000);
        assert!(
            parsed.lines.iter().all(|line| line.end_ms.is_none()),
            "line-timed payload never invents an end time"
        );
        assert!(!parsed.has_word_timing());
    }

    /// M6 §7.1 ms semantics: the wire times are i32 and can go negative
    /// (an LRC `offset` attribute shifts raw timestamps below zero,
    /// lyrics-parsers lrc_parser.rs:61/92 `*ts - offset`); musid clamps at
    /// the parse boundary with `.max(0)` (lyrics.rs:59-67) instead of
    /// wrapping or keeping a negative that would break the u64 model.
    #[test]
    fn parse_document_clamps_negative_ms_to_zero() {
        let parsed = parse_document("[offset:5000]\n[00:01.00]Hello").unwrap();
        assert_eq!(
            parsed.lines[0].start_ms, 0,
            "raw i32 -4000 clamps to 0 at the parse boundary"
        );
        // The clamp must not conjure an end time either.
        assert_eq!(parsed.lines[0].end_ms, None);
    }

    /// M6 §7.1: unsupported payloads fail loudly as `Error::InvalidInput`
    /// (lyrics.rs:21-23 rule) instead of producing a fabricated timeline.
    #[test]
    fn parse_document_rejects_unsupported_payloads_with_invalid_input() {
        for payload in ["", "plain prose without any lyric markup"] {
            match parse_document(payload) {
                Err(Error::InvalidInput(_)) => {}
                other => panic!("expected InvalidInput for {payload:?}, got {other:?}"),
            }
        }
    }

    /// Review P1: lyrics health records under a namespaced key so a
    /// monitor shared with the stream faces can never cross-pollute
    /// (`netease`/`kugou` collide with provider ids, `qq` does not even
    /// match `qqmusic`).
    #[cfg(feature = "lyrics-search")]
    #[test]
    fn lyrics_health_keys_are_namespaced_away_from_provider_ids() {
        assert_eq!(lyrics_health_key("netease"), "lyrics-source:netease");
        for source in ["netease", "qq", "kugou", "soda", "lrclib", "amll-ttml-db"] {
            assert_ne!(
                lyrics_health_key(source),
                source,
                "the lyrics key must never equal a (provider-looking) id"
            );
        }
    }

    /// Review round 2, issue 1: a cancelled run reports one `Cancelled`
    /// failure per source instead of a silent empty success. The pre-check
    /// short-circuits before any network, so the test needs no searcher.
    /// Callers (the lyrics cache) use the failures to avoid storing a fake
    /// absence in the positive domain.
    #[cfg(feature = "lyrics-search")]
    #[tokio::test]
    async fn cancelled_search_reports_cancelled_failures_per_source() {
        let cancel = crate::search::SearchCancelToken::new();
        cancel.cancel();
        let sources = [
            LyricsSearchSource::Netease,
            LyricsSearchSource::LRCLIB,
            LyricsSearchSource::QQMusic,
        ];
        let (results, failures) = search_many_typed(
            &sources,
            &crate::models::Track {
                id: crate::TrackId::new("netease", "1"),
                title: "t".into(),
                artists: vec![],
                album: None,
                album_id: None,
                duration: None,
                artwork_url: None,
                isrc: None,
                available_qualities: vec![],
                audio: None,
                extra: Default::default(),
            },
            false,
            std::time::Duration::from_secs(1),
            None,
            Some(&cancel),
        )
        .await;
        assert!(results.is_empty());
        assert_eq!(failures.len(), 3, "one Cancelled failure per source");
        assert!(
            failures
                .iter()
                .all(|failure| failure.category == crate::ErrorCategory::Cancelled),
            "every failure is Cancelled, never an implicit empty success"
        );
    }

    /// m6-lyrics-helper-audit pin (docs/DEPENDENCY_AUDIT.md 2026-11-20): the
    /// accepted cost of the `lyrics-search` face is the lyrics-helper 0.5.1
    /// search stack shipping its own HTTP stack, reqwest 0.13.x with base64
    /// 0.23.x, alongside musid's reqwest 0.12.x with base64 0.22.x (a
    /// version-difference pair of dual majors, Cargo.lock). This test parses
    /// Cargo.lock (compile-time `include_str!`, feature-independent) and
    /// fails when the audited premise changes either way: musid moving past
    /// reqwest 0.12, lyrics-helper dropping or re-versioning its reqwest
    /// 0.13, or a searcher replacement collapsing the dual major. Such a
    /// failure forces the audit entry and the keep-vs-replace decision to be
    /// revisited instead of the dependency graph drifting silently.
    #[cfg(feature = "lyrics-search")]
    #[test]
    fn cargo_lock_pins_the_audited_dual_reqwest_stack() {
        let lock = include_str!("../Cargo.lock");
        let mut name: Option<String> = None;
        let mut versions: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        let mut lyrics_search_reqwest: Option<String> = None;
        for line in lock.lines() {
            if line == "[[package]]" {
                name = None;
                continue;
            }
            if let Some(rest) = line.strip_prefix("name = \"") {
                name = Some(rest.trim_end_matches('"').to_owned());
            } else if let Some(rest) = line.strip_prefix("version = \"") {
                if let Some(crate_name) = name.clone() {
                    versions
                        .entry(crate_name)
                        .or_default()
                        .push(rest.trim_end_matches('"').to_owned());
                }
            } else if line.trim_start().starts_with('"') {
                // A dependency entry of the block currently being read. The
                // lyrics-search block is identified by the name seen last.
                let dep = line.trim().trim_end_matches(',').trim_matches('"');
                if name.as_deref() == Some("lyrics-search")
                    && dep.split(' ').next() == Some("reqwest")
                {
                    // Versioned entries look like `reqwest 0.13.5`; plain
                    // entries mean the lock carries a single reqwest major.
                    lyrics_search_reqwest = Some(dep.to_owned());
                }
            }
        }
        let majors = |crate_name: &str| -> Vec<String> {
            let mut keys: Vec<String> = versions
                .get(crate_name)
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|version| version.split('.').take(2).collect::<Vec<_>>().join("."))
                .collect();
            keys.sort();
            keys.dedup();
            keys
        };
        assert_eq!(
            majors("reqwest"),
            vec!["0.12", "0.13"],
            "the audited dual reqwest major changed: update docs/DEPENDENCY_AUDIT.md \
             (keep-vs-replace decision) and this pin"
        );
        assert_eq!(
            majors("base64"),
            vec!["0.22", "0.23"],
            "the audited dual base64 changed alongside the searcher stack"
        );
        let helper = versions
            .get("lyrics-helper")
            .expect("lyrics-helper in lock");
        assert_eq!(
            helper.len(),
            1,
            "exactly one lyrics-helper resolution is audited"
        );
        assert!(
            helper[0].starts_with("0.5."),
            "audited lyrics-helper release drifted: {helper:?}"
        );
        let pinned = lyrics_search_reqwest.expect("lyrics-search depends on reqwest");
        assert!(
            pinned == "reqwest" || pinned.starts_with("reqwest 0.13."),
            "the searcher's reqwest is no longer the 0.13 major: {pinned}"
        );
    }
}
