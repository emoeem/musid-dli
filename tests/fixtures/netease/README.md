# NetEase response fixtures

Offline fixtures shaped like real NetEase Cloud Music API responses, used by the
`#[cfg(test)]` module in `src/source_netease.rs` (loaded with `include_str!`).

## Derivation (honesty note)

No raw NetEase response captures exist in this repository or in the upstream
VoiceFox `wy` module (its `source/tests/fixtures/` only ships a JS custom-source
script; every wy test there is an inline `json!` literal). Every fixture here is
therefore **reconstructed**, not captured:

- **Field shapes** mirror exactly what the parsers read, as recorded in the
  2026-11-16 source audit of `voicefox/source/src/wy/{parse.rs,playlist.rs,
  leaderboard.rs,search.rs}` (see `docs/VOICEFOX_AUDIT.md` and the audit
  conclusion for slice `netease-fixture-hardening`).
- **Live-verified parameters**: the 2026-11-16 anonymous smoke run recorded in
  `docs/IMPLEMENTATION_STATUS.md` ("playlist detail plus 145 tracks", "5 hot
  lists", "71 categories", "album 叶惠美, 11 tracks", "artist: 50 top tracks,
  5 albums page 1", "65 charts") drives counts like `trackCount: 145`, 50
  `hotSongs`, 11 album songs and 5 `hotAlbums`.
- **Inline vectors** that the audit extracted from upstream tests are kept
  verbatim: the `trackIds` mix of `{"id":N}` objects and bare numbers, the
  `hotAlbums` entries with a single `artist` object and preview `songs`, the
  chart entry `{id: 19723756, name: 飙升榜, updateFrequency: 每天更新}`, the
  legacy song detail shape (`artists`/`album`/`duration`) and the playlist
  catalogue groups (`语种`/`风格`).
- All remaining values (ids, names, URLs, sizes) are **placeholder samples**
  chosen to exercise every parser branch. They are not verified catalog data;
  a fixture passing does not mean the live endpoint was re-verified
  (`docs/tasks.md` §15.4 separates fixture tests from live smoke tests).

## Protocol gotchas pinned by these fixtures

- `playlist_detail_v3.json`: favorited playlists only return the first ~20
  songs inline in `playlist.tracks`; the complete list lives in
  `playlist.trackIds` (objects `{"id":N}`, bare numbers tolerated, non-positive
  ids dropped). The `n` query parameter is ignored by the server.
- `song_detail_legacy.json`: the `/api/song/detail` link-resolution endpoint
  returns the legacy `artists`/`album`/`duration` field names, which
  musid-dli's `parse_track` handles alongside the v3 `ar`/`al`/`dt` shape
  (upstream `parse_song` only reads the v3 shape).
- `artist_detail.json` / `artist_albums.json`: these `/api/artist/{id}` and
  `/api/artist/albums/{id}` endpoints have no VoiceFox counterpart (upstream
  falls back to name-based search); their shape basis is musid-dli's own
  2026-11-16 live verification.
- `album_detail.json`: `/api/v1/album/{id}` (musid-dli) and the legacy
  `/api/album/{id}` (upstream) carry the same `album` node fields;
  `track_count` prefers `size` and falls back to `songs.len()`.
- `song_enhance_player_url_v1.json`: the `/eapi/song/enhance/player/url/v1`
  stream data block (`data[0]`: `url`/`size`/`md5`/`level`/`encodeType`,
  `freeTrialInfo` null on full files). The `md5` value is deliberately
  uppercase so the parse path's `to_ascii_lowercase` normalization is
  exercised (live digests arrive lowercase); the URL carries no `ex`/`expires`
  query parameter, matching the CDN shape the `url_expiry` parser reads.
  Consumed through the real `resolve` path over a mock transport by
  `fixture_stream_response_pins_md5_checksum_algorithm`
  (src/source_netease.rs), which pins the M3 `checksum_algorithm`
  backfill (MODEL_ALIGNMENT_DESIGN.md §2.3).
