# CUE adapter fixtures

Offline fixture batch for the local CUE container adapter (`src/formats_cue.rs`).
Every vector comes from the upstream VoiceFox audit (voicefox
`source/src/local/scanner.rs` CUE reader, `cue-rw` 0.2.1 grammar) and was
re-verified in the audit's standalone vector harness.

CUE sheets are text; the audio files they reference are **not** committed.
Tests synthesize minimal PCM WAV stubs (1000 Hz, 16-bit mono) at runtime with
byte-exact lengths so `lofty` reports the durations the audit vectors used as
stub values. Upstream vectors referenced `.flac` names; the FILE names here use
`.wav` so the referenced name matches the synthesized stub. All timestamps,
titles, escapes and fallback semantics are identical to the audited vectors.

| Fixture | Vector | Exercise |
|---|---|---|
| `utf8_basic.cue` | V1 UTF8_SHEET | crate path, 3 tracks, `\"`/`\\` escapes, performer fallback, positional numbering; stub audio 400000 ms → start_ms 0/62493/210986, dur_ms 62493/148493/189013 |
| `gbk_sheet.cue` | V2 GBK_SHEET | committed as raw GB18030 bytes; UTF-8 read fails → GB18030 fallback; stub audio 300000 ms → track 2 start 120493 ms, dur 179506 ms |
| `legacy_sheet.cue` | V3 LEGACY_SHEET | cue-rw rejects (no global TITLE/PERFORMER, no 2-space indent) → legacy line parser; TRACK token kept verbatim, INDEX 00 ignored; stub audio 600000 ms |
| `multi_file.cue` | V6 MULTI_FILE_SHEET | two FILEs, per-track source mapping, cross-FILE end = file duration; stubs side-a 400000 ms / side-b 200000 ms |
| `catalog_ok.cue` | V7 | 13-digit CATALOG accepted by cue-rw → positional numbering (TRACK 07 token becomes "01") |
| `catalog_short.cue` | V7 | 5-digit CATALOG rejected by cue-rw → legacy parser keeps TRACK 07 verbatim |
| `index_seconds_out_of_range.cue` | V4b | INDEX `01:99:00` accepted by cue-rw (<100) but rejected by the strict CUE time parser → whole sheet errors, no legacy fallback. Every track carries a TITLE: cue-rw rejects sheets with untitled tracks outright (probe-verified, "missing title entry in cue track"), which would route the sheet to the legacy parser instead |
| `missing_audio.cue` | V8 | referenced audio absent → whole sheet fails ("读取 CUE 音频失败"), no partial splitting |

The garbage-byte (V2b) and UTF-16LE BOM (V9) vectors are byte-level encodings
generated inside the tests themselves rather than committed files.
