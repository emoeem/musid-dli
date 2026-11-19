use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LyricFormat {
    Lrc,
    /// KRC-family karaoke syntax label, not a wire provenance: decrypted
    /// kugou KRC and kuwo lrcx decrypt to the same `[ms,dur]<rel,dur>`
    /// syllable shape (`LyricFormat` has no lx/lrcx variant), so kuwo labels
    /// its word documents `Krc` too (source_kuwo.rs `lyric_document`). The
    /// label is a syntax hint only — karaoke availability is
    /// [`LyricDocument::has_word_timing`], never the format.
    Krc,
    Mrc,
    Qrc,
    Yrc,
    Ttml,
    Spotify,
    Musixmatch,
    PlainText,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricWord {
    pub text: String,
    pub start_ms: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    pub start_ms: u64,
    #[serde(default)]
    pub end_ms: Option<u64>,
    pub text: String,
    #[serde(default)]
    pub translations: BTreeMap<String, String>,
    #[serde(default)]
    pub romanization: Option<String>,
    #[serde(default)]
    pub words: Vec<LyricWord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricDocument {
    pub format: LyricFormat,
    pub original: String,
    #[serde(default)]
    pub translation: Option<String>,
    #[serde(default)]
    pub romanization: Option<String>,
    #[serde(default)]
    pub lines: Vec<LyricLine>,
}

/// Search candidate returned by an optional online lyrics backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricSearchResult {
    pub provider: String,
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub match_score: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(start_ms: u64, text: &str) -> LyricLine {
        LyricLine {
            start_ms,
            end_ms: None,
            text: text.to_owned(),
            translations: BTreeMap::new(),
            romanization: None,
            words: Vec::new(),
        }
    }

    fn document(lines: Vec<LyricLine>) -> LyricDocument {
        LyricDocument {
            format: LyricFormat::Lrc,
            original: String::new(),
            translation: None,
            romanization: None,
            lines,
        }
    }

    /// M6 §7.2 (R11): a document-level tlyric payload aligns into the line
    /// view within the tolerance window; line-level data wins when present.
    #[test]
    fn layered_lines_align_document_translation_within_tolerance() {
        let original = document(vec![
            {
                let mut line = line(1_000, "Hello World");
                line.translations.insert("zh".into(), "行级翻译".into());
                line
            },
            line(6_000, "Second Line"),
            line(11_500, "Third Line"),
        ]);
        let translation = document(vec![
            line(1_000, "document one"),
            line(6_000, "document two"),
            line(12_000, "document three"),
        ]);
        let view = original.layered_lines(Some(&translation), None, 500);
        assert_eq!(view.len(), 3);
        // Line-level map wins over the document alignment.
        assert_eq!(
            view[0].translation,
            Some(MergedTranslation {
                language: Some("zh".into()),
                text: "行级翻译".into(),
            })
        );
        assert_eq!(view[0].translation_source, Some(LyricLayer::Line));
        // Within tolerance: 6_000 vs 6_000.
        assert_eq!(
            view[1].translation,
            Some(MergedTranslation {
                language: None,
                text: "document two".into(),
            })
        );
        assert_eq!(view[1].translation_source, Some(LyricLayer::Document));
        // 11_500 vs 12_000: 500 ms diff is exactly the tolerance edge.
        assert_eq!(
            view[2].translation.as_ref().map(|t| t.text.as_str()),
            Some("document three")
        );
        // Far outside tolerance aligns to nothing: line 0 keeps its
        // producer-supplied line-level map (source stays Line), and line 1
        // (no line-level data) stays empty-handed.
        let far = document(vec![line(60_000, "way off")]);
        let view = original.layered_lines(Some(&far), None, 500);
        assert_eq!(view[0].translation_source, Some(LyricLayer::Line));
        assert!(view[1].translation.is_none());
        assert_eq!(view[1].translation_source, None);
    }

    #[test]
    fn layered_lines_merge_romanization_and_blank_texts_are_absent() {
        let mut original = document(vec![line(1_000, "Hello")]);
        original.lines[0].romanization = Some("harō".into());
        let romanization = document(vec![
            line(1_000, "  "), // blank aligned text is absence, not content
            line(2_000, "haro"),
        ]);
        let view = original.layered_lines(None, Some(&romanization), 500);
        assert_eq!(view[0].romanization.as_deref(), Some("harō"));
        assert_eq!(view[0].romanization_source, Some(LyricLayer::Line));
        // No line-level romanization: the document layer fills in.
        let original_no_line = document(vec![line(2_100, "Hello")]);
        let view = original_no_line.layered_lines(None, Some(&romanization), 500);
        assert_eq!(view[0].romanization.as_deref(), Some("haro"));
        assert_eq!(view[0].romanization_source, Some(LyricLayer::Document));
        // No layers at all: the view stays empty-handed, never fabricated.
        let none_view = original_no_line.layered_lines(None, None, 500);
        assert_eq!(none_view[0].romanization, None);
    }

    /// M6 §7.2: the 逐字 marker flips only when words exist.
    #[test]
    fn has_word_timing_reflects_line_words() {
        let mut document = document(vec![line(1_000, "Hello")]);
        assert!(!document.has_word_timing());
        document.lines[0].words.push(LyricWord {
            text: "Hel".into(),
            start_ms: 1_000,
            duration_ms: 300,
        });
        assert!(document.has_word_timing());
    }

    /// M6 §7.4: the format label is a syntax hint, not the capability
    /// contract. Kuwo labels its decrypted lrcx `LyricFormat::Krc` (the
    /// KRC-family syntax; there is no lx/lrcx variant), so a host reads the
    /// 逐字 marker `has_word_timing()` for karaoke availability, whatever
    /// the label claims.
    #[test]
    fn format_label_is_not_the_word_timing_contract() {
        let mut document = document(vec![line(1_000, "Hello")]);
        document.format = LyricFormat::Krc;
        assert!(
            !document.has_word_timing(),
            "a Krc-family label without word payloads is line-timed only"
        );
        document.lines[0].words.push(LyricWord {
            text: "He".into(),
            start_ms: 1_000,
            duration_ms: 300,
        });
        assert!(document.has_word_timing());
        // And an Lrc-labelled document carrying syllable payloads is
        // karaoke too: the label never gates the marker.
        document.format = LyricFormat::Lrc;
        assert!(document.has_word_timing());
    }

    /// M6 §7.3 (R11): materializing keeps line-level data, fills only empty
    /// lines, skips blanks, and preserves the document-level raw slots.
    #[test]
    fn materialize_layers_fills_line_maps_and_keeps_document_slots() {
        let mut original = document(vec![
            {
                let mut line = line(1_000, "Hello");
                line.translations.insert("zh".into(), "行级翻译".into());
                line
            },
            line(6_000, "Second"),
        ]);
        let original_doc_translation = "[00:01.00]文档原始串".to_owned();
        original.translation = Some(original_doc_translation.clone());
        let translation = document(vec![
            line(1_000, "ignored for line 1"),
            line(6_050, "文档对齐二"),
        ]);
        original.materialize_layers(Some(&translation), None, 500);
        // Line 1 keeps its producer-supplied map; document text not merged.
        assert_eq!(
            original.lines[0].translations.get("zh").map(String::as_str),
            Some("行级翻译")
        );
        assert_eq!(original.lines[0].translations.len(), 1);
        // Line 2 gains the aligned document translation under the
        // unspecified-language key.
        assert_eq!(
            original.lines[1]
                .translations
                .get(UNSPECIFIED_LANGUAGE)
                .map(String::as_str),
            Some("文档对齐二")
        );
        // The document-level raw payload stays verbatim.
        assert_eq!(
            original.translation.as_deref(),
            Some(original_doc_translation.as_str())
        );
        // Review P2: provenance survives materialization. The line-level
        // map carries the merged data, but the view still reports the true
        // origin through the UNSPECIFIED_LANGUAGE marker: producer-supplied
        // line data stays Line, document-aligned data reads Document.
        let view = original.layered_lines(None, None, 500);
        assert_eq!(view[0].translation_source, Some(LyricLayer::Line));
        assert_eq!(view[1].translation_source, Some(LyricLayer::Document));
        assert_eq!(
            view[1]
                .translation
                .as_ref()
                .and_then(|t| t.language.as_deref()),
            None
        );
        // Round-2 review, romanization provenance is covered separately by
        // romanization_provenance_survives_materialization_by_comparison
        // (the scalar layer needs content comparison instead of a marker
        // key).
    }

    /// Review round 2, issue 4: romanization provenance survives the
    /// materialized path by content comparison (the scalar layer has no
    /// language key to carry a marker): a materialized line value,
    /// identical to the nearest document-layer text, reports Document; a
    /// producer value that differs reports Line.
    #[test]
    fn romanization_provenance_survives_materialization_by_comparison() {
        let mut original = document(vec![line(1_000, "Hello"), line(6_000, "Second")]);
        original.lines[1].romanization = Some("producer haro".into());
        let romanization = document(vec![line(1_000, "haro"), line(6_000, "haro two")]);
        original.materialize_layers(None, Some(&romanization), 500);
        let view = original.layered_lines(None, Some(&romanization), 500);
        // Line 1: the materialized value is the document layer's own text.
        assert_eq!(view[0].romanization.as_deref(), Some("haro"));
        assert_eq!(view[0].romanization_source, Some(LyricLayer::Document));
        // Line 2: the producer value differs from the document text.
        assert_eq!(view[1].romanization.as_deref(), Some("producer haro"));
        assert_eq!(view[1].romanization_source, Some(LyricLayer::Line));
        // No document layer at all: any line value is producer-supplied.
        let plain_view = original.layered_lines(None, None, 500);
        assert_eq!(plain_view[0].romanization_source, Some(LyricLayer::Line));
    }

    /// Review P2: nothing is dropped when a line carries several
    /// translations; `translation` is the deterministic alphabetical-first
    /// pick and the full map rides `LayeredLyricLine::translations`.
    #[test]
    fn layered_lines_keep_the_full_multi_language_map() {
        let mut original = document(vec![line(1_000, "Hello")]);
        original.lines[0]
            .translations
            .insert("zh".into(), "中文".into());
        original.lines[0]
            .translations
            .insert("en".into(), "hello".into());
        let view = original.layered_lines(None, None, 500);
        assert_eq!(view[0].translations.len(), 2);
        assert_eq!(
            view[0].translations.get("zh").map(String::as_str),
            Some("中文")
        );
        assert_eq!(
            view[0].translations.get("en").map(String::as_str),
            Some("hello")
        );
        // Deterministic primary pick: "" < "en" < "zh" in key order; no
        // UNSPECIFIED_LANGUAGE entry here, so the first real language wins
        // and reports Line provenance.
        assert_eq!(
            view[0].translation,
            Some(MergedTranslation {
                language: Some("en".into()),
                text: "hello".into(),
            })
        );
        assert_eq!(view[0].translation_source, Some(LyricLayer::Line));
    }

    /// M6 §7.2: a legacy pre-M6 document (serde default fields) has no
    /// layers and no timing; the accessors answer accordingly.
    #[test]
    fn legacy_document_shape_answers_accessors_without_layers() {
        let legacy: LyricDocument = serde_json::from_str(
            r#"{"format":"Lrc","original":"[00:01.00]Hello","lines":[{"start_ms":1000,"text":"Hello"}]}"#,
        )
        .expect("legacy json loads");
        assert!(!legacy.has_word_timing());
        let view = legacy.layered_lines(None, None, 500);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].translation, None);
        assert_eq!(view[0].romanization, None);
        assert_eq!(view[0].translation_source, None);
        assert_eq!(view[0].start_ms, 1_000);
    }
}

/// BTreeMap language key used when materializing a document-level payload
/// that carries no language tag (RESOURCE_DESIGN.md §7.3). The document
/// `translation`/`romanization` slots are raw payload strings (what netease/
/// qqmusic producers fill), so the aligned text has no language to name.
pub const UNSPECIFIED_LANGUAGE: &str = "";

/// Where a merged layer value came from (RESOURCE_DESIGN.md §7.2):
/// producer-supplied line-level data always wins over an aligned
/// document-level payload, and the source is recorded so hosts can tell
/// the two apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LyricLayer {
    /// From `LyricLine::translations` / `LyricLine::romanization`.
    Line,
    /// Aligned from the document-level payload by `start_ms`.
    Document,
}

/// One merged translation with its (optional) language tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergedTranslation {
    /// Language key of the line-level map entry; `None` when the value was
    /// aligned from a document-level payload (no language on that wire).
    pub language: Option<String>,
    pub text: String,
}

/// One line of the normalized merged view (RESOURCE_DESIGN.md §7.2): the
/// original line plus whichever translation/romanization layer data was
/// available, each annotated with its origin. Purely a view: `LyricDocument`
/// is never mutated by [`LyricDocument::layered_lines`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayeredLyricLine {
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub text: String,
    pub words: Vec<LyricWord>,
    /// The line-level translations map in full. `translation` is the
    /// deterministic primary pick (first entry in language-key order);
    /// nothing is dropped when a line carries several languages.
    #[serde(default)]
    pub translations: BTreeMap<String, String>,
    pub translation: Option<MergedTranslation>,
    pub romanization: Option<String>,
    pub translation_source: Option<LyricLayer>,
    pub romanization_source: Option<LyricLayer>,
}

impl LyricDocument {
    /// Whether any line carries word timing (RESOURCE_DESIGN.md §7.2, the
    /// 逐字可用性 marker). Empty `words` on every line means the document is
    /// line-timed only, whatever the `format` claims.
    pub fn has_word_timing(&self) -> bool {
        self.lines.iter().any(|line| !line.words.is_empty())
    }

    /// The normalized merged view over the original lines (RESOURCE_DESIGN.md
    /// §7.2). `translation`/`romanization` are parsed documents of the
    /// document-level payloads (e.g. netease `tlyric`/`rlyric` parsed through
    /// the lyrics facade). Alignment: each original line takes the nearest
    /// layer-line start within `tolerance_ms` (typical 500; ties prefer the
    /// earlier line). Line-level data always wins when both exist, and the
    /// `*_source` fields record which layer supplied each piece.
    ///
    /// Provenance survives the materialized path (RESOURCE_DESIGN.md §7.3):
    /// [`parse_document_layers`](crate::lyrics::parse_document_layers)
    /// materializes document-layer data into the line maps under the
    /// [`UNSPECIFIED_LANGUAGE`] key, and this view reports such an entry as
    /// `LyricLayer::Document` (with `language: None`) so a host can still
    /// tell aligned-document translation from producer-supplied line data.
    /// `translation` picks the first entry in language-key order; the full
    /// map rides [`LayeredLyricLine::translations`]. Never mutates the
    /// document; never concatenates two layer lines (lossy-nearest by
    /// design).
    pub fn layered_lines(
        &self,
        translation: Option<&LyricDocument>,
        romanization: Option<&LyricDocument>,
        tolerance_ms: u64,
    ) -> Vec<LayeredLyricLine> {
        let translation_starts = sorted_layer_starts(translation);
        let romanization_starts = sorted_layer_starts(romanization);
        self.lines
            .iter()
            .map(|line| {
                let line_translation =
                    line.translations
                        .iter()
                        .next()
                        .map(|(language, text)| MergedTranslation {
                            language: Some(language.clone()),
                            text: text.clone(),
                        });
                let (translation, translation_source) = match line_translation {
                    // A line entry under UNSPECIFIED_LANGUAGE is the
                    // materialized form of a document-layer alignment
                    // (materialize_layers only ever writes that key):
                    // report the true origin so provenance survives the
                    // materialized path.
                    Some(value) if value.language.as_deref() == Some(UNSPECIFIED_LANGUAGE) => (
                        Some(MergedTranslation {
                            language: None,
                            text: value.text,
                        }),
                        Some(LyricLayer::Document),
                    ),
                    Some(value) => (Some(value), Some(LyricLayer::Line)),
                    None => nearest_layer_text(
                        &translation_starts,
                        translation,
                        line.start_ms,
                        tolerance_ms,
                    )
                    .map(|text| {
                        (
                            Some(MergedTranslation {
                                language: None,
                                text,
                            }),
                            Some(LyricLayer::Document),
                        )
                    })
                    .unwrap_or((None, None)),
                };
                let (romanization, romanization_source) = match &line.romanization {
                    Some(text) => {
                        // Provenance on the materialized path (RESOURCE_DESIGN.md
                        // §7.3): the romanization layer is a scalar with no
                        // language key to carry a marker, so attribution is
                        // by comparison: a line value identical to the
                        // nearest document-layer text within tolerance is
                        // exactly what materialize_layers writes, and is
                        // reported as Document-origin. A differing value is
                        // producer-supplied. Edge case: a producer line
                        // romanization that coincides byte-for-byte with the
                        // aligned document text is reported as Document too
                        // (indistinguishable by content; documented).
                        match nearest_layer_text(
                            &romanization_starts,
                            romanization,
                            line.start_ms,
                            tolerance_ms,
                        ) {
                            Some(doc_text) if doc_text == text.trim() => {
                                (Some(text.clone()), Some(LyricLayer::Document))
                            }
                            _ => (Some(text.clone()), Some(LyricLayer::Line)),
                        }
                    }
                    None => nearest_layer_text(
                        &romanization_starts,
                        romanization,
                        line.start_ms,
                        tolerance_ms,
                    )
                    .map(|text| (Some(text), Some(LyricLayer::Document)))
                    .unwrap_or((None, None)),
                };
                LayeredLyricLine {
                    start_ms: line.start_ms,
                    end_ms: line.end_ms,
                    text: line.text.clone(),
                    words: line.words.clone(),
                    translations: line.translations.clone(),
                    translation,
                    romanization,
                    translation_source,
                    romanization_source,
                }
            })
            .collect()
    }

    /// Materialize the merged view into this document's line maps
    /// (RESOURCE_DESIGN.md §7.3): aligned document-level translation/
    /// romanization payloads are written into `lines[].translations` (under
    /// [`UNSPECIFIED_LANGUAGE`] when the payload has no language) and
    /// `lines[].romanization`. Line-level data always wins (never
    /// overwritten); blank aligned texts are skipped; the document-level
    /// raw slots (`translation`/`romanization`) are kept verbatim so the
    /// document stays serde-complete. Producers keep filling whichever
    /// layer they fill today; this is the normalization face on top.
    pub fn materialize_layers(
        &mut self,
        translation: Option<&LyricDocument>,
        romanization: Option<&LyricDocument>,
        tolerance_ms: u64,
    ) {
        let translation_starts = sorted_layer_starts(translation);
        let romanization_starts = sorted_layer_starts(romanization);
        for line in &mut self.lines {
            if line.translations.is_empty()
                && let Some(text) = nearest_layer_text(
                    &translation_starts,
                    translation,
                    line.start_ms,
                    tolerance_ms,
                )
            {
                line.translations
                    .insert(UNSPECIFIED_LANGUAGE.to_owned(), text);
            }
            if line.romanization.is_none()
                && let Some(text) = nearest_layer_text(
                    &romanization_starts,
                    romanization,
                    line.start_ms,
                    tolerance_ms,
                )
            {
                line.romanization = Some(text);
            }
        }
    }
}

/// `(start_ms, line index)` pairs of a layer document, sorted for the
/// nearest-line search.
fn sorted_layer_starts(document: Option<&LyricDocument>) -> Vec<(u64, usize)> {
    document
        .map(|document| {
            let mut starts: Vec<(u64, usize)> = document
                .lines
                .iter()
                .enumerate()
                .map(|(index, line)| (line.start_ms, index))
                .collect();
            starts.sort_by_key(|(start, _)| *start);
            starts
        })
        .unwrap_or_default()
}

/// Nearest layer line text within `tolerance_ms` of `start_ms`, skipping
/// blank texts (a blank tlyric line is absence, not an empty translation).
fn nearest_layer_text(
    starts: &[(u64, usize)],
    document: Option<&LyricDocument>,
    start_ms: u64,
    tolerance_ms: u64,
) -> Option<String> {
    let document = document?;
    if starts.is_empty() {
        return None;
    }
    let position = starts
        .binary_search_by(|(start, _)| start.cmp(&start_ms))
        .unwrap_or_else(|insertion| insertion);
    let mut best: Option<(u64, usize)> = None;
    let window_start = position.saturating_sub(1);
    let window_end = position.min(starts.len() - 1);
    for &(start, line_index) in &starts[window_start..=window_end] {
        let diff = start.abs_diff(start_ms);
        if diff <= tolerance_ms && best.is_none_or(|(best_diff, _)| diff < best_diff) {
            best = Some((diff, line_index));
        }
    }
    best.and_then(|(_, line_index)| {
        let text = document.lines[line_index].text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    })
}
