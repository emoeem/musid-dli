# Apple Music offline fixtures

Offline fixtures and vectors for the Apple Music provider, used by the
`#[cfg(test)]` module in `src/source_apple.rs` (loaded with `include_str!`).
Created 2026-11-18 (slice `apple-fixture-hardening`), following the same
fixture-batch pattern as the seven earlier provider batches.

## Derivation (honesty note)

The upstream VoiceFox `apple` module ships **no** fixture files: every apple
test there is an inline `json!` literal (`voicefox/source/src/apple/api.rs:494-586`
and `apple/mod.rs:201-211`). Nothing in `tests/fixtures/apple/` is a captured
HTTP response:

- **`token_bootstrap.json`**: the script-path/JWT-extraction vectors reuse the
  upstream inline test values (`api.rs:494-509`) plus musid-dli's own inline
  test value. The AMP endpoints, storefront, User-Agent, script markers and
  the probe URL are pinned as source constants (`api.rs:18-23` and
  `api.rs:120-137`; musid-dli `src/source_apple.rs` constants block and
  `token()`). The web-embedded JWT itself is **not** pinned: it rotates
  (upstream re-derives it per process from the homepage bundle and probes
  candidates, `api.rs:90-142`), so fixtures only pin the *extraction and
  probe protocol*, never token bytes.
- **`song_parse.json` / `search_response.json`**: golden values follow the
  upstream inline parse test (`api.rs:520-536`: id 1440833098, 晴天, 周杰伦,
  叶惠美, 269000 ms, `{w}x{h}` artwork template). The previews node and the
  detail-call `relationships.albums` shape are reconstructed from the parser
  read paths; their URL values are placeholder samples, not verified catalog
  data.
- **`playlist_parse.json`**: golden values follow the upstream inline parse
  test (`api.rs:538-551`: pl.abc, 华语精选, curatorName, trackCount 50,
  description.standard).
- **`container_tracks.json`**: the `data[0]` + `relationships.tracks.data`
  envelope is reconstructed from `api.rs:335-357`; album/playlist attribute
  values are placeholder samples.
- **`lyrics.json`**: the `relationships.lyrics.data[0].attributes.text` path
  is reconstructed from `api.rs:316-331`; the text is a placeholder built to
  exercise the trim/filter branches.
- **`link_targets.json`**: every upstream-cited case follows the upstream
  inline link test (`api.rs:554-573`) and the `link_target` semantics
  (`api.rs:442-479`).

A fixture passing here says nothing about live endpoints (`docs/tasks.md`
§15.4); live behaviour is covered by the apple smoke (`examples/apple_smoke.rs`).

## Files and the gotchas they pin

- `token_bootstrap.json`: the token bootstrap is homepage, then script path,
  then JWT candidate extraction, then a per-candidate probe against a
  *generic* search (`term=music&types=songs&limit=1`). Never probe with a
  specific song id: a specific id can 404 on storefront grounds, which says
  nothing about token validity (`api.rs:120-137`). musid-dli adds a 60s
  rotation guard around re-fetches (source_apple.rs `token_fetched_at`), an
  upstream-less musid-dli addition.
- `song_parse.json`: search results carry no album id (`album_id` is None);
  only the detail call carries `relationships.albums`, and this batch added
  `include=albums` to that call to mirror upstream's
  `extend=extendedAssetUrls&include=lyrics,albums` (`api.rs:291-299`). Every
  parsed track carries `extra.preview_only=true` and `available_qualities`
  `Low`; resolve() always answers a 30-second M4a preview with quality Low
  (full tracks are DRM-protected and out of scope; see voicefox
  apple/mod.rs:1-6, 22-23, 116-119).
- `search_response.json`: the search envelope is `results.songs.data`;
  missing-name/blank-id rows are filtered; `total` is the parsed count and
  `has_more` is always false; an empty/wrong-shape envelope is an empty page,
  not an error.
- `playlist_parse.json`: playlist ids look like `pl.abc123` / `pl.u-...` and
  keep their casing. Upstream also records creator (artistName, else
  curatorName), a canonical link and a `{kind}_id` extra (`api.rs:237-244`);
  musid-dli's `Playlist` model has no fields for those, so they are dropped
  on port. Same for the description fallback: upstream accepts a bare-string
  `description` (`api.rs:229-236`), musid-dli only reads
  `description.standard`.
- `container_tracks.json`: one envelope serves albums and playlists
  (`/v1/catalog/us/{albums|playlists}/{id}?include=tracks&limit[tracks]=100`);
  empty/absent `data` yields `Error::NotFound` (upstream: an Other error,
  `api.rs:344-347`); paging is decorative, the fetch is capped at 100 tracks.
- `lyrics.json`: lyrics are plain text only (trimmed, no timing, no
  translation); `Ok(None)` for every absent shape (upstream turns the None
  into `FetchError::NotFound` at the trait layer, `apple/mod.rs:101-110`;
  musid-dli returns `Ok(None)` directly, a documented deviation).
- `link_targets.json`: `?i=` beats the album id; the id is the LAST path
  segment before `?`/`#`; bare numeric strings are song ids. **Both of the
  first two behaviors were broken in musid-dli before this batch** (it took
  the first segment and ignored `?i=`) and are fixed in this slice to match
  upstream.

## musid-dli deviations recorded by these fixtures

1. **Link recognition was hardened, not just pinned**: `?i=` handling and
   slug last-segment ids are upstream-cited fixes (`api.rs:447-450`,
   `api.rs:459-468`); case-insensitive matching is a musid-dli extension.
2. **`include=albums` added to the song-detail call** (upstream `api.rs:295-297`
   includes `lyrics,albums`; the lyrics part is not needed here because the
   lyrics path is a separate call).
3. **`Ok(None)` instead of NotFound** for absent lyrics (trait-level
   difference from upstream, see `lyrics.json`).
4. **Playlist creator/link/extra and bare-string description** are dropped
   (model surface, see `playlist_parse.json`).
5. **Token rotation guard** (60s cooldown after a failed bootstrap) has no
   upstream counterpart.
6. Upstream `curator_categories()` (a hardcoded 18-entry cn-curators table,
   `api.rs:369-405`) and `category_playlists`
   (`/v1/catalog/cn/apple-curators/{id}/playlists`, `api.rs:408-432`) are
   **not ported**: musid-dli's `PlaylistProvider::categories`/`playlists`
   answer `Error::Unsupported`, and `get_playlist_categories` in upstream
   is a static table (no fixture exists for it here: there is nothing to
   pin offline).
