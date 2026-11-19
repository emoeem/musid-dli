# Bilibili response fixtures

Offline fixtures shaped like real Bilibili API responses, used by the
`#[cfg(test)]` module in `src/source_bilibili.rs` (loaded with `include_str!`).

## Derivation (honesty note)

No raw Bilibili response captures exist in this repository or in the upstream
VoiceFox `bili` module (every bili test there is an inline `json!` literal; a
full-tree grep finds no `wbi_img`/`dash` hit in any recorded JSON). Every
fixture here is therefore **reconstructed or transcribed**, not captured:

- **Upstream inline test vectors kept verbatim** (audit slice
  `source-bilibili`, vectors V6/V8/V11): the DASH 64k/128k/192k audio array
  (`playurl_dash.json`; extended 2026-11-19 with the real-response
  `backupUrl` arrays per item, which the upstream vector itself carries
  none of, so the primary-URL assertions are unchanged; the extension pins
  the structured backup endpoints, MODEL_ALIGNMENT_DESIGN.md §2.4), the
  20-result search page with
  `numResults: 45, pagesize: 20` and `BV{index:010}` ids
  (`search_music_zone.json`), the `<em>`-highlighted title
  (`search_title_em.json`), the two-part video expansion
  (`view_multipage.json`), the single-part video (`view_singlepage.json`)
  and the BV/av/`?p=`/b23.tv reference strings.
- **WBI golden vectors**: `nav_wbi_keys.json` carries the audit's V5
  placeholder URLs (they are shape placeholders, not captured values). The
  signing vectors themselves (audit V2-V4, synthetic keys `a`x32 / `b`x32
  and frozen `wts=1700000000`) live as exact expected query strings in the
  test module; both sides of the migration and this session re-verified all
  three digests independently with `md5sum`, and the 64-entry permutation
  table was asserted to be a complete permutation of 0..63.
- **Reconstructed shapes** (audit vectors V7/V9, no upstream test exists):
  dolby/flac candidate nodes, `backupUrl`/`backup_url` precedence, the
  legacy `durl` fallback, the favorites folder list and the favorites media
  list. Field shapes mirror exactly what the parsers read; all ids, names
  and URLs are placeholder samples chosen to exercise every parser branch.
- **Chart board shapes** (slice `bilibili-charts`, 2026-11-18 audit vectors,
  no upstream leaderboard test exists): `chart_popular.json`,
  `chart_ranking.json`, `chart_music_hot.json` and `chart_recommend.json`
  reconstruct the four board replies from what the upstream parsers read
  (`source/src/bili/leaderboard.rs:161-188, 220-303`): the bvid-required
  rows, the author fallback chains (`owner.name` → flat `author` →
  `author.name` → "哔哩哔哩用户"), the pic/cover alternation, the
  string-vs-numeric duration split per board and the empty-bvid drop case.
  The board catalogue itself and the ranking board's signed parameter set
  are pinned as exact test vectors, not fixtures.
- **Live anchors**: the 2026-11-17 live smoke run recorded in
  `docs/IMPLEMENTATION_STATUS.md` (BV reference expansion, WBI-signed DASH
  audio, QR create + first poll, honest skip when logged out) backs the
  protocol behaviours; anonymous keyword *search* itself is risk-controlled
  (`v_voucher` empty result), so the fixtures pin the signed query
  construction (V2-V4), **not** the server accepting it. A fixture passing
  does not mean the live endpoint was re-verified (`docs/tasks.md` 15.4
  separates fixture tests from live smoke tests).

## Adjudications recorded by this slice

- **High320 announcement divergence**: upstream derives the advertised
  quality set from the *maximum* candidate bandwidth across all DASH
  candidates (`url.rs:50-54`); musid-dli reports the *selected* stream's
  actual bandwidth (>160000 to High). musid-dli's `StreamInfo` has no
  advertised-qualities field, so the upstream announcement surface does not
  exist here; the selected-stream rule (audit fact 8, `url.rs:122-127`) is
  what the fixtures pin. Recorded as a deliberate non-port, not a bug.
- **Favorites pagination cap**: upstream returns an error when a favorites
  listing exceeds `MAX_PAGES` (100 pages, roughly 2000 items,
  `playlist.rs:118-123`); musid-dli previously stopped silently, hiding the
  truncation. This slice ports the upstream error so the limit stays honest.
- **`durl` fallback**: upstream falls back to `data.durl[0].url` when the
  playurl response carries no DASH audio (legacy FLV/MP4 container,
  `url.rs:132-141`); musid-dli previously answered `NotFound`. This slice
  ports the fallback (`playurl_durl_fallback.json`).

## Protocol gotchas pinned by these fixtures

- `nav_wbi_keys.json`: the nav endpoint answers `code: -101` for anonymous
  callers but `data.wbi_img` still carries the rotating keys; only a missing
  key block is fatal. Keys live in the URL file name (between the last `/`
  and the first `.`). `wts` and the keys are injected by the tests; the
  signing itself is time-sensitive and cannot be replayed from a fixture.
- `search_title_em.json`: search titles carry `<em>` highlights that must be
  stripped; the video doubles as the album; `cid`/`aid` are always numeric
  on this endpoint; `duration` is a `"mm:ss"` / `"hh:mm:ss"` / `"ss"` string.
- `view_nopages_fallback.json`: when `data.pages` is missing the parser
  falls back to the top-level `data.cid`; with pages present, multi-part
  videos expand to `{bvid}-p{n}` ids while single-part videos keep the video
  title as the track name.
- `fav_resources.json`: the favorites endpoint returns `cid` as a JSON
  string *or* a number, and both forms are accepted (unlike search, where it
  is always numeric); `duration` is integer seconds; `has_more: false` is
  the pagination terminator.
- `playurl_dash_backup.json`: stream URLs prefer `baseUrl`/`base_url`, then
  the first backup in `backupUrl` (camelCase wins over snake_case when both
  appear).
- `search_risk_control.json`: `code -412` (and `-352`) are rewritten to an
  explicit risk-control error instead of surfacing the server message, so
  the server-side rejection stays honest and is never disguised as success.
- `chart_*.json`: the four board rows parse per board, and the split is
  deliberate: popular/ranking rows accept `"h:mm:ss"`/`"mm:ss"`/`"ss"`
  string durations *and* numeric seconds, while music-hot and recommend
  rows are numeric seconds only (a string duration parses to none there).
  music-hot carries no owner object and keeps an empty author verbatim
  (upstream `unwrap_or_default`, no placeholder), unlike the other boards.
  The recommend reply never errors in code: a non-zero code, missing
  `data.archives`, or zero parsable rows silently falls back to the
  popular board upstream (leaderboard.rs:106-123), a product decision the
  port keeps. No chart endpoint was live-verified in this slice; anonymous
  search on the same host is already risk-controlled, so the unsigned
  board endpoints may behave the same until a live smoke runs.
