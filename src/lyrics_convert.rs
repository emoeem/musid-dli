//! Lyric conversion semantics between the voicefox host model set and the
//! musid-dli lyric model. M3 slice s3; contract in
//! docs/integration/MODEL_ALIGNMENT_DESIGN.md §4, baseline in
//! docs/integration/MODEL_MAPPING.md §5.
//!
//! voicefox types do not exist in this crate, so the module carries
//! field-faithful **mirror types** of the host shapes (voicefox
//! `core/src/model/lyric.rs:5-17` `LyricData`, `:20-28` `LyricLine`,
//! `:30-43` `YrcLine`/`YrcWord`). Mirror fields keep the voicefox names so a
//! host adapter can pipe its own structs (or JSON) straight in; every field
//! is `#[serde(default)]` per the R1 additive rule.
//!
//! Loss handling is explicit on both directions (no silent drops):
//!
//! - voicefox → musid: `lyric`/`tlyric`/`rlyric` map verbatim; the
//!   word-timed `lxlyric` payload is **parsed** into `lines[].words`, never
//!   stuffed into a string slot. An unparsable or word-less `lxlyric` is a
//!   hard [`Error::InvalidInput`] — the raw string has no slot to survive
//!   in, so the caller retries without it to degrade explicitly. `raw_lrc`
//!   has no dedicated slot (pinned lossy in MODEL_MAPPING.md §5): it is
//!   promoted to `original` only when `lyric` is blank, and it travels with
//!   the caller-supplied [`LyricFormat`] so the `.lrc` writer guard
//!   (`standard_lyric`/`looks_like_timed_lrc`, `download.rs:1327` and
//!   `:1350-1366`) never writes a private JSON payload as `.lrc`.
//! - musid → voicefox: word timing is dropped **explicitly** (`lxlyric`
//!   stays `None`; there is no YRC/QRC serializer in M3 core, §4.2), while
//!   the line text survives via `lyric` — trusted verbatim when the
//!   original carries `mm:ss.xx` brackets, otherwise rendered from the
//!   parsed lines (word texts substituted for blank line texts). The
//!   unprocessed original rides `raw_lrc` (voicefox's "未处理的原始歌词"
//!   slot, as produced by `source/src/wy/lyric.rs:81` and
//!   `source/src/kg/lyric.rs:184`). `LyricLine.end_ms` maps to the voicefox
//!   next-line duration rule (`lyric.rs:26-27`) via
//!   [`lyric_lines_from_document`]. Per-line `translations`/`romanization`
//!   and `LyricFormat` have no voicefox document-level slot and are dropped
//!   (M0 lossy register).

use crate::{
    Error, Result,
    models::{LyricDocument, LyricFormat},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// voicefox `LyricData` mirror (`core/src/model/lyric.rs:5-17`): the host
/// lyric payload as the sources produce it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LyricData {
    /// 原始歌词（LRC 格式）
    #[serde(default)]
    pub lyric: String,
    /// 翻译歌词
    #[serde(default)]
    pub tlyric: Option<String>,
    /// 罗马音歌词
    #[serde(default)]
    pub rlyric: Option<String>,
    /// 逐字歌词（YRC/QRC 格式）
    #[serde(default)]
    pub lxlyric: Option<String>,
    /// 未处理的原始歌词
    #[serde(default)]
    pub raw_lrc: Option<String>,
}

/// voicefox `LyricLine` mirror (`core/src/model/lyric.rs:20-28`): one parsed
/// LRC line; the duration is derived from the next line by
/// [`lyric_lines_from_document`]. Distinct from [`crate::models::LyricLine`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LyricLine {
    /// 该行起始时间（毫秒）
    #[serde(default)]
    pub timestamp: u64,
    /// 该行歌词文本
    #[serde(default)]
    pub text: String,
    /// 该行持续时长（毫秒），由下一行时间戳推算
    #[serde(default)]
    pub duration: u64,
}

/// voicefox `YrcLine` mirror (`core/src/model/lyric.rs:30-35`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct YrcLine {
    #[serde(default)]
    pub timestamp: u64,
    #[serde(default)]
    pub words: Vec<YrcWord>,
}

/// voicefox `YrcWord` mirror (`core/src/model/lyric.rs:37-43`): `{text,
/// start, duration}` against the musid-dli `{text, start_ms, duration_ms}`
/// (`src/models/lyrics.rs:18-23`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct YrcWord {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub start: u64,
    #[serde(default)]
    pub duration: u64,
}

impl From<YrcWord> for crate::models::LyricWord {
    fn from(word: YrcWord) -> Self {
        Self {
            text: word.text,
            start_ms: word.start,
            duration_ms: word.duration,
        }
    }
}

impl From<crate::models::LyricWord> for YrcWord {
    fn from(word: crate::models::LyricWord) -> Self {
        Self {
            text: word.text,
            start: word.start_ms,
            duration: word.duration_ms,
        }
    }
}

impl From<YrcLine> for crate::models::LyricLine {
    fn from(line: YrcLine) -> Self {
        let text = line
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<String>();
        Self {
            start_ms: line.timestamp,
            end_ms: None,
            text,
            translations: BTreeMap::new(),
            romanization: None,
            words: line.words.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<crate::models::LyricLine> for YrcLine {
    fn from(line: crate::models::LyricLine) -> Self {
        Self {
            timestamp: line.start_ms,
            words: line.words.into_iter().map(Into::into).collect(),
        }
    }
}

/// Convert a voicefox `LyricData` into a musid-dli [`LyricDocument`].
///
/// `format` is the producer's real lyric format and lands on the document
/// verbatim — it is never defaulted (§4.1: provenance rides `format` +
/// `original` so the engine's `.lrc` writer never emits a private payload).
///
/// Field mapping (§4.1):
/// - `lyric` → `original` verbatim; when `lyric` is blank, a non-blank
///   `raw_lrc` is promoted to `original` (the unprocessed payload is then
///   the only original; the format marker keeps the `.lrc` writer honest).
/// - `tlyric`/`rlyric` → `translation`/`romanization` verbatim.
/// - `lxlyric` → **parsed** into `lines[].words` (`YrcWord{text, start,
///   duration}` → `LyricWord{text, start_ms, duration_ms}`), line
///   `start_ms` from the payload line headers, `end_ms` from the payload.
///   An unparsable payload, or one that parses without word timing (e.g.
///   the rendered KRC hybrid voicefox kugou emits, `source/src/kg/lyric.rs:
///   170-183`, which lyrics-helper detects as plain LRC), is a hard
///   [`Error::InvalidInput`]: the raw string has no slot in the document,
///   so degrading silently is forbidden. Retry the call with `lxlyric =
///   None` to degrade explicitly to line text.
/// - `lyric` is additionally parsed as a timed LRC to build `lines[].text`
///   (its `end_ms` stays `None` — voicefox LRC lines carry no end time and
///   durations are not invented, §4.1). Word-timed lines and LRC lines are
///   merged on equal `start_ms` (the convention `merge_lyric` uses,
///   `download.rs:1293-1324`); a non-blank word-line text wins, LRC-only
///   lines are appended. A `lyric` that does not parse as LRC is graceful:
///   the string still survives verbatim in `original`.
pub fn lyric_data_to_document(data: &LyricData, format: LyricFormat) -> Result<LyricDocument> {
    let original = match data.lyric.trim().is_empty() {
        false => data.lyric.clone(),
        true => data
            .raw_lrc
            .as_deref()
            .filter(|raw| !raw.trim().is_empty())
            .unwrap_or_default()
            .to_owned(),
    };

    let mut merged: BTreeMap<u64, crate::models::LyricLine> = BTreeMap::new();
    for line in parse_word_lines(data.lxlyric.as_deref())? {
        merged.entry(line.start_ms).or_insert(line);
    }

    if !data.lyric.trim().is_empty()
        && let Ok(lrc) = crate::lyrics::parse_document(&data.lyric)
    {
        for line in lrc.lines {
            // LRC carries no end times: leave `end_ms` None rather than
            // inventing durations the parser did not compute (§4.1).
            let line = crate::models::LyricLine {
                end_ms: None,
                ..line
            };
            match merged.get_mut(&line.start_ms) {
                Some(word_line) => {
                    if word_line.text.trim().is_empty() {
                        word_line.text = line.text;
                    }
                }
                None => {
                    merged.insert(line.start_ms, line);
                }
            }
        }
    }

    Ok(LyricDocument {
        format,
        original,
        translation: data.tlyric.clone(),
        romanization: data.rlyric.clone(),
        lines: merged.into_values().collect(),
    })
}

/// Convert a musid-dli [`LyricDocument`] into a voicefox `LyricData`.
///
/// Field mapping (§4.2):
/// - `lyric` gets the LRC-suited text: the original verbatim when it
///   carries bracketed `mm:ss.xx` timestamps (the engine's trust rule,
///   `download.rs:1327-1348`), otherwise the parsed lines rendered as LRC
///   with word texts substituted for blank line texts — the explicit
///   word-timing degradation. When nothing is usable the field is empty.
/// - `raw_lrc` carries the unprocessed original verbatim whenever it is
///   non-blank (voicefox producers keep the payload there: wy
///   `source/src/wy/lyric.rs:81`, kg `source/src/kg/lyric.rs:184`).
/// - `translation`/`romanization` → `tlyric`/`rlyric` verbatim.
/// - `lxlyric` stays `None`: there is no YRC/QRC serializer in M3 core
///   (§4.2), and a synthesized word string would be a fake one. Word
///   timing lives in `lines[].words`; the host consumes it through
///   [`lyric_lines_from_document`] or the [`YrcWord`] mapping.
/// - Per-line `translations`/`romanization` and `format` have no voicefox
///   document-level slot and are dropped (M0 lossy register, §4.2).
pub fn document_to_lyric_data(document: &LyricDocument) -> LyricData {
    LyricData {
        lyric: trusted_or_rendered_lrc(document).unwrap_or_default(),
        tlyric: document.translation.clone(),
        rlyric: document.romanization.clone(),
        lxlyric: None,
        raw_lrc: (!document.original.trim().is_empty()).then(|| document.original.clone()),
    }
}

/// Map document lines onto the voicefox `LyricLine` shape
/// (`core/src/model/lyric.rs:20-28`): `start_ms` → `timestamp`, and
/// `duration` from `end_ms - start_ms` when the document carries an end
/// time, otherwise from the next line's `start_ms` minus this one, else 0
/// on the last line (voicefox derives the duration from the next line,
/// `lyric.rs:26-27`; §4.2 "end_ms: None lines get duration 0 + the
/// next-line rule"). Blank line texts fall back to the concatenation of
/// the word texts, matching the renderer in `download.rs:1332-1344`.
pub fn lyric_lines_from_document(lines: &[crate::models::LyricLine]) -> Vec<LyricLine> {
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let timestamp = line.start_ms;
            let duration = match line.end_ms {
                Some(end_ms) => end_ms.saturating_sub(timestamp),
                None => lines
                    .get(index + 1)
                    .map_or(0, |next| next.start_ms.saturating_sub(timestamp)),
            };
            LyricLine {
                timestamp,
                text: line_text(line),
                duration,
            }
        })
        .collect()
}

fn line_text(line: &crate::models::LyricLine) -> String {
    if line.text.trim().is_empty() {
        line.words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<String>()
    } else {
        line.text.clone()
    }
}

/// Parse the raw word-timed `lxlyric` payload, or fail loudly. The raw
/// string has no slot in [`LyricDocument`], so a payload that does not
/// yield word-level timing must error instead of degrading silently.
fn parse_word_lines(lxlyric: Option<&str>) -> Result<Vec<crate::models::LyricLine>> {
    let Some(payload) = lxlyric.map(str::trim).filter(|payload| !payload.is_empty()) else {
        return Ok(Vec::new());
    };
    let parsed = crate::lyrics::parse_document(payload).map_err(|_| {
        Error::InvalidInput(
            "lxlyric payload is not a parsable word-timed lyric (YRC/QRC/KRC); retry the \
             conversion without lxlyric to degrade to line text explicitly"
                .into(),
        )
    })?;
    if parsed.lines.iter().all(|line| line.words.is_empty()) {
        return Err(Error::InvalidInput(
            "lxlyric payload carries no word-level timing (e.g. the rendered KRC hybrid \
             voicefox kugou emits, source/src/kg/lyric.rs:170-183); retry the conversion \
             without lxlyric to degrade to line text explicitly"
                .into(),
        ));
    }
    Ok(parsed.lines)
}

/// The `.lrc`-suited text of a document: byte-level mirror of
/// `standard_lyric` (`src/download.rs:1327-1348`, which lives behind the
/// `download` feature this module must not depend on). The raw text is
/// trusted only when it looks like timed LRC; otherwise the parsed lines
/// are rendered with word texts substituted for blank line texts.
fn trusted_or_rendered_lrc(document: &LyricDocument) -> Option<String> {
    if looks_like_timed_lrc(&document.original) {
        return Some(document.original.trim().to_owned());
    }
    let mut rendered = String::new();
    for line in &document.lines {
        let text = line_text(line);
        let text = text.trim();
        if !text.is_empty() {
            rendered.push_str(&format_lrc_line(line.start_ms, text));
        }
    }
    (!rendered.trim().is_empty()).then_some(rendered)
}

/// Byte-level mirror of `looks_like_timed_lrc` (`src/download.rs:1350-1366`,
/// behind the `download` feature): does the text carry at least one
/// bracketed `mm:ss` timestamp?
fn looks_like_timed_lrc(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    for index in 0..bytes.len().saturating_sub(6) {
        if bytes[index] == b'['
            && bytes[index + 1].is_ascii_digit()
            && bytes[index + 2].is_ascii_digit()
            && bytes[index + 3] == b':'
            && bytes[index + 4].is_ascii_digit()
        {
            return true;
        }
    }
    false
}

/// Byte-level mirror of `format_lrc_line` (`src/download.rs:1368-1372`,
/// behind the `download` feature): `[mm:ss.xx]text\n`.
fn format_lrc_line(timestamp_ms: u64, text: &str) -> String {
    let minutes = timestamp_ms / 60_000;
    let seconds = (timestamp_ms % 60_000) as f64 / 1000.0;
    format!("[{minutes:02}:{seconds:05.2}]{}\n", text.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{LyricLine as MusidLyricLine, LyricWord as MusidLyricWord};

    const YRC: &str = "[12000,3000](12000,400,0)Hel(12400,500,0)lo\n[15000,2000](15000,900,0)World";
    const QRC: &str = "[12000,3000]Hel(12000,400)lo(12400,500)\n[15000,2000]World(15000,800)";
    const KRC: &str = "[12000,3000]<0,400,0>Hel<400,500,0>lo\n[15000,2000]<0,900,0>World";
    /// The rendered KRC hybrid voicefox kugou emits (`source/src/kg/lyric.rs:
    /// 170-183`): `mm:ss.xx` line headers plus `<start,dur>` word tags, which
    /// lyrics-helper detects as plain LRC — no word timing extractable.
    const KG_HYBRID: &str = "[00:12.00]<0,400,0>Hel<400,500,0>lo";
    const GARBAGE: &str = "hello world not a lyric at all";

    fn word(text: &str, start_ms: u64, duration_ms: u64) -> MusidLyricWord {
        MusidLyricWord {
            text: text.to_owned(),
            start_ms,
            duration_ms,
        }
    }

    fn musid_line(
        start_ms: u64,
        end_ms: Option<u64>,
        text: &str,
        words: Vec<MusidLyricWord>,
    ) -> MusidLyricLine {
        MusidLyricLine {
            start_ms,
            end_ms,
            text: text.to_owned(),
            translations: BTreeMap::new(),
            romanization: None,
            words,
        }
    }

    #[test]
    fn lyric_data_maps_all_fields_and_word_lines() {
        let data = LyricData {
            lyric: "[00:12.00]Hello World\n[00:18.00]Outro".into(),
            tlyric: Some("[00:12.00]你好".into()),
            rlyric: Some("[00:12.00]nǐ hǎo".into()),
            lxlyric: Some(YRC.into()),
            raw_lrc: Some("RAW-KRC-CONTENT".into()),
        };
        let doc = lyric_data_to_document(&data, LyricFormat::Yrc).unwrap();
        // Format never defaulted; string fields verbatim.
        assert_eq!(doc.format, LyricFormat::Yrc);
        assert_eq!(doc.original, "[00:12.00]Hello World\n[00:18.00]Outro");
        assert_eq!(doc.translation.as_deref(), Some("[00:12.00]你好"));
        assert_eq!(doc.romanization.as_deref(), Some("[00:12.00]nǐ hǎo"));
        // Merged timeline: word lines 12000/15000 from YRC + LRC line 18000.
        assert_eq!(doc.lines.len(), 3);
        assert_eq!(doc.lines[0].start_ms, 12000);
        assert_eq!(doc.lines[0].text, "Hello"); // word-line text wins
        assert_eq!(doc.lines[0].end_ms, Some(12900)); // payload-computed
        assert_eq!(
            doc.lines[0].words,
            vec![word("Hel", 12000, 400), word("lo", 12400, 500)]
        );
        assert_eq!(doc.lines[1].start_ms, 15000);
        assert_eq!(doc.lines[1].words, vec![word("World", 15000, 900)]);
        // LRC-only line: end_ms stays None, no durations invented.
        assert_eq!(doc.lines[2].start_ms, 18000);
        assert_eq!(doc.lines[2].text, "Outro");
        assert_eq!(doc.lines[2].end_ms, None);
        assert!(doc.lines[2].words.is_empty());
    }

    #[test]
    fn qrc_and_krc_word_payloads_parse_into_words() {
        for (payload, expected_format) in [(QRC, LyricFormat::Qrc), (KRC, LyricFormat::Krc)] {
            let doc = lyric_data_to_document(
                &LyricData {
                    lxlyric: Some(payload.into()),
                    ..Default::default()
                },
                expected_format,
            )
            .unwrap();
            assert_eq!(doc.lines.len(), 2);
            assert_eq!(doc.lines[0].start_ms, 12000);
            assert_eq!(doc.lines[0].words.len(), 2);
            assert_eq!(doc.lines[0].words[1], word("lo", 12400, 500));
            assert_eq!(
                doc.lines[1].words,
                vec![word("World", 15000, expected_word_duration(payload))]
            );
        }
    }

    fn expected_word_duration(payload: &str) -> u64 {
        if payload == QRC { 800 } else { 900 }
    }

    #[test]
    fn unparsable_or_wordless_lxlyric_is_an_error_not_a_silent_drop() {
        let data = LyricData {
            lyric: "[00:12.00]Hello".into(),
            lxlyric: Some(GARBAGE.into()),
            ..Default::default()
        };
        let error = lyric_data_to_document(&data, LyricFormat::Lrc).unwrap_err();
        assert!(error.to_string().contains("lxlyric"));

        // The kugou rendered hybrid parses as plain LRC with no word timing.
        let hybrid = LyricData {
            lxlyric: Some(KG_HYBRID.into()),
            ..Default::default()
        };
        let error = lyric_data_to_document(&hybrid, LyricFormat::Krc).unwrap_err();
        assert!(error.to_string().contains("word-level timing"));

        // The documented explicit degradation path: retry without lxlyric.
        let degraded = lyric_data_to_document(
            &LyricData {
                lyric: "[00:12.00]Hello".into(),
                ..Default::default()
            },
            LyricFormat::Krc,
        )
        .unwrap();
        assert_eq!(degraded.lines.len(), 1);
        assert_eq!(degraded.lines[0].text, "Hello");
    }

    #[test]
    fn empty_lyric_promotes_raw_payload_and_keeps_provenance() {
        let private_json = r#"{"header":{"status":0},"types":["mxm"]}"#;
        let data = LyricData {
            lyric: String::new(),
            raw_lrc: Some(private_json.into()),
            ..Default::default()
        };
        let doc = lyric_data_to_document(&data, LyricFormat::Musixmatch).unwrap();
        assert_eq!(doc.original, private_json);
        assert_eq!(doc.format, LyricFormat::Musixmatch);
        assert!(doc.lines.is_empty());
        assert!(!looks_like_timed_lrc(&doc.original));

        // Round trip: the private payload must not surface as usable LRC.
        let back = document_to_lyric_data(&doc);
        assert_eq!(back.lyric, String::new());
        assert_eq!(back.raw_lrc.as_deref(), Some(private_json));
        assert_eq!(back.lxlyric, None);
    }

    #[test]
    fn timed_raw_is_trusted_verbatim_both_directions() {
        let doc = LyricDocument {
            format: LyricFormat::Lrc,
            original: "[00:01.00]Hello".into(),
            translation: None,
            romanization: None,
            lines: Vec::new(),
        };
        let data = document_to_lyric_data(&doc);
        assert_eq!(data.lyric, "[00:01.00]Hello");
        assert_eq!(data.raw_lrc.as_deref(), Some("[00:01.00]Hello"));

        // Engine-parity edge (download.rs:1350-1366): the byte scan matches
        // a bracketed timestamp anywhere, including inside a JSON string.
        assert!(looks_like_timed_lrc("{\"a\":\"[00:01.00]x\"}"));
        assert!(looks_like_timed_lrc("[01:02.03]x"));
        assert!(!looks_like_timed_lrc("[12000,3000](12000,400,0)Hel"));
        assert!(!looks_like_timed_lrc("   "));
        assert!(!looks_like_timed_lrc("{\"a\":1}"));
    }

    #[test]
    fn word_document_degrades_line_text_explicitly() {
        let doc = LyricDocument {
            format: LyricFormat::Qrc,
            original: "<QRC-XML-PAYLOAD>".into(),
            translation: Some("译".into()),
            romanization: Some("yi".into()),
            lines: vec![
                musid_line(
                    1000,
                    Some(2000),
                    "",
                    vec![word("你", 1000, 500), word("好", 1500, 500)],
                ),
                musid_line(3000, None, "World", Vec::new()),
            ],
        };
        let data = document_to_lyric_data(&doc);
        // Word timing dropped explicitly; line text survives rendered.
        assert_eq!(data.lxlyric, None);
        assert!(data.lyric.contains("[00:01.00]你好"));
        assert!(data.lyric.contains("[00:03.00]World"));
        assert_eq!(data.raw_lrc.as_deref(), Some("<QRC-XML-PAYLOAD>"));
        assert_eq!(data.tlyric.as_deref(), Some("译"));
        assert_eq!(data.rlyric.as_deref(), Some("yi"));

        let lines = lyric_lines_from_document(&doc.lines);
        assert_eq!(lines[0].timestamp, 1000);
        assert_eq!(lines[0].duration, 1000); // end_ms wins
        assert_eq!(lines[0].text, "你好"); // word fallback for blank text
        assert_eq!(lines[1].timestamp, 3000);
        assert_eq!(lines[1].duration, 0); // last line without end_ms
    }

    #[test]
    fn duration_uses_end_ms_else_next_line_else_zero() {
        let lines = vec![
            musid_line(1000, None, "A", Vec::new()),
            musid_line(3000, Some(4500), "B", Vec::new()),
            musid_line(6000, None, "C", Vec::new()),
        ];
        let mapped = lyric_lines_from_document(&lines);
        assert_eq!(mapped[0].duration, 2000); // next line's timestamp
        assert_eq!(mapped[1].duration, 1500); // document end_ms
        assert_eq!(mapped[2].duration, 0); // last line

        // Saturating on inverted/overlapping timestamps.
        let overlapping = vec![
            musid_line(100, Some(50), "D", Vec::new()),
            musid_line(50, None, "E", Vec::new()),
        ];
        let mapped = lyric_lines_from_document(&overlapping);
        assert_eq!(mapped[0].duration, 0);
        assert_eq!(mapped[1].duration, 0);
    }

    #[test]
    fn yrc_word_field_mapping_round_trips() {
        let yrc = YrcWord {
            text: "你".into(),
            start: 1234,
            duration: 567,
        };
        let musid: MusidLyricWord = yrc.clone().into();
        assert_eq!(
            musid,
            MusidLyricWord {
                text: "你".into(),
                start_ms: 1234,
                duration_ms: 567,
            }
        );
        let back: YrcWord = musid.into();
        assert_eq!(back, yrc);
    }

    #[test]
    fn yrc_line_mapping_derives_text_and_words() {
        let yrc = YrcLine {
            timestamp: 12000,
            words: vec![
                YrcWord {
                    text: "Hel".into(),
                    start: 12000,
                    duration: 400,
                },
                YrcWord {
                    text: "lo".into(),
                    start: 12400,
                    duration: 500,
                },
            ],
        };
        let musid: MusidLyricLine = yrc.clone().into();
        assert_eq!(musid.start_ms, 12000);
        assert_eq!(musid.text, "Hello"); // derived from word texts
        assert_eq!(musid.end_ms, None);
        assert_eq!(
            musid.words,
            vec![word("Hel", 12000, 400), word("lo", 12400, 500)]
        );

        let back: YrcLine =
            musid_line(12000, Some(15000), "ignored", vec![word("你", 12000, 300)]).into();
        assert_eq!(back.timestamp, 12000);
        assert_eq!(
            back.words,
            vec![YrcWord {
                text: "你".into(),
                start: 12000,
                duration: 300,
            }]
        );
    }

    #[test]
    fn legacy_json_compat_both_models() {
        // Pre-alignment musid-dli LyricDocument JSON: no translation,
        // romanization, end_ms, translations or words keys.
        let legacy = r#"{"format":"Lrc","original":"[00:01.00]Hello","lines":[{"start_ms":1000,"text":"Hello"}]}"#;
        let doc: LyricDocument = serde_json::from_str(legacy).unwrap();
        let data = document_to_lyric_data(&doc);
        assert_eq!(data.lyric, "[00:01.00]Hello");
        assert_eq!(data.tlyric, None);
        assert_eq!(data.lxlyric, None);
        assert_eq!(data.raw_lrc.as_deref(), Some("[00:01.00]Hello"));
        let lines = lyric_lines_from_document(&doc.lines);
        assert_eq!(
            lines,
            vec![LyricLine {
                timestamp: 1000,
                text: "Hello".into(),
                duration: 0,
            }]
        );

        // voicefox-shaped LyricData JSON (as wy/kg/tx producers emit it),
        // plus the missing-keys tolerance of the mirror defaults.
        let host = r#"{"lyric":"[00:12.00]Hi","tlyric":null,"rlyric":null,"lxlyric":null,"raw_lrc":"[00:12.00]Hi"}"#;
        let data: LyricData = serde_json::from_str(host).unwrap();
        let doc = lyric_data_to_document(&data, LyricFormat::Lrc).unwrap();
        assert_eq!(doc.original, "[00:12.00]Hi");
        assert_eq!(doc.lines.len(), 1);
        assert_eq!(doc.lines[0].text, "Hi");

        let minimal: LyricData = serde_json::from_str(r#"{"lyric":""}"#).unwrap();
        assert_eq!(minimal, LyricData::default());
    }

    #[test]
    fn mirror_serde_round_trip_preserves_every_field() {
        let data = LyricData {
            lyric: "[00:12.00]Hello".into(),
            tlyric: Some("t".into()),
            rlyric: Some("r".into()),
            lxlyric: Some(YRC.into()),
            raw_lrc: Some("raw".into()),
        };
        let value = serde_json::to_value(&data).unwrap();
        assert_eq!(serde_json::from_value::<LyricData>(value).unwrap(), data);

        let line = LyricLine {
            timestamp: 12000,
            text: "Hello".into(),
            duration: 3000,
        };
        let value = serde_json::to_value(&line).unwrap();
        assert_eq!(serde_json::from_value::<LyricLine>(value).unwrap(), line);

        let yrc = YrcLine {
            timestamp: 12000,
            words: vec![YrcWord {
                text: "Hel".into(),
                start: 12000,
                duration: 400,
            }],
        };
        let value = serde_json::to_value(&yrc).unwrap();
        assert_eq!(serde_json::from_value::<YrcLine>(value).unwrap(), yrc);
    }

    #[test]
    fn round_trip_stabilizes_original_and_language_fields() {
        let data = LyricData {
            lyric: "[00:12.00]Hello World\n[00:18.00]Outro".into(),
            tlyric: Some("[00:12.00]你好".into()),
            rlyric: None,
            lxlyric: Some(QRC.into()),
            raw_lrc: Some("[00:12.00]Hello World".into()),
        };
        let doc = lyric_data_to_document(&data, LyricFormat::Qrc).unwrap();
        let back = document_to_lyric_data(&doc);
        // Timed original is trusted verbatim; language fields survive.
        assert_eq!(back.lyric, doc.original.trim());
        assert_eq!(back.tlyric, doc.translation.clone());
        assert_eq!(back.rlyric, doc.romanization.clone());
        assert_eq!(back.raw_lrc.as_deref(), Some(doc.original.as_str()));
        // Word timing degrades explicitly on the way back: re-converting
        // without lxlyric yields line text but no words.
        let doc2 = lyric_data_to_document(&back, LyricFormat::Qrc).unwrap();
        assert_eq!(doc2.original, back.lyric);
        assert!(doc2.lines.iter().all(|line| line.words.is_empty()));
        assert_eq!(doc2.translation, doc.translation);
    }

    #[test]
    fn lrc_only_lyric_yields_lines_without_invented_ends() {
        let data = LyricData {
            lyric: "[00:01.00]Hello World\n[00:03.50]Second line".into(),
            ..Default::default()
        };
        let doc = lyric_data_to_document(&data, LyricFormat::Lrc).unwrap();
        assert_eq!(doc.format, LyricFormat::Lrc);
        assert_eq!(doc.lines.len(), 2);
        assert!(doc.lines.iter().all(|line| line.end_ms.is_none()));
        assert!(doc.lines.iter().all(|line| line.words.is_empty()));
        assert_eq!(doc.lines[1].start_ms, 3500);
        // Untimed plain text degrades gracefully: original keeps it verbatim.
        let plain = lyric_data_to_document(
            &LyricData {
                lyric: "just plain words".into(),
                ..Default::default()
            },
            LyricFormat::PlainText,
        )
        .unwrap();
        assert_eq!(plain.original, "just plain words");
        assert!(plain.lines.is_empty());
    }
}
