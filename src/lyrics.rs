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
#[cfg(feature = "lyrics-search")]
pub async fn search_many(
    sources: &[LyricsSearchSource],
    track: &crate::models::Track,
    full_search: bool,
    per_source_timeout: std::time::Duration,
) -> (Vec<crate::models::LyricSearchResult>, Vec<(String, String)>) {
    use futures::future::join_all;
    let metadata = to_search_metadata(track);
    let jobs = sources.iter().copied().map(|source| {
        let metadata = metadata.clone();
        async move {
            let result = tokio::time::timeout(
                per_source_timeout,
                search_source(source, &metadata, full_search),
            )
            .await;
            (source.id(), result)
        }
    });
    let mut results = Vec::new();
    let mut errors = Vec::new();
    for (source, result) in join_all(jobs).await {
        match result {
            Ok(Ok(items)) => results.extend(items),
            Ok(Err(error)) => errors.push((source.into(), error.to_string())),
            Err(_) => errors.push((source.into(), "lyrics search timed out".into())),
        }
    }
    results.sort_by(|a, b| {
        b.match_score
            .unwrap_or_default()
            .cmp(&a.match_score.unwrap_or_default())
    });
    (results, errors)
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
}
