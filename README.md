# musid-dli

[![CI](https://github.com/emoeem/musid-dli/actions/workflows/ci.yml/badge.svg)](https://github.com/emoeem/musid-dli/actions/workflows/ci.yml)

Independent Rust music metadata and provider aggregation library.

musid-dli is not an audio player or decoder. It returns stream metadata (URL, headers, quality, expiry and fallback URLs) to the host application. VoiceFox continues to use lx-player + libmpv2.

## Boundaries

- Online providers: search, stream resolution, lyrics, playlists, albums, artists, charts, links and authentication.
- Local library: directory scanning and metadata; separate from online providers.
- Format handlers: local containers such as NCM; opt-in and separately audited.
- Download: separate optional service; keep VoiceFox downloader until parity.
- Playback/DSP/output: out of scope.

## Current bootstrap

The initial public models, capability-specific provider traits, registry, concurrent search aggregation, conservative deduplication, cross-provider match ranking, ordered stream fallback with circuit-breaker health tracking, quality-scoped expiry-aware stream cache, secure session persistence, separate local-library and container-handler interfaces, and lyrics-helper normalization are in place. An optional RustyPipe YouTube Music adapter compiles for search and stream URL resolution. An optional NetEase adapter now implements search, quality-fallback stream resolution, lyrics, cookie-backed session status, QR login (key generation + status polling) and registry registration. Playlists (categories, hot lists, search, detail, tracks), albums, artists, charts and share-link resolution are implemented and live-verified; user playlists require an authenticated session. An optional QQ Music adapter implements signed search, song detail, stream URL resolution (vkey ladder), lyrics, playlists (categories, lists, search, detail, user lists), albums, charts, song/playlist/album link resolution, QQ and WeChat scan login and session status; playable stream URLs require a logged-in session cookie. Optional Kugou and Bilibili adapters implement search, lyrics/word timing (KRC), QR scan login and stream resolution; Kugou adds playlists, charts, albums, artists and link resolution and Bilibili adds WBI-signed DASH audio plus favorites. Provider evaluation verdicts for the remaining platforms live in docs/PROVIDER_EVALUATION.md. An optional Kuwo adapter implements anonymous search, lyrics (yeelion-XOR plus GB18030), mobile stream resolution with paid-song placeholder rejection and web-signed lossless streams. An optional Migu adapter implements signed search, TEA-decrypted MRC word-timed lyrics, anonymous stream resolution with served-size quality correction and playlists. An optional Jamendo adapter (CC catalog) implements signed search with embedded stream URLs. An optional Apple Music adapter implements search, 30-second preview streams (full tracks are DRM), plain-text lyrics and containers. An optional 5sing adapter implements search, three-tier stream URLs and LRC lyrics. The LX Music JS bridge is designed and scaffolded (feature `lx-js-bridge`, docs/LX_JS_BRIDGE.md). The human test checklist covers the four QR login flows (docs/HUMAN_TEST_CHECKLIST.md). Remaining VoiceFox providers: Qianqian/JOOX (deferred, endpoints closed), Soda (deferred, CENC model), plus the LX JS engine wiring (host-provided).

## Cargo features

- default = local + lyrics + session-file
- local = lofty metadata + walkdir scanning
- lyrics = offline parsing/generation/decryption facade; network search disabled
- lyrics-search = opt-in concurrent lyrics search across Netease, QQ, Kugou, Soda Music, LRCLIB and AMLL TTML DB
- session-file = atomic, permission-restricted provider session persistence
- source-youtube-music = optional RustyPipe provider (GPL-3.0); requires a dedicated cache directory; use RustyPipeProvider::new_with_botguard when the helper is not on PATH
- source-netease = optional native NetEase provider (AES EAPI search, stream URL resolution with quality fallback, lyrics, cookie session, QR login, playlists, albums, artists, charts and link resolution)
- source-qqmusic = optional native QQ Music provider (zzc-signed lyrics, unsigned musicu.fcg search/detail/vkey resolution, M500-M800-Q000-F000-AI00 quality ladder, playlists, albums, charts, link parsing, QQ scan login, cookie session status; WeChat login and QRC decryption pending)
- source-kugou = optional native Kugou provider (unsigned search, KRC lyrics decryption with word timing and language metadata, QR scan login, login-gated signed v5 stream URLs; collections pending)
- source-kuwo = optional native Kuwo provider (anonymous search, N_MINFO quality parsing, mobile 128k/320k stream URLs with placeholder rejection, yeelion-XOR GB18030 lyrics; web session signing and lossless tiers pending)
- source-migu = optional native Migu provider (signed search, MRC word-timed lyrics via TEA containers, LRC/TRC fallbacks, anonymous stream resolution with obfuscated H5 strategy endpoint; login and collections pending)
- source-bilibili = optional native Bilibili provider (WBI-signed search and DASH audio, BV/av reference expansion, QR login, login-gated favorites; no lyric API exists)
- source-jamendo = optional native Jamendo provider (CC catalog, x-jam-call SHA1 signing, search with embedded stream URLs, lossless-first; container endpoints hang anonymously and stay unsupported)
- source-apple = optional native Apple Music provider (JWT bootstrap from the web bundle, signed search, 30-second preview streams only: full tracks are DRM, plain-text lyrics, album/playlist containers)
- source-fivesing = optional native 5sing provider (unsigned search, three-tier stream URLs with backups, LRC lyrics via song metadata, playlist metadata; track lists are JS-rendered and unsupported)
- download is intentionally not exposed yet; VoiceFox's existing engine remains in place until a reusable API is designed

## NetEase provider quick start

```rust,no_run
use musid_dli::{source_netease::NeteaseProvider, ProviderRegistry};

let provider = NeteaseProvider::new();
let mut registry = ProviderRegistry::new();
provider.register(&mut registry);
```

Run a live, read-only connectivity smoke test per module with `cargo run --features source-netease --example netease_smoke [MODULE...]`. Modules: `search track stream lyrics playlist chart album artist link auth user` (default: `search track stream lyrics`). The smoke test never downloads or plays audio; `auth` only generates a QR key and polls the status once, and `user` skips gracefully when no session cookie is configured. QQ Music has the equivalent `qqmusic_smoke` (`search track stream lyrics playlist chart album link auth wechat user`) behind `source-qqmusic`; anonymous runs are refused playable stream URLs by QQ itself (module code 10006), and `auth`/`wechat` render the QR images as data URLs then poll once. Kugou has `kugou_smoke` (`search lyrics stream auth user`) behind `source-kugou`; the stream module reports SKIPPED for anonymous sessions because Kugou serves no URLs without login. Kuwo has `kuwo_smoke` (`search lyrics stream`) behind `source-kuwo`; the stream module walks the first search hits because paid tracks resolve to rejected placeholders. Migu has `migu_smoke` (`search lyrics stream`) behind `source-migu`; anonymously only PQ (128k) is served and the resolved quality reflects that. Jamendo and Apple Music each have their own smoke (`jamendo_smoke`, `apple_smoke`); 5sing has `fivesing_smoke` (`search stream lyrics playlist link`).

Required checks: cargo build --no-default-features, cargo test --no-default-features, cargo test.

See docs/VOICEFOX_AUDIT.md, docs/DEPENDENCY_AUDIT.md, docs/MIGRATION.md, docs/SOURCE_QQ_MUSIC_AUDIT.md, docs/PROVIDER_EVALUATION.md, docs/HUMAN_TEST_CHECKLIST.md and docs/IMPLEMENTATION_STATUS.md.
