# VoiceFox migration map

## Hard boundary

Playback remains lx-player + libmpv2. musid-dli returns StreamInfo only. Do not add Symphonia, Rodio, CPAL or another playback backend in this migration.

## Incremental mapping

- SongInfo -> Track: explicit conversion preserving provider IDs, metadata, duration, quality, ISRC and extras.
- NetEase first migration batch: search, EAPI crypto, quality ladder, official stream URL metadata, validated public redirect fallback, LRC lyrics, cookie-backed session status, QR key creation/polling and retry/backoff now live under the optional `source-netease` feature. Phone-confirmed QR login has not been smoke-tested; collection APIs, chart APIs and link parsing remain outstanding.
- SongUrl -> StreamInfo: preserve URL headers, fallback URLs, size/checksum, expiry and bounded Range constraints.
- MusicSource -> capability traits: implement only verified capabilities.
- SourceManager -> ProviderRegistry + aggregators: shadow-run and compare before switching; use cross-provider match ranking before stream fallback.
- SourceSession/SessionStore -> ProviderSession + secure persistence: preserve cookie filtering, atomic writes, 0600 permissions and legacy migration.
- LocalSource -> LocalLibrary: separate local files from online providers.
- LX Music JS -> dynamic provider adapter: preserve script protocol, loader, origin order and fallback.
- lx-lyric -> LyricsProvider/parser facade: preserve line/word timing, translation and romanization.
- app download engine -> optional DownloadEngine: retain until parity.
- lx-player -> unchanged.

## Rollout gates

1. Record baseline tests and capability matrix.
2. Run musid-dli in shadow mode beside lx-source.
3. Port one provider at a time with fixture, auth/session, online smoke and playback hand-off tests.
4. Compare aggregation, dedup, fallback, rate limits, errors and latency.
5. Switch one provider behind an opt-in flag; keep rollback.
6. Migrate JS, lyrics, local library and download separately.
7. Remove old code only after all consumers and tests pass and explicit approval is given.

## Deferred

Secure persistent session backend/keyring; provider health/circuit breaker; expiry-aware stream cache; collection/chart/link pagination; download resume/integrity/tagging; NCM handler; CUE adapter; Windows/macOS builds.
