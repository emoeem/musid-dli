# Remaining provider evaluation (Qianqian, JOOX, 5sing, Jamendo, Apple Music)

Audited: 2026-11-17, from `/media/emo/code/voicefox/source/src/{qianqian,joox,fivesing,jamendo,apple}` (read-only; VoiceFox untouched). Each verdict follows the musid-dli contract: the library returns StreamInfo URLs, does not decrypt DRM, and marks unsupported capabilities explicitly.

# Remaining provider evaluation (Qianqian, JOOX, 5sing, Jamendo, Apple Music)

Audited: 2026-11-17, from `/media/emo/code/voicefox/source/src/{qianqian,joox,fivesing,jamendo,apple}` (read-only; VoiceFox untouched). Each verdict follows the musid-dli contract: the library returns StreamInfo URLs, does not decrypt DRM, and marks unsupported capabilities explicitly.

## Implemented since the audit

- **Jamendo**: implemented and live-verified (2026-11-17). Live finding: the container endpoints (`/api/tracks`, `/api/albums`, `/api/playlists`) hang anonymously since the audit, so only `/api/search` stays wired and stream URLs are embedded in the search response; albums/playlists capabilities stay false.
- **Apple Music**: implemented and live-verified (2026-11-17). Live finding: the web bundle script marker moved from `/assets/index-legacy…` to `/assets/index~…` (token bootstrap handles all known shapes); every stream is a 30-second preview by contract.
- **5sing**: implemented and live-verified (2026-11-17). Live finding: the playlist track list is now JS-rendered (static page and anonymous API both omit it), so playlist metadata resolves anonymously and `playlist_tracks` reports Unsupported.

## Qianqian (千千音乐, 91q.com)

- Upstream: 8 files, ~1200 lines. Search/song/album/playlist/lyric/url.
- Signing: MD5 of sorted `k=v&...` pairs plus a fixed secret (`0b50b02fd0d73a9c4c8c3a781c30845f`) with appid `16073360`; the tracklink endpoint excludes `rate` from the signed text (upstream documents this quirk).
- Viability probe (2026-11-17): the site is alive (homepage 200) but the audited search paths (`/api/search/advanced`, `/wsearch/song`) answer 404 or encrypted blobs anonymously; the endpoints have moved or need additional session state.
- Effort: medium implementation, plus endpoint rediscovery of unknown size.
- Verdict: deferred. The signing recipe is portable, but without live endpoints to test against, shipping an implementation would be unverifiable. Revisit when endpoint discovery is worth a dedicated batch.

## JOOX

- Upstream: 2 files, ~714 lines. OpenJOOX v2 `search_type`, v3 playlist, JSONP `web_get_songinfo` and `web_lyric`.
- Viability probe (2026-11-17): OpenJOOX v2 answers `9007001 parameters type error` for every documented parameter shape (plain, plus country/lang), and `web_get_songinfo` returns an empty body. The anonymous surface has been closed since the audit.
- Verdict: deferred. Same reasoning as Qianqian: no live endpoint to verify against.

## 5sing (原创音乐)

- Upstream: 2 files, ~719 lines. Unsigned JSON endpoints on kugou infrastructure (`search.5sing.kugou.com/home/json`, `mobileapi.5sing.kugou.com/song/newget` etc.).
- Effort: easy (comparable to Kuwo's first slice).
- Live finding (2026-11-17): the playlist track list is JS-rendered; metadata resolves anonymously, track lists report Unsupported.
- Verdict: implemented and live-verified.

## Jamendo (CC-licensed music)

- Upstream: 1 file, 611 lines. Unsigned search/song/album/playlist endpoints with an `x-jam-call` header: `$sha1(path + nonce)*nonce~` plus a fixed `x-jam-version`; download and stream URL groups with flac/mp3 tiers.
- Value: Creative Commons catalog, globally available, no login, no DRM; the legally cleanest provider in the queue.
- Verdict: implemented and live-verified (search + streams; containers tightened server-side and honestly unsupported).

## Apple Music

- Upstream: 2 files, ~797 lines. Unsigned iTunes/amp-api search and playlist endpoints; the only playable assets are 30-second preview URLs (`attributes.previews[0].url`). Full tracks are DRM-protected and require external decryption tools, which musid-dli will never ship.
- Contract requirement: the capability set must be marked preview-only (no lossless, no full-track playback), and every StreamInfo handed out is a preview. The upstream module ships a `PREVIEW_HINT` constant for exactly this reason.
- Verdict: implemented and live-verified (previews, lyrics, containers, links).

## Recommended order

1. ~~Jamendo~~ done.
2. ~~Apple Music~~ done.
3. ~~5sing~~ done.
4. Qianqian and JOOX: deferred pending endpoint rediscovery or a service revival; re-probe before investing.
