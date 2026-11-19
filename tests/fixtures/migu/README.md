# Migu offline fixtures

Offline fixtures and vectors for the Migu provider, used by the
`#[cfg(test)]` module in `src/source_migu.rs` (loaded with `include_str!`).

## Derivation (honesty note)

The upstream VoiceFox `mg` module ships **no** fixture files: the only
fixture in the whole voicefox repository is
`source/tests/fixtures/user_api_v3.js` (an LX-JS user-API script), and
`crypto.rs`, `lyric.rs`, `url.rs` and `parse.rs` (the link-target goldens,
`parse.rs:159-206`) have inline `#[cfg(test)]` modules.
`search.rs`, `playlist.rs`, `song.rs` and `leaderboard.rs` have none, so
there are no upstream goldens for the signature, the search/playlist
response shapes or pagination. Nothing in `tests/fixtures/migu/` is a
captured HTTP response; every JSON file is either an upstream inline vector
transplanted verbatim, a formula-derived vector, or a reconstructed shape
fixture over the field contracts read from the upstream source:

- **`mg_sign.json`**: **formula-derived only**. Upstream `mg/crypto.rs:26-41`
  defines the concatenation (keyword + `6cdc72a439cef99a3418d2a78aa28c73` +
  `yyapp2d16148780a1dcc7408e06336b98cfd50` + device id + timestamp) hashed
  with plain MD5 but ships no golden. The two signs were recomputed with an
  independent Python transcription before pinning (both digests contain
  bytes < 0x10, pinning the per-byte zero-padded lower-hex output). The
  constants at the top of the file double as a pin on the source literals
  (`SEARCH_SIGN_MD5`, `SEARCH_DEVICE_ID`).
- **`mrc_tea_hex.json`**: **formula-derived** XXTEA containers over the
  64-bit-word variant of `mg/crypto.rs:9-23,236-266` (delta 0x9E3779B9,
  rounds 6+52/n, 9-word key; only key slots [0..3] are ever indexed because
  `(index & 3) ^ keyIndex` is 2-bit on both sides). Both hex payloads were
  encrypted from their plaintexts with an independent Python transcription
  and re-decrypted; the upstream `encrypt_fixture`-style round-trip test
  alone would not catch a shared encrypt/decrypt bias, so the absolute
  ciphertexts are the point of these vectors. No real online MRC body was
  captured: if live containers use a different word length or byte order,
  these are the vectors to revisit.
- **`mrc_parse.json`**: `single_line` is the upstream `mg/lyric.rs:184-189`
  inline golden verbatim (markers precede their syllable, lx word offsets
  are relative to the line head, timestamps are `MM:SS.mmm`);
  `two_lines` extends it with musid-dli's recorded two-line case;
  `no_timed_lines` pins the `None` that triggers the LRC fallback.
- **`strategy_decrypt.json`**: **formula-derived** from
  `mg/url.rs:179-194` (magic `\xab\xcd\x01`, per-byte
  `(byte + seed - key[i % 32])` over the ASCII key
  `Jk8qzuePiJ1qE3mDYhLQ3T73DtDoAhLP`). Upstream has zero tests for this
  decoder; the hex was produced by encrypting the compact-JSON payload
  (seed 0x42) with an independent Python transcription and verified by
  decrypting it back. musid-dli additionally sends the mobile playing-page
  Referer and a `channel: 0146921` header on this endpoint (live-verified
  2026-11-17, absent upstream); request headers are not fixture-pinnable,
  only the response decoder is.
- **`stream_tiers.json`**: the `sq_variant_pairing` row and the
  `size_text_cases` / `quality_for_size` / `resource_id_cases` /
  `preview_cases` vectors are the upstream `mg/url.rs:527-633` inline test
  vectors verbatim (plus one `trail_audio` path case and the iOS-size
  quality mapping, per the same contract). The declared sizes are
  audit-recorded samples (anonymous PQ was observed live at 4317311 bytes
  on 2026-11-17, but sizes vary per track, so no absolute size is asserted
  as an online invariant).
- **`search_response.json`**: **reconstructed shape** over
  `mg/search.rs:52-106`: `code` must be `"000000"` (errors surface
  `info`), `totalCount` may be a number or a string, `resultList` is a
  two-dimensional array whose groups are merged and deduped on
  `songId:copyrightId`, rows without a song id or a title are dropped, and
  `has_more = page * limit < total`. `missing_result_list` pins musid-dli's
  behaviour of keeping the parsed total on a missing/non-array
  `resultList`; upstream instead returns total 0 in that branch
  (`search.rs:73-80`), a deliberate, documented port difference. The
  `playlist_search` case (2026-11-20 M3 s4 slice) is a **reconstructed
  shape** over the `mg/playlist.rs:134-156` searched-playlist contract:
  play_count from numeric-or-string `playNum`, creator from the first
  string among `userName`/`ownerName` with the upstream find-then-filter
  quirk (a blank first hit voids the chain, mg/playlist.rs:149-153), the
  canonical web link, and the empty-id row drop.
- **`playlist_pages.json`**: **reconstructed shape** over
  `mg/playlist.rs:28-52` + `mg/song.rs:83-139`: `code == "000000"`, rows in
  `data.songList` go through the shared song parser (singerList, the
  顿号-separated `singer` fallback, numeric or `MM:SS` durations),
  `totalCount` may be a number or a string, and the metadata title falls
  back name -> title -> playlistName. The three `page_113_*` pages are the
  live-recorded shape of a 113-track playlist (50 + 50 + 13 rows; 113
  tracks were fetched live on 2026-11-17 through this endpoint) with
  placeholder catalog values. Pagination stop rules pinned in the tests: an
  empty page, everything fetched, or musid-dli's defensive 40-page cap.
  Upstream differs in two documented ways: it stops on
  `pageNo * 50 >= total` (equivalent only when every page is full) and
  treats a missing `totalCount` as the current page's row count (stopping
  after the first non-empty page), while musid-dli keeps paginating until an
  empty page or the cap. Upstream also errors on a missing `songList`;
  musid-dli treats it as an empty page (`missing_songlist` pins the port
  behaviour). The 2026-11-20 M3 s4 slice added the get_detail_with_meta
  header chains (mg/playlist.rs:82-91): `page_113_1` carries
  `userName`/`imgUrl` (first chain keys), `title_fallback_playlist_name`
  carries `ownerName`/`picUrl` (second keys), and `creator_blank_first_hit`
  pins the find-then-filter quirk (a whitespace `userName` voids the whole
  creator chain and an empty `picUrl` voids the cover chain without falling
  through to the next key).
- **`chart_rank_index.json`**: `synthetic_desc_inheritance` is a **contract
  vector derived from `mg/leaderboard.rs:87-122`** (boards = objects with a
  string `rankId` and a string `rankName` at any depth, deduped by id, with
  the update note inherited from the nearest ancestor's non-empty `desc`).
  `live_shape_2026_11_18` is a **reconstructed trim** of the live capture
  taken anonymously on 2026-11-18 (GET
  `https://app.c.nf.migu.cn/pc/bmw/rank/rank-index/v1.0` with Referer
  `https://app.c.nf.migu.cn/`, `channel: 0146921` and the Nexus-6 UA; HTTP
  200, code `000000`, 31820 bytes): the two style groups (咪咕特色榜 flat,
  咪咕尖叫榜 carrying `desc`) keep their real ids, names, imageUrls and
  ordering; 9 boards total, of which the four 尖叫榜 entries inherit
  "每小时更新", and one real song-preview object stays to pin the
  no-`rankId` skip. `error_gate` / `empty_contents` pin the `info` error
  text, the fixed fallback and the empty-catalogue error. `imageUrl` and
  `data.header` are ignored by the parser and are not pinned behaviour.
  Since 2026-11-20 (M3 s4 slice) the tests also assert that the same desc
  chain fills `Chart.updated_description` (the upstream
  `LeaderboardInfo.update` slot, mg/leaderboard.rs:111); the fixture data
  itself is unchanged.
- **`chart_board.json`**: entry 1 is a **trimmed verbatim slice** of the
  live capture for `columnId=83048887` (全网热歌榜, same anonymous headers;
  HTTP 200, code `000000`, 341485 bytes, `contentsCount` 50 with 50 dict
  `objectInfo`s, none missing `songId` or empty `songName`): string songId,
  no `duration` (only `length` hh:mm:ss), `audioFormats` absent with
  `newRateFormats` carrying PQ/HQ/SQ/ZQ, `img1/2/3` absent with the cover
  from `albumImgs[0].img`, `lrcUrl`/`mrcUrl` present and an empty `trcUrl`.
  Entries 2-6 are **reconstructed** around that shape: entry 3 pins the
  array-order-over-`relationSort` trap (the live board carries
  relationSort 70,69,68,...), entry 4 lacks `songId` (ranks renumber over
  the retained items, like the upstream `filter_map`), entry 6 carries
  `objectInfo` as a JSON string which is silently dropped (upstream has no
  handling for it either). The musid-dli `ChartProvider` trait returns the
  whole board, so upstream's client-side skip/take pagination and the
  `contentsCount` total have no target in this port; the API delivers the
  full board per call (live: 50 == 50).
- **`album_songs.json`**: `ok` is a **trimmed verbatim slice** of the live
  capture taken anonymously on 2026-11-18 (GET
  `https://app.c.nf.migu.cn/MIGUM2.0/v1.0/content/queryAlbumSong?albumId=10927&pageNo=1&pageSize=100`
  with the playlist header set: Referer `https://m.music.migu.cn/`,
  `channel: 0146921`, iPhone UA; HTTP 200, 87885 bytes, code `000000`, 12
  rows, `totalCount` int 12). Row 0 keeps the parser-consumed and trap keys
  verbatim: string songId; the live rows carry **no**
  `duration`/`audioFormats`/`singerList` keys at all (the audit recorded
  them as null/empty; absent and null parse identically through
  `serde_json::Value::Null`), the duration comes from `length` hh:mm:ss,
  qualities live in `newRateFormats` (PQ/HQ/SQ on row 0), the cover exists
  only in `albumImgs[0].img` (`img1/2/3` absent, `webpImg` empty), artists
  carries `{id,name,nameSpelling}`, `mrcUrl`/`trcUrl` are empty strings and
  no lowercase `mrcurl` key exists, `copyrightId` is alphanumeric
  (`6005751F411`), `contentId` numeric (`600910000003611567`). Rows 1-11
  are trimmed to identity/length/track keys with their real catalog values;
  row 1 keeps its verbatim `newRateFormats` so ZQ (HiRes) is pinned on a
  later row. `string_total_count` is a **reconstructed** variant
  (`playlist_pages.json` already pins string totals for the shared
  wrapper). `empty_data` is the **verbatim 49-byte body** the live endpoint
  returns for an invalid albumId (`999999999999`, HTTP 200, code still
  `000000`) and for `pageNo=2` alike: the code gate cannot catch it, so
  NotFound must be derived from the missing/empty `songList` (upstream
  `mg/parse.rs:125-133` does exactly that). `code_error` is a
  **reconstructed** defensive gate case: musid-dli's sibling parsers all
  gate on code `000000` while upstream fetch_album has no code gate, a
  deliberate documented port difference. There is no album-detail endpoint
  upstream (`grep` mg/ hits parse.rs:122 only), so title/artists/cover are
  derived from the first row and the track count from `totalCount`.
- **`link_targets.json`**: the first five `positive` cases and the first two
  `rejected` cases are the upstream `mg/parse.rs:163-205` inline test vectors
  verbatim; the rest are audit probes confirmed against a verbatim
  transcription of `link_target` (`cargo test --offline` in a standalone
  harness, 10 passed / 0 failed). They pin the routing order (the
  `playlistId=`/`musicListId=`/`playlist|songlist` loop runs before the album
  patterns), the v3/v5 album-path alternation, the bare-numeric → Playlist
  rule (a bare song contentId is never inferred as a song) and the unanchored
  captures (no scheme/host validation, so a `music.migu.cn` fragment inside a
  foreign URL matches; a documented inherited gap). Downstream
  `mg_link_target` maps "no match" to `Error::InvalidInput` where upstream
  returns `None`, so the `rejected` cases assert that error.

A fixture passing here says nothing about live endpoints
(`docs/tasks.md` §15.4); live behaviour is covered by the Migu smoke.
The listenSong.do anonymous 302 fallback is documented-parity only: the
live endpoint answers "PE参数格式错误" for anonymous calls (2026-11-17), and
no fixture can pin a working anonymous call.

## Files and the gotchas they pin

- `mg_sign.json`: sign = lower-hex MD5 over keyword + two fixed salts +
  device id + the millisecond timestamp echoed in the `timestamp` header;
  the digest input is UTF-8 of the whole concatenation and every output
  byte prints as exactly two hex digits.
- `mrc_tea_hex.json`: hex containers (at least 32 hex chars, multiple of 16
  after whitespace removal) decrypt as 64-bit-word XXTEA; words unpack
  little-endian into UTF-16 code units and trailing NULs are trimmed.
- `mrc_parse.json`: line header `[\d+,\d+]`, word markers `(start,dur)` are
  absolute milliseconds; the LRC form strips markers, the lx form rewrites
  word times as offsets from the line head; a body with no timed line
  returns `None` so the LRC fallback can take over.
- `strategy_decrypt.json`: responses starting `\xab\xcd\x01` are
  byte-deobfuscated with the seed byte before JSON parsing; without the
  prefix the body is plain JSON.
- `stream_tiers.json`: url/size pairing must stay per-variant (SQ flac
  31529675 vs m4a 31931278), sizes accept plain bytes or `3.5MB`-style
  text, the served size maps back to the declared tier (anonymous PQ only),
  ZQ24 matches Flac24 but not Flac, resourceinfo.do needs the contentId
  (copyrightId is only a fallback for older data), and preview/audition/
  trail_audio-marked URLs are rejected as incomplete audio.
- `search_response.json`: two-dimensional group merge with
  `songId:copyrightId` dedup, malformed rows dropped, numeric-or-string
  totals, `has_more` from the page arithmetic, `code != "000000"` errors,
  and the `playlist_search` case pinning the searched-playlist
  play_count/creator/link fills plus the find-then-filter creator quirk.
- `playlist_pages.json`: shared song parser on playlist rows, string or
  numeric `totalCount`, title fallback chain, 50-per-page window with the
  boundary walk of a 113-track playlist (3 pages), the stop rules above,
  and the creator/cover header chains with the blank-first-hit quirk.
- `chart_rank_index.json` / `chart_board.json`: recursive catalogue walk
  with desc inheritance and rankId dedup, `info`-text error gates plus the
  fixed fallbacks, the empty-catalogue and missing-`contents` errors, and
  board songs parsed through the shared song parser with ranks 1..N in
  array order (relationSort ignored, dropped entries shift later ranks);
  the catalogue tests also pin the desc chain riding
  `Chart.updated_description`.
- `album_songs.json`: album rows through the shared song parser with the
  metadata derived from the first row plus `totalCount` (numeric or
  string); the empty-`data` shape (invalid albumId, `pageNo > 1`) stays
  code `000000` so it must surface `NotFound` from the empty songList, not
  from the code gate; the defensive code gate is musid-dli convention.
- `link_targets.json`: the three link shapes (song/playlist/album), the
  routing order between the parameter and path patterns, the
  bare-numeric → Playlist rule, and the unanchored captures (see the
  derivation above for the gaps that stay inherited).

## Not covered (would target code musid-dli does not implement)

- The playlist-square recommend payload, and login/user playlists (upstream
  has no implementation either). The album endpoint is covered above since
  the 2026-11-18 album slice; the chart endpoints are covered too, and the
  board-client skip/take pagination they once fed has no target in the
  musid-dli `ChartProvider` trait and stays unpinned.
