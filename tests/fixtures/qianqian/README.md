# Qianqian offline fixtures

Offline fixtures and vectors for the qianqian provider, used by the
`#[cfg(test)]` module in `src/source_qianqian.rs` (loaded with `include_str!`).

## Derivation (honesty note)

The upstream VoiceFox `qianqian` module ships **no** fixture files; its only
inline tests are `crypto.rs:73-80` (signature shape), `song.rs:218-248` (track
parse + rejects), `url.rs:94-108` (rate ladder), `playlist.rs:188-212`
(playlist parse + rejects), `album.rs:147-175` (asset-code sanitize; the
`parse_album` tests cover the dead `search_albums` path, not ported) and
`parse.rs:138-179` (link targets). Everything else is captured or derived:

Live captures (2026-11-19, anonymous read-only GETs, gzip-decoded
byte-for-byte, whole list items beyond the kept prefix dropped):

- `search_songs.json`: `/v1/search?word=晴天&type=1&pageNo=1&pageSize=5`,
  first 2 of 5 items verbatim (TSID `T10065400429` 等晴天 / `T10063944833`
  晴れの日(晴天), both with the full 128/3000/320/64 `rateFileInfo`).
- `tracklist_category.json`: `/v1/tracklist/category` verbatim (5 groups,
  22 subCate entries; 语种/场景/主题/风格 and more).
- `tracklist_list.json`:
  `/v1/tracklist/list?subCateId=2368&pageNo=1&pageSize=3`, first 2 of 3 rows
  verbatim (`309558` 怀旧经典 | 华语至醇金曲 et al).
- `tracklist_info.json`: `/v1/tracklist/info?id=309558&type=0`, first 3 of
  20 `trackList` rows verbatim (26-track playlist).
- `song_info.json`: `/v1/song/info?TSID=T10065400429` verbatim, including
  the `lyric` URL on the static taihe CDN.
- `tracklink_free_128.json`:
  `/v1/song/tracklink?TSID=T10063944833&rate=128` (free track `isVip:0`),
  verbatim: `path` on audio04.dmhmusic.com with the pre-signed `xcode`,
  `size 4794100`, `filemd5 05450caa...`, `expireTime`, `format "mp3"`,
  `bits 16`.
- `tracklink_vip_nopath.json`:
  `/v1/song/tracklink?TSID=T10065400429&rate=128` (VIP track `isVip:1`,
  `pay_model:3`), verbatim: HTTP 200 `errno 22000` with full metadata
  (`size`, `filemd5`, `expireTime`) but **no `path` key** and no
  `trail_audio_info`.
- `search_playlist_refused.json`: `/v1/search?word=华语&type=6`, verbatim
  refusal `{"state":false,"errno":23001,"errmsg":"不被支持的搜索类型(10000)",
  "data":[]}`: playlist keyword search is dead live (2026-11-19).
- `album_info.json`: `/v1/album/info?albumAssetCode=P10003660422` verbatim
  (逆流而上, 7-artist array, `introduce` text, one `trackList` row that
  carries **`assetId` only, no `TSID`**: the documented deviation this
  fixture pins).

- **`sign_vectors.json`**: **formula-derived** MD5 goldens over the
  `signed_query` recipe (sorted unencoded `k=v&` pairs + timestamp, secret
  suffix, hex MD5), recomputed with an independent Python transcription. The
  search and categories vectors were additionally **live-accepted** (the
  exact `(timestamp, sign)` pairs answered HTTP 200 `errno 22000`); the
  tracklink vector pins the musid-dli recipe where `rate` rides *inside* the
  signed text: the upstream recipe (rate excluded, `crypto.rs:22-25`) was
  answered `errno 22001 签名错误:sign无效` by the same probe, so the signed
  shape is the only server-accepted one.
- **`links.json`**: the upstream `parse.rs:140-179` inline goldens verbatim
  (song/songlist/album/albumAssetCode + bare-id + the two rejections) plus
  derived extras (tracklist/playlist path shapes, the unanchored `*id=`
  captures, numeric `albumid=`, empty/dash/slash rejections).

## Protocol facts and gotchas pinned by these fixtures

- **Every `/v1/*` request signs**: sort the parameters plus `timestamp` by
  key, join as *unencoded* `k=v&`, append the secret
  `0b50b02fd0d73a9c4c8c3a781c30845f`, MD5-hex as `sign`, then URL-encode the
  pairs (upstream `crypto.rs:27-59`). `timestamp` is unix seconds.
- **Gzip always**: the endpoints ignore `Accept-Encoding` and answer gzip
  even when identity is declared (upstream `song.rs:44-49`); the decoder
  sniffs the `1f 8b` magic per body.
- **Envelope**: `state: true` + `errno: 22000` on success; the two tracklist
  endpoints are gated on `state == true || errno == 22000`
  (`playlist.rs:58-60`, `:110-112`), while search and tracklink are *not*
  gated at all upstream: refusals parse to empty results / `NotFound`
  through the unguarded field reads (pinned by the refused fixture).
- **Tracklink URL delivery**: a full file is `data.path`; the preview
  fallback is `data.trail_audio_info.path` (upstream `url.rs:47-69`). A VIP
  track answers path-less metadata anonymously, so anonymous resolution
  exhausts the ladder into `NotFound` (live-pinned by
  `tracklink_vip_nopath.json`); the free-track shape carries everything the
  M3 stream fields need (`size` to `content_length`, `filemd5` to `checksum`
  with the MD5 default algorithm, `expireTime` to `expires_at_unix`,
  `format` to `AudioFormat`), with `size_is_advisory: true` per upstream
  `url.rs:81-82`.
- **Playlist search refused live**: `type=6` answers `errno 23001`; the
  ported `search_playlists` keeps upstream parity (parse to empty) and the
  provider row records the face as live-refused rather than verified.
- **Album rows changed shape**: `trackList` rows under `/v1/album/info`
  carry `assetId` (same value space as TSID) and no `TSID`; upstream's
  `parse_song` would drop every row (then `NotFound`). musid-dli extends the
  id probe with `assetId` (documented deviation, `track_tsid`).
- **Playlist detail carries no creator key** of the five upstream probes
  (`creator/author/userName/nickName/ownerName`): `creator` stays `None`
  live; `trackCount` (26) is the real fill for `Playlist.track_count`.
- **Category list rows**: `desc` is an empty string live, so the joined
  `tagList` (upstream `playlist.rs:117-131` backfill) is the real
  description; `play_count` has no live producer outside the refused
  playlist search (upstream `playCount` field read kept, fixture-pinned by
  the upstream inline vector in
  `parses_searched_playlists_and_rejects_incomplete`).

## What a fixture passing here does and does not say

The parse-tier vectors assert that musid-dli's parsers accept the exact
shapes the live anonymous endpoints served on 2026-11-19 and the exact
shapes upstream's inline tests pin. Live behavior of the endpoints
themselves (availability, VIP gating, link expiry windows) is covered by
`examples/qianqian_smoke.rs`, not by these files.
