# musid-dli handoff document

Date: 2026-11-17. Written for the next engineer (human or AI) picking up this codebase.

## What this project is

Rust library aggregating multiple music-platform APIs behind a unified `MusicProvider` trait. It returns stream URLs, metadata, lyrics and container listings to a host application. It does not decode or play audio, does not handle DRM, and does not bundle a JS runtime.

Independent project at `/media/emo/code/musid-dli`. VoiceFox at `/media/emo/code/voicefox` is the migration reference (read-only; never modify).

## Current state (one-line summary)

9 of 12 VoiceFox providers implemented and live-verified, 2 deferred with recorded reasons, 1 pending (LX JS engine wiring). 172 unit tests, 0 failures, clippy clean, all feature combinations compile.

## Source layout

```
src/
├── lib.rs                  # feature-gated module declarations
├── models/                 # Track, Album, Artist, Playlist, Chart, StreamInfo, LyricDocument...
├── provider/traits.rs      # MusicProvider + capability traits (search/stream/lyrics/auth/etc)
├── registry.rs             # ProviderRegistry: capability-trait lookup by id
├── search.rs               # concurrent aggregation + dedup
├── resolve.rs              # ordered stream fallback + ResolvePipeline (health + cache)
├── matcher.rs              # cross-provider track matching
├── cache.rs                # quality-scoped expiry-aware stream cache
├── health.rs               # circuit breaker
├── retry.rs                # shared retry/backoff classifier (ApiCodes per provider)
├── lrc.rs                  # shared LRC parser
├── lx_js.rs                # LX Music JS bridge protocol scaffold
├── lx_js_engine.rs         # Node.js process engine (JSON-lines over stdio)
├── qr_terminal.rs          # terminal QR renderer (included via #[path] from smokes)
├── session.rs              # ProviderSession + SessionStore
├── session_file.rs         # FileSessionStore (feature session-file)
├── local.rs                # local music library (feature local)
├── lyrics.rs               # lyrics-helper facade (feature lyrics)
├── formats.rs              # format utilities
├── error.rs                # Error enum (Provider/Unsupported/NotFound/etc)
└── source_*.rs             # one file per provider
```

## Provider matrix

| Provider | Feature | Search | Lyrics | Stream (anon) | Login | Collections | Notes |
|---|---|---|---|---|---|---|---|
| netease | source-netease | yes | LRC | yes (+redirect fallback) | QR | playlists/albums/artists/charts/links | user playlists need session |
| qqmusic | source-qqmusic | yes | LRC | gated (code 10006) | QR (QQ + WeChat) | playlists/albums/charts/links | full tracks need session |
| kugou | source-kugou | yes | KRC (word timing) | login-required | QR | playlists/charts/albums/links | artist via search fallback |
| kuwo | source-kuwo | yes | yeelion-XOR LRC | yes (incl. lossless via web signing) | none (web signing is anon) | none | lossless tier via web-signed playUrl |
| migu | source-migu | yes | MRC (TEA word timing) | yes (128k only) | none (upstream lacks it) | playlists | quality corrected to served size |
| bilibili | source-bilibili | yes (BV/av refs; keyword gated) | none (no API) | DASH AAC (320k) | QR | favorites | keyword search v_voucher-gated |
| jamendo | source-jamendo | yes | none (no API) | yes (lossless-first) | none | playlists/albums (containers hang anonymously) | CC catalog |
| apple | source-apple | yes | plain text | 30s preview only | none (no API) | playlists/albums | every stream is a preview by contract |
| fivesing | source-fivesing | yes | LRC (dynamicWords) | 3-tier with backups | none (no API) | playlists (metadata; tracks JS-rendered) | original-music community |

Deferred: soda (CENC local-file model conflicts with StreamInfo contract), qianqian/joox (endpoints closed since audit, probes 2026-11-17), apple full tracks (DRM).

## Architecture conventions

1. **One file per provider**: `source_{id}.rs`, feature `source-{id}`. The lib compiles without any provider feature.
2. **Shared retry policy**: `src/retry.rs` exposes `classify_http` + `classify_api` with per-provider `ApiCodes`. Every provider request loop uses these; no bespoke retry logic.
3. **Parse functions are pure and unit-tested**: `parse_track`, `parse_playlist`, etc. take `&Value` and return `Option<T>`. Network calls are separate methods.
4. **QR login contract**: `QrAuthenticationProvider::create_qr_login` then `poll_qr_login(key)` puts the session into the provider's `SessionStore` on Success. Session cookies never leak into Debug output.
5. **Quality honesty**: `available_qualities` reflects what the API declares; resolved quality reflects what was actually served. Never claim a tier that was not verified.
6. **Honest gates**: server-side restrictions (VIP, risk control, login-required) surface as errors with the actual code and message. Never fake success.
7. **Container endpoints**: when a service's container API hangs anonymously (Jamendo), the capability stays false and the method returns `Error::Unsupported` with the reason and date.

## Key docs

| Doc | Content |
|---|---|
| docs/IMPLEMENTATION_STATUS.md | Per-provider live-verification state + remaining work |
| docs/PROVIDER_EVALUATION.md | Audit + probe results for the deferred/remaining providers |
| docs/SOURCE_QQ_MUSIC_AUDIT.md | QQ Music protocol details (signing, endpoints, quality ladder) |
| docs/VOICEFOX_AUDIT.md | Full VoiceFox source audit (pre-migration map) |
| docs/LX_JS_BRIDGE.md | LX Music JS adapter protocol contract + security boundary |
| docs/HUMAN_TEST_CHECKLIST.md | QR login flows that need a phone |
| docs/WORKFLOW.md | End-to-end build, verify, login, parity workflow |
| docs/DEPENDENCY_AUDIT.md | Dependency tree analysis per feature |

## Human test checklist (phone required)

The four QR login flows are the only remaining untested items. Full steps in `docs/HUMAN_TEST_CHECKLIST.md`; short version:

```bash
# 1. QQ Music (QQ scan)
cargo run --features source-qqmusic --example qqmusic_smoke auth
cargo run --features source-qqmusic --example qqmusic_smoke user stream

# 2. QQ Music (WeChat scan)
cargo run --features source-qqmusic --example qqmusic_smoke wechat
cargo run --features source-qqmusic --example qqmusic_smoke user stream

# 3. Kugou
cargo run --features source-kugou --example kugou_smoke auth
cargo run --features source-kugou --example kugou_smoke user stream

# 4. Bilibili
cargo run --features source-bilibili --example bilibili_smoke auth
cargo run --features source-bilibili --example bilibili_smoke search stream favorites
```

QRs render as ANSI half-blocks directly in the terminal; scan the moment the command starts (180s budget). On Success the session lands in the store and the gated modules run in the same or a subsequent invocation.

Pass criteria per platform: `poll_qr_login` reaches Success with non-empty identity; gated modules return real data instead of the documented anonymous gates (qqmusic 10006, kugou login-required, bilibili v_voucher, netease user-playlists session-required).

## Remaining work (prioritized)

1. **Phone-verified login flows** (4 QR channels above). Everything automatable is done; only the physical scan and post-login module verification remain.
2. **Kugou artists**: done via search fallback. **Albums**: done. **Legacy v3 fallback**: done. Remaining: none for kugou.
3. **Kuwo web signing**: done (lossless verified live). Remaining: none for kuwo.
4. **Migu playlists**: done (113 tracks live). Remaining: Migu user playlists need the mobile login session, which upstream does not document.
5. **Bilibili favorites**: done (login-gated). Remaining: Bilibili artist/album modules.
6. **LX Music JS adapter**: protocol scaffold + Node engine done (feature `lx-js-bridge`). Remaining: wire the engine into actual provider wrappers (`impl TrackSearch for JsBridgeProvider`) and test with a real LX custom-source file.
7. **Qianqian/JOOX**: deferred (endpoints closed). Re-probe before implementing.
8. **Soda**: deferred (CENC model). Needs a host-side decryption contract first.

## Build and test commands

```bash
cargo fmt --all -- --check          # must pass
cargo clippy --all-targets --all-features -- -D warnings  # must pass
cargo test --all-features           # 172 tests green
cargo test --no-default-features    # 19 tests green
cargo build --no-default-features   # lib compiles without any provider
./scripts/run_all_smokes.sh         # all 9 provider smokes in sequence
./scripts/run_all_smokes.sh netease kugou  # filter to specific providers
```

## Feature flags

| Feature | Pulls in | Purpose |
|---|---|---|
| default | local + lyrics + session-file | core + local lib + lyrics facade + session persistence |
| source-netease | reqwest + aes + md-5 + hex + tracing + urlencoding | NetEase adapter |
| source-qqmusic | reqwest + sha1 + base64 + hex + tracing + urlencoding | QQ Music adapter |
| source-kugou | reqwest + md-5 + base64 + flate2 + tracing + urlencoding | Kugou adapter |
| source-kuwo | reqwest + md-5 + hex + base64 + flate2 + encoding_rs + tracing + urlencoding | Kuwo adapter |
| source-migu | reqwest + md-5 + base64 + flate2 + tracing + urlencoding | Migu adapter |
| source-bilibili | reqwest + md-5 + tracing + urlencoding | Bilibili adapter |
| source-jamendo | reqwest + sha1 + hex + tracing + urlencoding | Jamendo adapter |
| source-apple | reqwest + tracing + urlencoding | Apple Music adapter |
| source-fivesing | reqwest + tracing + urlencoding | 5sing adapter |
| lx-js-bridge | (none, serde_json is non-optional) | LX Music JS bridge protocol scaffold |
| source-youtube-music | reqwest + rustypipe | YouTube Music adapter (search/stream compile; Botguard untested) |
| lyrics | lyrics-helper | lyrics normalization facade |
| local | lofty + walkdir | local music library |
| session-file | tempfile | atomic session persistence |

## Live verification log

| Date | What was verified |
|---|---|
| 2026-11-16 | NetEase full anonymous smoke (search/stream/lyrics/playlists/charts/albums/artists/links/auth/QR key) |
| 2026-11-17 | QQ anonymous smoke (search/track/lyrics/playlist/chart/album/link/QR-auth/QR-WeChat); stream gated 10006 |
| 2026-11-17 | Kugou anonymous smoke (search/lyrics-KRC/playlist-194-tracks/charts-TOP500/albums/links/QR-auth); stream login-gated |
| 2026-11-17 | Kuwo anonymous smoke (search/lyrics-63-lines/stream-Lossless-Flac via web signing) |
| 2026-11-17 | Migu anonymous smoke (search/lyrics-MRC/stream-128k/playlists-113-tracks) |
| 2026-11-17 | Bilibili anonymous smoke (BV-reference/DASH-320k/links/QR-auth; keyword search v_voucher-gated) |
| 2026-11-17 | Jamendo anonymous smoke (search/stream/playlist-metadata; containers hang anonymously) |
| 2026-11-17 | Apple Music anonymous smoke (search/preview-stream/lyrics/playlist-50-tracks/album-11-tracks/links) |
| 2026-11-17 | 5sing anonymous smoke (search/stream-320k/lyrics-64-LRC/playlist-metadata/links; track-lists JS-rendered) |
| 2026-11-17 | All four QR channels verified through first poll (Waiting); phone scan pending |
| 2026-11-17 | QR renderer fixed: inverted colors on dark terminals blocked phone scans (bilibili scan attempt stuck on Waiting); explicit black-on-white ANSI + PNG fallback shipped, round-trip test added; phone rescan still pending |
| 2026-11-17 | Bilibili QR phone scan confirmed working by the user; musid-dli v0.1.0 published to crates.io (docs.rs green), GitHub repo emoeem/musid-dli created and pushed |

## Gotchas for the next engineer

1. **Bilibili nav returns code -101 anonymously** but `data.wbi_img` still carries valid WBI keys; extract them without checking `code` (source_bilibili.rs documents this).
2. **Jamendo container endpoints hang anonymously** (SSL EOF / timeout) since some 2026 change; only `/api/search` works. Stream URLs are embedded in search results.
3. **Apple Music web bundle marker** moved from `/assets/index-legacy` to `/assets/index~`; `script_path` tries all known shapes.
4. **Migu H5 strategy endpoint** needs a mobile playing-page Referer + `channel: 0146921` header, absent from the upstream audit.
5. **Kuwo mobile endpoint serves 11-second placeholders** for paid tracks; always verify `rid` + `duration` match, never trust the response blindly.
6. **Kuwo media CDN 403s** without the `okhttp/3.10.0` User-Agent header on the stream request.
7. **Kugou playlist detail is an HTML page** with embedded JS data (`global.data = [...]`); the assignment may be spaced (`= [`), so the extractor skips whitespace.
8. **QQ Music search responses use two field-name shapes** (`baseInfo.simpleSongData` for new, flat for legacy); `parse_track` handles both.
9. **Migu anonymously serves only PQ (128k)** regardless of toneFlag; the resolved quality is corrected against the actually served file size, never the metadata claim.
10. **5sing track ids embed the song type** (`{id}|{yc|fc|bz}`) because the same numeric id can exist as three different songs.
11. **Terminal QRs need explicit ANSI colors**: painting dark modules with the default foreground color inverts the code on dark-themed terminals; phone scanners (the Bilibili in-app scanner included) silently fail and the poll stays Waiting for the full 180s budget. `qr_terminal::render` now pins SGR 30/107 (black on bright white) and `render_png` writes a PNG fallback wired into the bilibili/kugou/qqmusic smokes; the decode round-trip regression test lives in `tests/qr_terminal_png.rs`.

## Git and publishing

The project is initialized on branch `main` and published. GitHub: https://github.com/emoeem/musid-dli (public). crates.io: `musid-dli` v0.1.0 (2026-11-17), docs.rs build green. Publishing metadata lives in the `[package]` section (repository/readme/keywords/categories). Registry tokens are never stored in the repo or docs; release commits need explicit user instruction.
