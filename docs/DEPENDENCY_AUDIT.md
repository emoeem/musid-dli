# Third-party dependency audit (P1)

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
