# Third-party dependency audit (P1)

Audit date: 2026-11-16. Decisions distinguish verified embeddable crates from projects that are only architecture references.

## M6 lyrics-helper audit (2026-11-20, RESOURCE_DESIGN.md §11)

Re-measured live in the m6-lyrics-helper-audit session. Counting rule
(verbatim, so re-measures reproduce): `cargo tree --locked --no-default-features
--features <face> -e normal --prefix none`, drop lines ending in ` (*)` (graph
repeats), drop the root `musid-dli`, strip the version, deduplicate by crate
name. Resolved versions read from `Cargo.lock` of the same session.

| Feature face | Unique crates | Delta over `lyrics` | Notables |
|---|---|---|---|
| `lyrics` (lyrics-helper 0.5.1, default-features = false) | 98 | baseline | lyrics-core -> ferrous-opencc -> fst + rkyv (OpenCC dictionaries, part of the parse face, not slimmable), tokio, and lyrics-crypto -> aes 0.9.4 |
| `lyrics-cache` (M6) | 98 | **0** | pure gate feature over `lyrics`; tree identical to `lyrics` line for line (`diff` empty) |
| `lyrics-search` (lyrics-helper/search) | 216 | +118 | **reqwest 0.13.5 alongside musid's reqwest 0.12.28 (double major, Cargo.lock:2180-2181 and 2218-2219)**, base64 0.23.1 alongside 0.22.1 (Cargo.lock:128-135), aws-lc-sys 0.47.0 native build (Cargo.lock:115-116), hyper/rustls stack, rustls-platform-verifier 0.7.1, syn 2.0.119 and 3.0.10 in one graph |

An earlier M6 session recorded 78 / +89 on an older lockfile state; the
current lock (newer proc-macro resolutions, syn 3.0.10 among them) measures
98 / +118 with the rule above. The decision below does not change in kind:
the searcher stack still adds on the order of a hundred crates and the double
major stands.

Version-difference pairs this audit pins (the `lyrics` face itself carries
part of them; the search face carries the HTTP stack):

- reqwest 0.12.28 (musid's `source-*`/`download` faces) vs reqwest 0.13.5
  (lyrics-search 0.5.1, Cargo.lock:1482-1499). Verified in one graph:
  `cargo tree --locked --no-default-features --features source-netease,lyrics-search`
  lists both majors; `--duplicates` shows them side by side.
- base64 0.22.1 (musid) vs 0.23.1 (lyrics-parsers/lyrics-search).
- aes 0.8.4 (netease streaming) vs aes 0.9.4, which rides the `lyrics` parse
  face itself via lyrics-crypto 0.5.1 - not only the `ncm` feature as the
  2026-11-16 note assumed.
- syn 2.0.119 (rkyv/phf derive stack) vs syn 3.0.10 (musid's own proc-macro
  stack), both inside the `lyrics` face.
- tokio unifies at 1.54.1, but lyrics-search's manifest requests tokio
  features = ["full"], so enabling `lyrics-search` widens the tokio feature
  set (net/fs/process/signal/io-util) for the whole build.

TLS face of the searcher's reqwest 0.13.5: lyrics-search enables features
`json`, `form`, `rustls` on top of reqwest's default features; `rustls`
resolves to the aws-lc-rs provider (hence the aws-lc-sys 0.47.0 native
build) plus rustls-platform-verifier 0.7.1 and openssl-probe for native
certificate discovery. Verified absent from `Cargo.lock`: native-tls and
openssl-sys - the searcher face never links OpenSSL despite reqwest's
default features being on.

Build cost, measured in this session with a fresh target directory:
`CARGO_TARGET_DIR=<dir> cargo build --locked --offline --no-default-features
--features lyrics-search` finished in 35.05 s (exit 0), target directory
919 MB, with aws-lc-sys compiling on Linux without extra setup. The
`lyrics` face alone stays HTTP-free: `cargo tree` shows no reqwest under
`--features lyrics` or `--features lyrics-cache`.

Decision, standing from the M6 design (RESOURCE_DESIGN.md §11) and re-ratified
on this session's numbers:

1. **Keep lyrics-helper for parsing/decryption and for `lyrics-search` as
   shipped.** The double reqwest major is accepted for opt-in users of
   `lyrics-search`; it never reaches `lyrics` or `lyrics-cache` users, and
   no-default builds stay reqwest-free. The measured build cost (35 s cold,
   919 MB target for the search face) is real but bounded, and the duplicate
   stack is compile-time only: reqwest 0.12 and 0.13 are distinct crates with
   no symbol or feature unification between them.
2. **A self-hosted searcher replacement (LRCLIB/AMLL on the M1 transport
   seam) is deferred, not rejected**: it would eliminate the +118 crates, the
   double major and the tokio "full" widening, but
   `lyrics-helper::search_with_refinement` carries match_type semantics the
   results ordering depends on, so a replacement requires `LyricSearchResult`
   fixture parity first (tasks.md §15.4). The seam it would ride is ready:
   `ClientProfile`/`RetryPolicy`/`Transport`/`Http::send_raw`/`send_api_json`
   (src/transport.rs:245-459) and `into_provider_error` (src/transport.rs:233).
   Owner: a separate slice, not M6 core.
3. The lyrics-helper search stack **bypasses musid's error classification,
   retry and transport** (verified at lyrics.rs:184-196: `search_one` maps a
   searcher error straight to `Error::Provider`, with no musid transport or
   retry in the path). M6 mitigates the observable surface, not the
   dependency: `search_many_typed` maps searcher errors into `Error::Provider`
   categories, and the lyrics-cache negative domain records them per source.

The audit premise is pinned by a unit test:
`cargo_lock_pins_the_audited_dual_reqwest_stack` (src/lyrics.rs, gated
`lyrics-search`) parses `Cargo.lock` at compile time and fails when the dual
reqwest/base64 majors, the audited lyrics-helper 0.5.x release or the
searcher's reqwest 0.13 dependency changes either way - forcing this entry
and the keep-vs-replace decision to be revisited instead of the graph
drifting silently.

## Original audit (2026-11-16)

Audit date: 2026-11-16. Decisions distinguish verified embeddable crates from projects that are only architecture references.

| Candidate | Evidence | Decision |
|---|---|---|
| RustyPipe 0.11.4 | Published Rust library, GPL-3.0, Rust 1.67.1 MSRV; YouTube Music search, track details, albums, artists, playlists, charts, lyrics and audio stream models; optional TLS features | **Integrated as optional source-youtube-music feature.** Search + audio URL resolver compile. Runtime stream reliability requires Botguard/PO-token and live tests; provider uses a dedicated cache directory. |
| ytmusicapi | Python YouTube Music client, not a Rust crate/library | **Not a direct dependency.** Consider only as a separately managed service/bridge if RustyPipe coverage proves insufficient. |
| lyrics-helper 0.5.1 | Apache-2.0, Rust 1.85 declaration, parser/generator/decryption facade; default network search can be disabled | **Adopted for offline lyrics facade.** Network search is a separate optional lyrics-search feature. |
| ncm-api-rs 0.1.0 | Rust SDK with public create_client/ApiClient/Query API, 300+ endpoint claims, reqwest 0.12; docs coverage 45.92%, 0 of 37 items with examples; one published release (2026-04-07); WTFPL reported by upstream README | **Candidate only; do not replace VoiceFox Netease yet.** It is embeddable, but documentation and release maturity are weak; compare cookie/session compatibility and endpoint parity first. |
| ncmapi 1.0.0 | Public NcmClient and focused discovery/library/profile services; MIT; reqwest 0.13 + OpenSSL; docs coverage 33.77%; release 2026-08-26 | **Candidate only.** Clean service architecture, but adds OpenSSL and a newer HTTP stack; migration from legacy ncmapi 0.1.x is breaking. |
| netease-music 0.1.1 | Stateful NeteaseMusicClient, cookie capture, QR login, search, song URLs, lyrics, playlist and user APIs; reqwest 0.12; docs coverage 0.61% | **Candidate only.** Promising session behavior but very weak API documentation and a synchronous surface; test cross-platform TLS and async adapter overhead. |
| MoosicBox music_api 0.5.0 | MPL-2.0; MusicApi, MusicApis, CachedMusicApi and separate model/provider/audio crates; optional actix-web and broader MoosicBox runtime dependencies | **Architecture reference only.** Learn from provider registration/cache/capability design; do not pull the wider ecosystem into the core crate. |
| RSpotify 0.16.1 | Rust Spotify Web API wrapper with auth flows and endpoint helpers | **Candidate provider SDK.** Metadata/account APIs only; no entitlement to full-track audio URLs. Verify scopes, terms, rate limits and current API coverage before integration. |
| KugouMusic.rs | No verified mature embeddable library contract in this pass | **Unverified; retain VoiceFox Kugou adapter.** |
| ncm2mp3 / ncm-core | Workspace separates a Rust core library from CLI; Apache-2.0; release v0.3.2 surfaced | **P4 audit candidate.** Verify actual public exports, package publication, stream-to-Write API, malformed-input bounds and fixture coverage before adding a path/git dependency. |
| lofty 0.22.4 | Already used by VoiceFox; metadata reader/writer | **Adopted for local metadata.** |
| image 0.25.10 | crates.io, MIT OR Apache-2.0, MSRV 1.88; decode-only usage: png (already in the tree via the dev-dependency) plus jpeg (zune-jpeg 0.5.15 / zune-core 0.5.3, MIT OR Apache-2.0 OR Zlib, MSRV 1.75); no default features, no network, no encoder pulled beyond the enabled formats | **Adopted 2026-11-20 (m5-s8) behind feature `local-cover`.** Only the decode-validation face (probe_dimensions) and the test PNG/JPEG synthesis use it; the compile/size weight stays off the default `local` feature, same rationale as ncm. |
| sha2 0.10.9 | crates.io, MIT OR Apache-2.0, pure Rust (RustCrypto); transitive cpufeatures 0.2.17 only; no network | **Adopted 2026-11-20 (m5-s8) behind feature `local-cover`.** Hashes the cover cache key (audio path bytes + picture bytes, sha256, the upstream cover_cache recipe); hex encoding is written in std (the `hex` crate is not pulled for this face). |
| notify 8.2.2 | crates.io (notify-rs/notify), CC0-1.0, rust-version 1.88 (below this crate's 1.89 CI gate); the same major the upstream voicefox watcher pins (`voicefox/Cargo.toml:84` `notify = "8.2"`, which resolves 8.2.0 in the upstream lock); Linux subtree measured live via `cargo tree --locked --features local-watch -e normal`: inotify 0.11.5 (+ inotify-sys 0.1.8), libc 0.2.191, log 0.4.35, mio 1.2.5, notify-types 2.2.0, bitflags 2.13.2, walkdir 2.5.0 (already in the tree via `local`); default features keep notify's internal channel on std::sync::mpsc (`notify 8.2 lib.rs:170-183`; the `crossbeam-channel`/`flume` features stay off); Windows backend windows-sys, macOS fsevent-sys (default `macos_fsevent`); no network, no transcoder | **Adopted 2026-11-20 (m5-s9) behind feature `local-watch`.** Only the watcher event-stream abstraction (`src/local_watch.rs`) pulls the platform FFI backends (inotify/FSEvents/ReadDirectoryChangesW) and notify's event thread; hosts that only batch-scan keep zero watcher cost, the same separation rationale as `ncm`/`local-cover`. Delivery face is callback or std::sync::mpsc; zero tokio/runtime. |
| walkdir 2.x | Already used by VoiceFox | **Adopted for local scanning.** |
| cue-rw 0.2.1 | Already used by VoiceFox | **Preserve existing implementation; wrap after fixture parity.** |

## RustyPipe runtime constraints

- RustyPipe is an embeddable library, not just its CLI; the optional musid-dli adapter uses its public RustyPipe client.
- The crate is GPL-3.0. The user explicitly permits GPL/AGPL options. The dependency remains opt-in, so core-only users do not inherit it.
- Web-based YouTube clients require PO tokens for stream URLs; RustyPipe delegates PO-token generation to the separate rustypipe-botguard helper. Search can work without that helper while some stream resolution paths may fail.
- RustyPipe stores client-version/deobfuscation cache and may store authentication data. musid-dli requires a dedicated storage directory; do not place that cache in the project root or share it.
- DRM-protected audio is rejected by the adapter rather than being exposed as a normal URL.

## License and integration rules

- Review direct and transitive license expressions, MSRV, target support and resolved feature tree for each optional integration.
- A Rust repository is not automatically an embeddable SDK; verify library exports and API stability.
- Do not bundle a complete app or service to obtain one provider.
- Do not claim premium, DRM-protected or full-track access from preview/API metadata.
- Keep network search optional; offline lyrics parsing must not pull HTTP features.
- Record exact version/commit, license, MSRV, dependency tree, API coverage, tests and service-side limitations for every adapter.

## Local verification still required

- cargo tree -e features and license inventory after lock generation.
- Build no-default, default and each optional feature independently.
- Cross-compile Linux/Windows/macOS.
- Compare lyrics-helper parsing/decryption against VoiceFox LRC/QRC/KRC/YRC fixtures.
- Compare ncm-api-rs and ncmapi against VoiceFox login, search, stream URL, lyrics and playlist flows.
- Run YouTube Music live search/stream tests with and without Botguard, recording rate-limit/403 behavior.
