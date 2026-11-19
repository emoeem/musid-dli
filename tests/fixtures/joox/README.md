# JOOX offline fixtures

Offline fixtures and vectors for the joox provider, used by the `#[cfg(test)]`
module in `src/source_joox.rs` (`include_str!`).

## Derivation (honesty note)

The upstream VoiceFox `joox` module ships **no** fixture files; its inline
tests cover `strip_jsonp` (api.rs:361-368), the section parse
(api.rs:371-398), `normalize_id` (api.rs:400-404) and the link router
(mod.rs:282-308). Everything else is captured or derived:

Live captures (2026-11-19, anonymous read-only GETs, 3 per endpoint max, no
credentials):

- `search_songs.json`: `/openjoox/v2/search_type?country=sg&lang=zh_cn&key=晴天&type=0`
  verbatim except that `tracks` keeps the first 3 of 30 entries (晴天
  VIP `vip_flag 1 is_playable false`, two free covers with
  `vip_flag 0 is_playable true`). The envelope keys (`error`, `error_code`,
  `has_more`, `next_page`) are verbatim. The live response carries **no
  `section_list`**, which is why the playlist-search face parses to an empty
  result.
- `lyric_live.json`: `web_lyric?musicid=bLnv0PqDX_qAlIqapc%2BOkw%3D%3D`
  verbatim after stripping the `MusicJsonCallback(...)` shell: the inner
  object with `code 0` and the **base64-encoded** `lyric` field (69 LRC lines
  decode). This pins the live server change: upstream api.rs:350-354 reads the
  field as plain text.
- `playlist_dead_404.html`: verbatim nginx 404 body of
  `/openjoox/v3/playlist?...&id=YiT3xohoUs5qeo9aJ2yjDw%3D%3D` (an album id;
  upstream serves albums through this endpoint as special playlists). The
  container faces (playlist, album, playlist/album links) are dead anonymously;
  this fixture pins the refusal shape. `web_get_songinfo` (the stream face) is
  equally dead live (HTTP 404, empty body; no file could be captured).

Derived vectors (no live capture exists):

- `songinfo_stream.txt` / `songinfo_stream_object_kbps.txt`: **schema-derived**
  JSONP vectors for `web_get_songinfo`, transcribed from the upstream
  `fetch_song_detail` reads (api.rs:261-330): `msong`/`msinger`/`malbum`/
  `minterval`/`img`, the dual `kbps_map` branch (JSON-encoded string vs
  object), the availability flags (`"0"`/`0` drops the tier) and the four URL
  fields `r320Url`/`r192Url`/`mp3Url`/`m4aUrl`. URLs use the reserved
  `.example` domain: they pin the shape, not any real CDN address.
- `links.json`: link-router cases transcribed from the upstream tests
  (mod.rs:282-308) plus one URL-decoding capture case and one live-shaped id
  case (the search fixture's id).
