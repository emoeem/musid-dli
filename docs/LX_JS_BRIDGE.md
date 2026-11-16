# LX Music JS custom-source bridge (design)

Status: **design + protocol scaffold**; the JS engine wiring is a separate, feature-gated future step. This document fixes the contract so any host (VoiceFox included) can implement it without musid-dli growing a JS runtime dependency.

## What LX Music custom sources are

LX Music custom sources are user-supplied JavaScript files implementing a well-known protocol: each source registers handlers for `init`, and per-source `musicUrl`/`lyric`/`pic` requests over an internal API (`@/utils/musicSdk`-style). VoiceFox already runs them through a hardened Node.js child process (`js/engine.rs`: `node --permission`, wrapper script in the cache dir, JSON-lines over stdio).

## Design goals

1. **No JS engine in the default build.** The bridge lives behind an optional feature (`lx-js-bridge`) and speaks to any engine process through the same JSON-lines protocol. musid-dli's core stays free of Node/rquickjs/boa.
2. **No implicit trust.** A user script is untrusted code: the engine process must run with filesystem read access limited to the script itself and its declared data dir, no env inheritance, and network access is the script's whole point (so it cannot be disabled, but the host never passes session cookies into the engine).
3. **musid-dli stays the contract owner.** The bridge implements musid-dli's own `MusicProvider` traits (`TrackSearch`, `StreamResolver`, `LyricsProvider`) by translating to/from the LX protocol messages. Host apps see ordinary Rust providers.

## Protocol (JSON-lines over stdio)

Each request is one JSON object per line; each response is one JSON object per line, in order.

```text
→ {"id": 1, "cmd": "init", "params": {"sourceId": "my-source", "version": "1.0"}}
← {"id": 1, "ok": true}
→ {"id": 2, "cmd": "musicUrl", "params": {"songId": "...", "quality": "320k"}}
← {"id": 2, "ok": true, "result": {"url": "https://...", "headers": {}}}
← {"id": 2, "ok": false, "error": "vip required"}
→ {"id": 3, "cmd": "lyric", "params": {"songId": "..."}}
← {"id": 3, "ok": true, "result": {"lyric": "[00:00]...", "tlyric": null}}
→ {"id": 4, "cmd": "search", "params": {"keyword": "...", "type": "song", "page": 1, "limit": 20}}
← {"id": 4, "ok": true, "result": {"items": [...], "total": 100}}
```

## Security boundary

| Concern | Decision |
|---|---|
| Engine | Out of scope for musid-dli: the host picks Node, rquickjs, boa, or any sandbox. musid-dli defines the transport contract only. |
| Filesystem | Engine must run with `--permission`-style restrictions (VoiceFox's `js/engine.rs` already does this) |
| Secrets | Session cookies never enter the engine; JS sources are anonymous by contract |
| Results | A bridge response is wrapped like any provider error; engine failures are `Error::Provider`, never panics |
| Auth | The bridge never registers an `AuthenticationProvider`: LX custom sources have no login flow |

## Trait mapping

| musid-dli capability | LX cmd | Notes |
|---|---|---|
| `TrackSearch::search` | `search` | keyword/page/limit passthrough |
| `StreamResolver::resolve` | `musicUrl` | quality strings follow LX conventions (`128k`/`320k`/`flac`) |
| `LyricsProvider::lyrics` | `lyric` | plain LRC text; no timing data in v1 |
| `AuthenticationProvider` | not exposed | no login flow in LX custom sources |
| charts/albums/playlists | not exposed | LX custom sources are music-only |

## Scaffold

`src/lx_js.rs` defines the request/response envelope types and a `JsBridgeEngine` trait (send a request line, receive a response line) so the host can plug its own engine. The provider wrappers translate errors and map unsupported commands to `Error::Unsupported`.

## Non-goals

- Bundling a JS runtime into musid-dli.
- Supporting the full LX Music desktop plugin API (toasts, settings UI, etc.).
- Caching decrypted media locally (see Soda's rejection reason in PROVIDER_EVALUATION.md).
