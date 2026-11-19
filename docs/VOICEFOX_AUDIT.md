# VoiceFox P0 audit

Audit date: 2026-11-16. Scope: read-only inspection of /media/emo/code/voicefox. The worktree was clean at audit start (main aligned with origin/main).

## Workspace

VoiceFox is Rust 2024, version 0.8.1, MIT. Crates:
- lx-core: domain models, MusicSource/player/lyric traits, config, keybindings and cross-source sync.
- lx-source: online providers, local source, JS source engine, SourceManager, sessions, networking, cover cache and sync adapters.
- lx-player: in-process libmpv2 playback.
- lx-lyric: lyric fetch facade, LRC/QRC/YRC parsing and presentation service.
- voicefox-application: application services.
- voicefox-runtime: runtime composition and platform services.
- app: Ratatui TUI, configuration, queue, download manager/engine, local music UI, login and playlists.
- ffi and test: integration targets.

## Provider inventory

12 built-in online providers, local library and dynamic LX Music JavaScript sources exist.

| Provider | Existing code | Current behavior / caveat |
|---|---|---|
| Kuwo | source/src/kw | Search, URL, lyrics, cover, artist/album, playlists, charts, link parsing |
| Kugou | source/src/kg | Above + QR login and personal playlists |
| QQ Music | source/src/tx | Above + QQ/WeChat QR login and personal playlists |
| Netease | source/src/wy | Above + QR login and personal playlists |
| Migu | source/src/mg | Search, URL, lyrics, cover, album (derived track list; mg/ has no artist function), playlists, charts, links |
| Bilibili | source/src/bili | Search, audio stream, cover, creator, playlists, charts, links, QR login; no lyrics |
| Qianqian | source/src/qianqian | Search, URL, lyrics, cover, artist/album, playlists, links |
| JOOX | source/src/joox | Search, URL, lyrics, cover, album, playlist search/detail, links |
| 5sing | source/src/fivesing | Search, URL, lyrics, cover, playlist search/detail, links |
| Jamendo | source/src/jamendo | Search, URL, cover, album, playlists, links; no lyrics |
| Apple Music | source/src/apple | Search, preview URL, lyrics, cover, album/playlist, links; preview-only |
| Soda Music | source/src/soda | Search, URL, lyrics, cover, playlist search/detail, links |
| Local | source/src/local | Incremental scan, tags, artwork, sidecar lyrics, CUE, duplicate/missing/corrupt diagnostics |
| LX Music JS | source/src/js | Dynamic scripts, multiple providers, search/URL/lyrics/cover, origin and fallback tracking |

The canonical feature matrix is VoiceFox docs/USER_GUIDE.md. Do not infer capabilities: release notes corrected false lyric capability declarations for Bilibili and Jamendo.

## Existing model/service mapping

| VoiceFox | musid-dli |
|---|---|
| SongInfo | Track + provider-scoped TrackId |
| SongUrl | StreamInfo; preserve headers, fallback URLs, size, MD5 and bounded Range |
| ParsedLink | ResolvedLink |
| SourceCapabilities | explicit capability traits/declarations |
| MusicSource | TrackSearch, StreamResolver, LyricsProvider, CollectionProvider, AlbumProvider, ArtistProvider, ChartProvider, LinkResolver, AuthenticationProvider |
| SourceManager | ProviderRegistry + search/resolve orchestration, health, cache and fallback |
| SourceSession / SessionStore | ProviderSession + secure persistent backend |
| LocalSource/scanner/metadata | LocalLibrary, separate from online providers |
| app/src/download | optional DownloadEngine later; keep current downloader |
| lx-lyric | LyricsProvider adapter + parser facade |
| lx-player | unchanged libmpv2 playback |
| source/src/sync.rs + core sync | keep in VoiceFox runtime initially |

## Risks and constraints

1. MusicSource is a large trait with defaults that can disguise unsupported capabilities as empty success.
2. SourceId is a closed enum; public library IDs need extensible provider-scoped strings.
3. SongInfo mixes online/local fields; JS sources currently use SourceId::Local plus extra["source"].
4. lx-source mixes online providers, local indexing, JS runtime, sessions and orchestration.
5. Download code lives in app, not lx-source. Do not delete it during extraction.
6. Session persistence includes atomic writes, 0600 permissions, provider-specific cookie filtering and legacy Bilibili migration. An in-memory abstraction is not production persistence.
7. lx-core depends on Crossterm; musid-dli must own its public models and avoid lx-core dependency.
8. Apple Music URLs are previews, not full-track entitlements.

## Baseline

Before edits: cargo test -p lx-core -p lx-source -p lx-lyric on Rust/Cargo 1.99.1. lx-core: 43 passed, 0 failed. lx-lyric: 19 passed, 0 failed. lx-source unit tests: 220 discovered, 218 passed, 2 ignored (one real-library fixture and one live Netease URL probe). lx-source integration tests: js_user_api 4 passed; network_smoke 5 ignored; wy_endpoint_probe 2 ignored because they require live services/login. No failing test was reported. VoiceFox remained unmodified.
