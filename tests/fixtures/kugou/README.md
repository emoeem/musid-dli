# Kugou offline fixtures

Offline fixtures and vectors for the Kugou provider, used by the
`#[cfg(test)]` module in `src/source_kugou.rs` (loaded with `include_str!`
and `include_bytes!`).

## Derivation (honesty note)

The upstream VoiceFox `kg` module ships **no** fixture files: every kg test
is an inline `#[cfg(test)]` literal (`parse.rs`, `lyric.rs`, `crypto.rs`,
`playlist.rs`, `login.rs`), and `search.rs` / `url.rs` / `leaderboard.rs` /
`session.rs` have no tests at all. Nothing in `tests/fixtures/kugou/` is a
captured HTTP response:

- **`link_targets.json`, `playlist_page.html`, `category_ids.json`**:
  reconstructed from the audit-recorded upstream inline tests
  (`kg/parse.rs:139-188`, `kg/playlist.rs:516-541`) and the regex/marker
  shapes in `kg/parse.rs:29-77` / `kg/playlist.rs:84-116`. HTML content is a
  minimal page built to exercise `special_page_title` +
  `embedded_song_data` + `kg_parse_special_song` in one file; the values are
  placeholder samples, not real catalog data.
- **`krc_encrypted.bin`**: a deterministic KRC ciphertext built with the
  `kg/crypto.rs:7-26` algorithm (`krc1` magic + 16-byte cyclic XOR key +
  zlib); verified to decrypt to the recorded plaintext with python3. Not a
  payload captured from lyrics.kugou.com.
- **`krc_parse.json`**: the `kg/lyric.rs:279-312` test vector; the
  `[language:...]` base64 is the compact-serialized metadata payload from the
  audit.
- **`login_sign.json`**: the `kg/login.rs:104-114` formula with the
  `login.rs:295-302` inline test values.
- **`v5_sign.json`**: **formula-class only**. Upstream `kg/url.rs:215-259`
  ships the recipes but no golden request, so the access_key/signature pair
  here was recomputed from the documented md5 formulas (and cross-checked
  against upstream's isomorphic `login.rs` test). It pins the 26-parameter
  set and ordering against drift; it does not prove the server accepts it.

A fixture passing here says nothing about live endpoints
(`docs/tasks.md` §15.4); live behaviour is covered by the kugou smoke.

## Files and the gotchas they pin

- `link_targets.json`: hash wins first; `special/single/546903.html` must
  yield `546903` (not `546903.html`, or `special_detail` builds
  `single/546903.html-5-9999.html`); bare `album/12345.html` is a recognized
  album shape; unrecognized links are rejected.
- `playlist_page.html`: the playlist name lives in the page `<title>` with a
  site suffix (`-`/`_`/`|` cut), the song array lives in the embedded
  `global.data = [...]` JS (the `var data =` variant is also accepted), and
  playlist-page `duration`/`timelength` are **milliseconds** while covers
  come from `trans_param.union_cover` with a `{size}` placeholder.
- `krc_parse.json`: word timings are line-relative offsets; the third value
  inside `<offset,duration,extra>` is ignored; type 0 language content is
  romanization, type 1 translation.
- `krc_encrypted.bin`: `krc1` magic, 16-byte cyclic XOR key, zlib inflate.
- `login_sign.json` / `v5_sign.json`: both signatures are "key-sandwiched"
  md5 over key-sorted `k=v` pairs joined directly.
- `category_ids.json`: `id:tagid` pairs; a missing tagid defaults to `0`.
