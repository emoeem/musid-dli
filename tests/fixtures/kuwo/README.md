# Kuwo offline fixtures

Offline fixtures and vectors for the Kuwo provider, used by the
`#[cfg(test)]` module in `src/source_kuwo.rs` (loaded with `include_str!`
and `include_bytes!`).

## Derivation (honesty note)

The upstream VoiceFox `kw` module ships **no** fixture files: the only
fixture in the whole voicefox repository is
`source/tests/fixtures/user_api_v3.js` (an LX-JS user-API script), and every
kw test is an inline `#[cfg(test)]` literal (`parse.rs`, `playlist.rs`,
`session.rs`, `url.rs`; `search.rs`, `leaderboard.rs`, `lyric.rs` and
`crypto.rs` have no test module at all). Nothing in `tests/fixtures/kuwo/`
is a captured HTTP response:

- **`web_secret.json`**: the `kw/session.rs:157-158` golden vectors verbatim
  (`"abc123"`/12345678 and `"Hm_X"`/1), plus the `session.rs:143-151` length
  invariant. Both ciphertexts were reproduced independently in this slice
  with a Python reimplementation of the LCG-XOR formula before pinning.
- **`lyric_params.json`**: **formula-derived only**. Upstream ships no golden
  ciphertext (`kw/lyric.rs` and `kw/crypto.rs` have zero tests), so the two
  base64 payloads were recomputed from the `yeelion` XOR formula
  (`kw/crypto.rs:8-13` + `kw/lyric.rs:19-22`) and round-trip verified with
  Python. The plain case pins the plain lyric request; the `lrcx` case pins
  the word-lyric request (params + `&lrcx=1`, `kw/lyric.rs:29-35`), sent by
  `fetch_raw_lyric(_, true)`.
- **`lyric_framed.bin` / `lyric_unframed.bin`**: deterministic bodies built
  from the recorded framing (`kw/lyric.rs:5-7,58-89`): the sample payload
  encoded GB18030, zlib-compressed, with and without the
  `tp=content\r\n\r\n` header frame. The GB18030 bytes are not valid UTF-8,
  so the test fails if the decode is switched to UTF-8. Not a payload
  captured from newlyric.kuwo.cn.
- **`lyric_lrcx_body.bin`**: the 2026-11-18 slice's **formula-derived**
  end-to-end word-lyric body vector (still no live capture exists of
  newlyric.kuwo.cn, plain or lrcx): a `tp=content\r\n\r\n` frame followed by
  a zlib stream whose inflation is a canonical base64 payload followed by
  four junk bytes: two non-ASCII (`0x9f 0x8e`) plus two ASCII
  (`\x00\x7f`). Decoding per the upstream chain (`kw/lyric.rs:58-90`: frame
  skip → zlib → ASCII filter → base64 → `yeelion` XOR → GB18030) reproduces
  exactly `[21456,2914]<0,146,0>牵<146,135,0>着<281,205,0>你` /
  `[24370,2810]<0,300,0>走<300,260,0>过`. Note the base64 step must ignore
  non-alphabet characters: upstream's strict STANDARD decode after its
  `is_ascii()` filter would keep `\x00\x7f` and reject the body, while the
  audit's verification (Python `b64decode(validate=False)`) drops them, so
  `decode_lrcx_body` follows the verification semantics and documents the
  divergence (the exact trailing-junk layout has no live evidence). The
  vector was constructed and verified with a Python reimplementation of the
  chain before pinning, and the ciphertexts of the request params were
  independently re-derived in the same run (both MATCH).
- **`search_page.json`**: a reconstructed `search.kuwo.cn/r.s` response
  shape. Key names (`TOTAL`, `SHOW`, `abslist`, `MUSICRID`, `SONGNAME`,
  `ARTIST`, `ALBUM`, `ALBUMID`, `DURATION`, `N_MINFO`) follow
  `kw/search.rs:61-136`; the first item's `N_MINFO` is the audit-recorded
  sample whose DRM bitrates (22000/20900) stay unmapped. Values are
  placeholder samples, not real catalog data. One item carries `DURATION` as
  a JSON number to pin musid-dli's `scalar_u64` normalization (upstream
  `search.rs:117-120` reads the string shape only).
- **`mobile_play.json`**: the placeholder body is the live-observed paid
  response recorded in the audit (`kw/url.rs:10-12` module doc: code 200,
  rid 260839262, 11 s, for a request of rid 228908); the good body and the
  string-shaped rid are the upstream inline test vectors
  (`kw/url.rs:326-333`). The `kw-*.kuwo.cn` hosts are placeholder CDN names.
- **`web_play.json`**: **code-shape contract only**. No live capture of the
  signed web playUrl exists upstream (`kw/url.rs:188-225`), so the success
  body is shape-derived and its `data.url` is a placeholder sample; only the
  `code != 200` rejection path is vectorizable. The `illegal_session` string
  is the exact marker body from `kw/session.rs:127-130`.
- **`url_filter.json`**: the `kw/url.rs:336-342` inline test vectors
  verbatim (the CDN also returns the paid-content text in place of a URL).
- **`cover_music_info.json`** (2026-11-18 cover slice): **shape vectors**
  from the 2026-11-17 audit live smoke (`kw/url.rs:108-117`): `success` is a
  `code=200` body whose `data.pic` is an absolute https albumcover URL (the
  literal `https://img1.kuwo.cn/star/albumcover/500/49/84/3394948607.jpg` is
  the day-of baseline for rid 6550104, recorded only as a comment: CDN
  hosts/paths rotate, so the test asserts the `https://` + `albumcover`
  shape, not the literal URL); `not_found` is the live-observed invalid-rid
  body (`code=-1`, `msg=歌曲不存在`); `empty_pic`/`missing_pic`/`paid_text_pic`
  pin that an unusable `data.pic` takes the same `is_http_url` filter as the
  playUrl gate. Not a captured HTTP response.
- **`cover_artist_pic.json`** (2026-11-18 cover slice): **shape vectors**
  from the 2026-11-17 audit live smoke (`kw/url.rs:120-137`): `success` is
  the one-line plain-text body artistpicserver answers with (the literal
  `http://img1.kwcdn.kuwo.cn/star/albumcover/...` is the day-of baseline for
  rid 6550104, again shape-asserted only); `no_pic` is the live-observed
  invalid-rid body (plain text `NO_PIC` at HTTP 200, so `is_http_url` is the
  only defense); `padded` is a **formula-class** entry pinning the upstream
  `.trim()` of the response body. Not a captured HTTP response.
- **`playlist_search.json`**: the `kw/playlist.rs:494-501` inline test vector
  verbatim as `literal` (the JS object literal: bare keys, single-quoted
  strings, trailing semicolon) plus the valid-JSON passthrough check from the
  same test. `valid_json`/`uppercase_fields` are shape-derived extra entries
  pinning the bad-entry skips and the three-way field-case fallback
  (`kw/playlist.rs:279-283`) and the `_150.` → `_700.` cover rewrite
  (`kw/playlist.rs:297-300`). No captured r.s playlist-search response
  exists upstream.
- **`playlist_list.json`**: a reconstructed `getRcmPlayList`/`getTagPlayList`
  body (`kw/playlist.rs:43-62,365-403`): `code==200`, entries in
  `data.data[]`, item fields `id`/`name`/`digest`/`img`/`total`/`desc`/
  `listencnt` per `parse_playlist` (`kw/playlist.rs:413-436`); the numeric
  `id` and numeric `total` entries pin string/number normalization. Values
  are placeholder samples, not real catalog data.
- **`playlist_categories.json`**: a reconstructed `getTagList` body
  (`kw/playlist.rs:317-362`): groups of `{name,data[]}` with items
  `{id,name,digest,extend}`. It pins the category filter rules
  (`kw/playlist.rs:346-357`): empty id/name dropped, `digest` set to anything
  but `"10000"` dropped (legacy categories), `extend` containing "HOT"
  case-insensitively flags `hot`, and the synthetic leading 全部 category.
- **`playlist_detail.json`**: a reconstructed `pl.svc` body
  (`kw/playlist.rs:182-199`): gate `result=="ok"`, rows in `musiclist[]`
  using the `kw/leaderboard.rs:76-125` parse shape (`id`/`name`/`artist`
  (`&`-joined)/`album`/`albumid`/`song_duration|duration`/`pic`/`formats`)
  plus `total`/`title`/`info`/`pic` for the metadata. The `formats` ladder
  pins the token map (MP3128|WMA128→Low128, MP3H→High320, ALFLAC→Flac,
  HIRFLAC|HIRES→Flac24, empty→Low128). Placeholder samples again.
- **`playlist_digest_node.json`**: shape of the qukudata `q.k` node-info
  reply (`kw/playlist.rs:445-464`): only `child[0].sourceid` is read; empty
  child, blank sourceid or missing `child` are all NotFound (no fallback
  path upstream).
- **`chart_catalogue.json`**: reconstructed qukudata board-tree body
  (`kw/leaderboard.rs:14-41` `get_boards`; no upstream fixture exists:
  `leaderboard.rs` has no test module). Row keys follow the 2026-11-18
  live capture ({`id`, `name`, `disname`, `info`, `source`, `sourceid`,
  `pic`, `like`, `listen`, `tips`, `isnew`, `newcnt`, `attribute`,
  `child`}); row 0 carries the audit's recorded vector (`sourceid="340"`,
  `name="青少年专属飙升榜"`, `info="更新于2024-08-15"`, the live `source`
  distribution is `"1"` x31 / `"27"` x1). The `source=="27"` row pins the
  external-link board filter (its `sourceid` is a tencentmusic URL), the
  blank-name and missing-sourceid rows pin the row drops, and the numeric
  `sourceid` pins string/number normalization. `url_golden` is the
  audit's live-verified URL verbatim (HTTP 200, 9359 bytes, 32 children).
  `no_child` and `all_external` pin the two upstream `get_boards` failure
  branches.
- **`chart_songs.json`**: reconstructed kbangserver board body
  (`kw/leaderboard.rs:51-100` `get_list`/`parse_song`; again no upstream
  fixture) over the 2026-11-18 audit vectors: top-level keys
  {`name`, `leader`, `term`, `info`, `pic`, `pub`, `timestamp`, `num`,
  `v9_pic2`, `type`, `musiclist`, `volume`, `currentVolume`} with `num` as
  the string `"300"`; row 0 is the audit's recorded board-93 vector
  (`id="668801544"`, `song_duration="189"` next to the bogus
  `duration="3"`, `formats` "MP3H|MP3128|ALFLAC" mixed with the unmapped
  tokens DDJOC768/AC4256/ZPGA714/ZPLY/BCMS); rows 1-3 are reconstructed
  placeholder rows pinning the numeric id, the `&` artist split, the
  `song_duration`→`duration` fallback, the empty-id drop and the
  empty-formats Low128 floor. `url_golden_board93_rn3` is the audit's
  golden URL probe (pn=0, rn=3). The top-level board metadata
  (name/info/pub/term/pic) is present but dropped: upstream reads only
  `musiclist` and `num`, and `num`/`has_more` have no slot in the
  musid-dli chart model. `no_musiclist` mirrors the live bad-board-id body
  `{}`: musid-dli classifies it NotFound (a deliberate reclassification of
  the upstream parse error); `empty_musiclist` stays a valid empty page.
- **`album_info.json`**: reconstructed `search.kuwo.cn/r.s stype=albuminfo`
  body from the 2026-11-18 live audit (no upstream fixture exists: grep
  `albuminfo` in voicefox tests/app/docs has zero hits). `url_golden` is the
  audit's endpoint vector verbatim (albumid=104004). `literal` mirrors the
  albumid=1293 capture: top-level meta (`albumid/name/artist/artistid/
  songnum/pub/company/lang/pay/vip/pic/hts_img`), row 0 with the recorded
  bare `musicrid`, camelCase numeric `albumId`, `duration:'342'`, `pic120`,
  `formats` token list and the full recorded `N_MINFO` (whose in-value
  `foo:bar` pairs are exactly what breaks a regex-based key quoter); rows
  1-3 are reconstructed placeholder rows pinning the no-N_MINFO formats
  fallback, the empty-rid row drop and the `MUSIC_`-prefixed variant.
  `empty` mirrors the albumid=104004 capture (songnum '0', `musiclist []`:
  a real, empty album, not the synthetic 104004/叶惠美 pairing used in
  older fixtures).

A fixture passing here says nothing about live endpoints
(`docs/tasks.md` §15.4); live behaviour is covered by the kuwo smoke.

## Files and the gotchas they pin

- `web_secret.json`: `Secret` = hex(cookie XOR LCG-mask) + 8 hex nonce
  digits; `secret.len() == 2 * cookie.len() + 8`, all ASCII hex.
- `lyric_params.json`: request params are `yeelion`-cyclic-XORed then
  base64-encoded; XOR is symmetric, so decryption recovers the params. The
  lrcx case appends `&lrcx=1` to the same params.
- `lyric_framed.bin` / `lyric_unframed.bin`: the `tp=content\r\n\r\n` frame
  is skipped when present, otherwise the whole body is treated as zlib;
  inflate then decode **GB18030** (not UTF-8).
- `lyric_lrcx_body.bin`: the word-lyric decode chain is
  frame skip → zlib → keep ASCII bytes → ignore non-alphabet characters
  (the fixture's trailing `\x00\x7f` plus `0x9f 0x8e` junk is dropped) →
  base64 STANDARD → `yeelion` XOR → GB18030; the payload is the
  lx-music unified karaoke text (`[line_start_ms,line_dur]` heads,
  `<rel_start,dur[,0]>word` tags with word starts relative to the line
  head).
- `search_page.json`: `MUSICRID` loses its `MUSIC_` prefix, `ARTIST` splits
  on `&` (blank segments dropped), titles are trimmed, a blank rid or blank
  title entry is skipped without failing the page, `bitrate` maps
  2000->Lossless / 320->High / 128->Low only, and
  `has_more = items.len() >= limit && SHOW >= limit`.
- `mobile_play.json`: paid songs return `code=200` with an ~11 s
  placeholder whose `rid` differs from the requested one, so the body must
  be rejected (rid mismatch or zero duration) and never served; `rid` and
  `duration` arrive as JSON numbers or strings and both shapes normalize.
- `web_play.json`: `code == 200` is required and the URL must pass the
  http(s) filter; `code = -1` is paid content, other codes are
  rights/region blocks; the `The request is illegal!` marker signals a
  rejected web session.
- `url_filter.json`: only `http://` and `https://` strings pass.
- `cover_music_info.json`: `code == 200` yields `data.pic` and the trimmed
  pic must pass the http(s) filter; `code = -1` (歌曲不存在), an empty pic,
  a missing pic, a non-URL pic and malformed JSON all yield None so the
  two-level fallback falls through to artistpicserver.
- `cover_artist_pic.json`: the trimmed single-line body is accepted only
  when it passes the http(s) filter, which turns the plain text `NO_PIC`
  answer of an invalid rid into None.
- `playlist_search.json`: `parse_kuwo_json` first tries strict JSON, then
  quotes bare keys, collapses doubled quotes, converts single-quoted strings
  and trims the trailing semicolon; `total` parses back as the *string* "10".
- `playlist_list.json`: `digest` non-empty composes the synthetic id
  `digest-{digest}__{id}`; empty id or empty/missing name drops the entry.
- `playlist_categories.json`: the leading 全部 entry is synthetic
  (id empty, group 全部, hot); legacy categories (`digest` non-"10000") are
  skipped; `extend` "hot"/"HOT" both flag `hot`, missing/other `extend` does
  not.
- `playlist_detail.json`: `result=="ok"` gates the body; `musiclist` rows
  skip empty ids, `artist` splits on `&`, `song_duration` wins over
  `duration`, `formats` tokens map to quality tiers with a Low128 floor; a
  missing `title` falls back to `酷我歌单 {id}` and a missing `musiclist`
  means NotFound for the metadata path.
- `playlist_digest_node.json`: `digest-5__` node ids resolve through
  `child[0].sourceid` only; everything else is NotFound.
- `chart_catalogue.json`: `source` string-compares to `"1"` only (the live
  `"27"` row is an external-link board whose `sourceid` is a URL); the
  board id is `sourceid`, not the tree `id`; blank sourceid or name drops
  the row; `info` maps to the chart description; a missing `child` and an
  all-external catalogue are both catalogue errors (upstream get_boards
  parse failures).
- `chart_songs.json`: rows reuse the pl.svc parse shape; `song_duration`
  beats the bogus live `duration`; bad rows are dropped before ranks are
  renumbered 1..N; a missing `musiclist` (live `{}` for a bad board id) is
  NotFound while a present-but-empty `musiclist` is a valid empty page;
  board `num`/`has_more` and the top-level board metadata have no
  musid-dli chart slot and are dropped.
- `album_info.json`: the albuminfo body is the JS object literal (no
  trailing semicolon on this route, unlike the ft=playlist variant), so the
  string-state-aware `parse_kuwo_json` rewrite is required; the row's
  `musicrid` is bare (the `MUSIC_` prefix is stripped only when present, or
  every live row would be dropped), the row album id is the camelCase
  numeric `albumId`, `N_MINFO` maps only 2000/320/128 with `formats` tokens
  as fallback, and an empty `musiclist` is an explicit empty result (the
  upstream conflation with a parse failure is not copied).
- `link_targets.json`: the `kw/parse.rs:104-125` inline test vectors
  verbatim (first two positives, all rejections) plus two derived
  positives pinning the trim-before-match and the unanchored capture.
  Upstream has no scheme/host validation, so a `play_detail/` fragment
  inside a query string also matches: a documented audit gap that the
  port inherits. Only these two link shapes exist upstream; album/artist
  forms are never produced.
- `songinfoandlrc.json`: the `m.kuwo.cn/newh5/singles/songinfoandlrc`
  metadata route (kw/parse.rs:62-96). The `success` body is the
  2026-11-18 audit **live capture** for rid=82068 (real values, full
  `songinfo` key set including the pay/isdownload/coopFormats/formats/
  mp3Nsig fields that nobody upstream reads); the `data.lrclist[]`
  (19 live rows) and `data.simpl.musiclist[]` (5 live rows) are truncated
  to two/one rows because the parser reads neither (lyrics have their own
  endpoint path upstream). `not_found` is the live failure envelope for
  non-existent rids (228908/1091518 captured): `status:301` with
  `data:null`. `reqid` is request-scoped and the success capture's value
  stands in as a placeholder; the empty-`songName` gate turns this body
  into NotFound, exactly like upstream, which never reads `status`
  either. `numeric_duration` is **shape-derived** (upstream has no test
  for fetch_song): it pins the numeric branch of the duration fallback
  (string parse first, `as_u64` second) and the `&`-split of the artist.
  No stream URL or quality ladder is mapped from this endpoint: the link
  parse is metadata-only upstream (paid songs still list).

## Not covered (would target code musid-dli does not implement yet)

- Covers and the legacy `convert_url2` + DES
  path (documented as not implemented upstream either, `kw/url.rs:54`).
  (Albums, charts and link parsing moved out of this list in the
  kuwo-album, kuwo-chart and kuwo-link slices; see `album_info.json`,
  `link_targets.json` and `songinfoandlrc.json` above.)
- No file-size field exists in any kw response; no vector can pin one.
