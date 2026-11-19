# Jamendo offline fixtures

Offline fixtures and vectors for the Jamendo provider, used by the
`#[cfg(test)]` module in `src/source_jamendo.rs` (loaded with `include_str!`).

## Derivation (honesty note)

The upstream VoiceFox `jamendo` module ships **no** fixture files and no
captured traffic: every upstream Jamendo test is an inline `json!` literal in
`voicefox/source/src/jamendo/mod.rs` (the signature shape test at
mod.rs:513-525 and the `pick_stream` tier test at mod.rs:528-547). Nothing in
`tests/fixtures/jamendo/` is a captured HTTP response:

- **`x_jam_call.json`**: formula-class vectors. The signature is
  `$sha1(path + nonce)*nonce~` over the ASCII concatenation of path and nonce,
  no query string (upstream mod.rs:66-70, path signed at :98/:243). The
  `/api/search` + `0.5` vector is the upstream inline test assertion; the
  other four were recomputed with SHA-1 from the documented input and
  cross-checked in this session with python hashlib. The fixed
  `x-jam-version: 4gvfvv`, the Chrome/143 User-Agent and the search-page
  Referer are pinned as source constants (upstream mod.rs:25-28); `4gvfvv`
  appears exactly once in the whole upstream tree with no documented origin,
  so it is pinned as-is, not explained.
- **`pick_stream.json`**: the first two cases are the upstream inline goldens
  (mod.rs:528-547); the real-form trackid 1538710 case follows the 2026-11-18
  live capture recorded in the source audit; the unknown-key trap (non-empty
  download dict without the five known keys returns None, no fallback) and
  the mp33 tier are upstream code behavior (mod.rs:133-154), not captured
  traffic.
- **`track_parse.json` / `search_response.json`**: field shapes follow the
  2026-11-18 live `/api/search` capture for trackid 1538710 as recorded in
  the source audit (the temporary capture was not committed). The harmless
  fields (`query`/`score`/`status`/`tags`/`type`/`uniqueId`/`dateCreated`/
  `artistId`/`tagEntityId`, `idstr` as empty string) are carried because the
  parser must ignore them, not because they are understood. Item 2 of the
  search array, all filtered-case items and the cover-fallback case are
  placeholder samples chosen to exercise parser branches, not real catalog
  data.
- **`from=` token values** in the stream-group URLs are kept as recorded in
  the audit but only the URL *shape* is asserted; token validity is unknown
  (an audit gap) and nothing pins the token bytes.

A fixture passing here says nothing about live endpoints
(`docs/tasks.md` §15.4); live behaviour is covered by the jamendo smoke.

## Protocol facts and gotchas pinned by these fixtures

- **Signature**: `x-jam-call = $hex(SHA1(path + nonce))*nonce~`; the nonce
  echoed in the header must byte-match the hashed input. The server does not
  verifiably *enforce* the signature (the 2026-11-18 matrix saw bare curl and
  wrong-signature requests answer HTTP 200, plus two intermittent plain-text
  403 "Forbidden" bodies earlier in the same session), so the signature is
  pinned as "always sent, exact shape", not as a security gate. The
  intermittent 403 is an edge-node anti-scrape behavior; musid-dli's
  single-shot `api_get` surfaces it as a provider error rather than fabricating
  a retry path no upstream basis describes.
- **Nonce**: musid-dli generates `{:.3}` three-decimal floats from system
  nanoseconds instead of the upstream f64 Display shape ("0.5379677314256315");
  the server accepted the three-decimal shape live (2026-11-18), so only the
  self-consistency (echo == hashed input) is pinned.
- **Envelope**: `/api/search` answers with a bare top-level JSON array, not a
  `{data: [...]}` wrapper (`search_response.json`).
- **Search-embedded stream URLs**: search items already carry full
  `download`/`stream` URL groups, and `jam_parse_track` embeds the
  `pick_stream` result into `extra[stream_url/stream_format/stream_quality]`
  so that `resolve()` needs no network (the container endpoints
  `/api/tracks`, `/api/albums`, `/api/playlists` have hung anonymously since
  the 2026-11-17 live check; upstream fetches `/api/tracks` at play time
  instead, a documented musid-dli deviation). This is the musid-dli
  difference this fixture set pins.
- **Tier selection**: download group first (when non-empty), then key order
  flac > mp33 > mp32 > mp3 > ogg; the mp3 key's Low label is approximate:
  the real `/mp32/` URL measured 192k VBR while the stream group's
  `format=mp31` measured 96k (2026-11-18 ffprobe), so picking
  `download.mp3` over `stream.mp3` serves the better stream despite both
  mapping to `Mp3`.
- **Filtering**: id must be non-empty and != "0"; name must be non-empty;
  cover prefers `cover.big.size300` and falls back to the top-level `image`.
- **Pagination**: the upstream search ignores page/limit (limit hardcoded 20,
  `has_more` always false, `total` = parsed count), so no pagination fixture
  exists: there is no protocol to pin.
