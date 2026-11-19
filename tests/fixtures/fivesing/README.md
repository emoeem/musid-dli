# 5sing offline fixtures

Offline fixtures and vectors for the 5sing provider, used by the
`#[cfg(test)]` module in `src/source_fivesing.rs` (loaded with `include_str!`).

## Derivation (honesty note)

Unlike every other provider fixture set built so far, **most files here are
trimmed verbatim live captures**, taken anonymously on **2026-11-18** with the
upstream Chrome/143 User-Agent and `Referer: http://5sing.kugou.com/` headers
(all four endpoints answered HTTP 200 on the first anonymous attempt; the
captures were made in this audit session and the response values are kept
byte-for-byte, only whole `list` items beyond the first two were dropped):

- **`search_songs.json`**: live `search.5sing.kugou.com/home/json?keyword=晴天&sort=1&page=1&filter=0&type=0` envelope (`list` + `type` + `pageInfo`), first 2 of 10 items verbatim. Both items are real catalog entries (songId 15622111 / 15089953, both `fc`).
- **`search_playlists.json`**: live same endpoint with `type=1&keyword=翻唱`, first of 10 items verbatim plus the real `pageInfo`.
- **`song_info.json`**: live `mobileapi.5sing.kugou.com/song/newget?songid=15622111&songtype=fc`, envelope and the full 35-key `data` block verbatim.
- **`song_info_missing.json`**: live `newget?songid=0&songtype=fc`, which is `{"message":"操作失败，请再次尝试","success":false,"data":[],"code":20001}`.
- **`get_song_url.json`**: live `mobileapi.5sing.kugou.com/song/getSongUrl?songid=15622111&songtype=fc`, all three tiers with backups, md5s and string sizes verbatim.
- **`get_song_url_missing_param.json`**: live `getSongUrl?songid=0&songtype=fc`, which is `{"code":1001,"message":"缺少songid","success":false}` (no `data` key at all).
- **`playlist_info.json`**: live `mobileapi.5sing.kugou.com/song/getsonglist?id=5c198e848788ade7cbe97542&songfields=ID,user`.
- **`playlist_info_missing.json`**: live `getsonglist?id=00000000&songfields=ID,user`, which is `{"message":"歌单不存在","success":false,"data":[],"code":24}`.

The remaining files are **not** captures:

- **`get_song_url_sparse.json`**: schema-derived case pinning the tier filter
  (empty string and `"0"` mark a missing tier) from upstream
  `voicefox/source/src/fivesing/api.rs:284-294`; no live response with a
  missing tier was captured in this session, so the URL values are
  placeholders shaped after the live response.
- **`clean_text.json`**: the first three cases are the upstream inline test
  goldens verbatim (`fivesing/api.rs:438-441`); the fourth is the live
  highlight shape `<em class="keyword">` (harder than the bare `<em>` upstream
  tests); the fifth only pins the shared entity decode list
  (`api.rs:74-84`, `source_fivesing.rs:196-202`), which was not separately
  live-captured.
- **`links.json`**: the first three cases are the upstream inline test
  goldens (`fivesing/api.rs:492-507`); the fourth and fifth carry live-captured
  URLs (search item `songurl` field; playlist search `url` field, which also
  matches upstream's `api.rs:332` link construction); the sixth pins the
  scheme-agnostic substring match both implementations share.
- **`split_id.json`**: the first two cases are the upstream
  `fivesing/mod.rs:183-201` inline test goldens; the third is upstream
  `mod.rs:202-209` (upstream returns `None`, musid-dli errors); the fourth is
  a **deviation pin** (see below), not exercised by any parser's own code
  path.

## Protocol facts and gotchas pinned by these fixtures

- **Endpoints (all plain HTTP GET, no signing, no encryption, no cookies)**:
  `http://search.5sing.kugou.com/home/json` (search, `type=0` songs / `type=1`
  playlists), `http://mobileapi.5sing.kugou.com/song/newget` (song detail +
  LRC), `http://mobileapi.5sing.kugou.com/song/getSongUrl` (stream tiers),
  `http://mobileapi.5sing.kugou.com/song/getsonglist` (playlist metadata;
  `songfields=ID,user`). Sources: upstream `fivesing/api.rs:14-17`,
  musid-dli `source_fivesing.rs:30-33`; all four re-verified live 2026-11-18.
- **Envelope codes differ per endpoint**: `home/json` has no code field at
  all; `newget`/`getsonglist` return `code` 0/24/20001 with `data` possibly an
  **empty array** (missing/inaccessible, per `api.rs:310-314` and
  `source_fivesing.rs:520-523`); only `getSongUrl` gates on `code == 1000`
  (`api.rs:272-274`, `source_fivesing.rs:153`). musid-dli checks the SN/code
  gates exactly like upstream; no global envelope contract exists.
- **Highlight shape**: real titles carry `<em class="keyword">` (live), not
  the bare `<em>` upstream's tests assume. Both implementations decode the
  same six entities and strip tags; musid-dli uses a char loop instead of a
  regex (no regex dependency in this feature), so a stray `<` swallows the
  rest of the string until the next `>`: an implementation difference that
  never triggers on observed titles.
- **Duration is estimated, not returned**: search items only carry
  `songSize` (bytes); both implementations derive seconds as
  `size * 8 / 320000` (320 kbps assumption; upstream `api.rs:129-131`,
  musid-dli `source_fivesing.rs:183-186`). Live item 1: 10462607 bytes becomes 261 s.
- **Search items carry no `user` object** (live 2026-11-18, both song and
  playlist search): upstream reads `item["user"]["I"]` for the avatar
  (`api.rs:134-137`) and musid-dli does the same (`source_fivesing.rs:248`),
  but the live item key set has no `user` key at all, so search covers are
  always `None` today. `song_info.json` (newget) does carry
  `user.I`/`user.NN`.
- **getSongUrl extras both implementations drop**: the live `data` block
  carries `squrlmd5`/`hqurlmd5`/`lqurlmd5` (content MD5s) and
  `sqsize`/`hqsize`/`lqsize` (string byte sizes), yet upstream leaves
  `SongUrl.size/md5` unset (`mod.rs:104-116`) and musid-dli leaves
  `StreamInfo.content_length/checksum` unset (`source_fivesing.rs:369-382`).
  Tier URLs embed a timestamp+hash path segment (`.../202611180449/<hash>/...`),
  so they expire.
- **Referer on direct links is stale upstream**: upstream pins
  `MEDIA_REFERER = "http://5sing.kugou.com/"` as required for media
  (`fivesing/mod.rs:23-24`, attached at `mod.rs:110`). A live probe on
  2026-11-18 fetched 4 KiB of the real SQ URL with `Range: 0-4095` **without**
  a Referer and got `206 audio/mpeg`, identical with the Referer: the CDN no
  longer enforces it. musid-dli's `StreamInfo.headers: vec![]`
  (`source_fivesing.rs:377`) therefore matches today's live behavior even
  though it drops the upstream-declared header; the mobileapi/search endpoints
  are sent the UA+Referer pair by both implementations.
- **Pagination is ignored by both**: `home/json` answers a real `pageInfo`
  (`{"cur":1,"totalCount":8364,"totalPages":837}`), but upstream hardcodes
  `page=1` (`api.rs:107`) and musid-dli reports `total = parsed count`,
  `has_more: false` (`source_fivesing.rs:343-348`). No pagination fixture
  exists: there is no protocol being pinned.
- **newget duplicates stream data**: `song_info.json`'s `data` block also
  carries `squrl`/`hqurl`/`lqurl`; both implementations ignore it and use the
  separate `getSongUrl` endpoint.
- **Lyrics are CRLF LRC**: the live `dynamicWords` (1295 bytes) contains 63
  `\r\n` and zero bare-`\n` lines; `LyricsProvider::lyrics` feeds it straight
  into `parse_lrc`.

## musid-dli deviations pinned or documented here

- **Playlist track lists**: upstream `fivesing/api.rs:307-403` fetches
  `http://5sing.kugou.com/{uid}/dj/{id}.html` and regex-parses
  `<li class="p_rel">` rows into songs; musid-dli reports
  `playlist_tracks` as `Unsupported` citing a 2026-11-17 live check ("JS
  rendered, no song data"). **This audit's live probe contradicts that
  evidence**: the same playlist's HTML page (39697 bytes, HTTP 200 anonymous)
  contains server-side `p_rel` rows and the upstream regex extracted 1 track
  (`fc/16645181` "苗族歌曲翻唱2018" by 迷彩云南), which is partial coverage
  (the playlist's `E` says 4), so the page carries *some* SSR data as of
  2026-11-18. No HTML fixture is committed because musid-dli deliberately
  implements no HTML parsing; the claim's basis is stale and re-verification
  is left to the slice that (re)implements the parser.
- **Playlist metadata fields dropped for model reasons**: upstream keeps
  creator (`userName` with an `"ID: {userId}"` fallback), `playCount` and a
  built `dj` link (`api.rs:163-176`, `api.rs:327-332`); musid-dli's
  `Playlist` model (`src/models/playlist.rs:4-15`) has no creator/play-count/
  link fields, so `fs_parse_playlist` parses `userName`/`userId` and then
  discards them (`source_fivesing.rs:267-275`). The live playlist-search item
  has `userName: ""` anyway.
- **Missing song type**: upstream `split_song_id` returns `None` when the
  type is empty (`mod.rs:54`); musid-dli `split_track_id` returns
  `Ok(("999",""))` for the `extra.songid`-without-`songtype` shape
  (`split_id.json` case 4). Unreachable from musid-dli's own parsers (they
  always set both extras), pinned as documented current behavior.
- **Search total**: upstream returns `total = items.len()` and musid-dli the
  same; neither surfaces the real `pageInfo.totalCount` (8364 for 晴天 live).

## What a fixture passing here does and does not say

The parse-tier vectors assert that musid-dli's parsers accept the exact
shapes the live anonymous endpoints served on 2026-11-18 and the exact
shapes upstream's own inline tests pin. Live behavior of the endpoints
themselves (availability, tier completeness, expiry windows) is covered by
`examples/fivesing_smoke.rs`, not by these files.
