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
- Verdict (2026-11-17): deferred. The signing recipe is portable, but without live endpoints to test against, shipping an implementation would be unverifiable. Revisit when endpoint discovery is worth a dedicated batch.
- **Re-probe and resolution (2026-11-19, anonymous read-only GETs, ≤3 per endpoint)**: the audited `/v1/*` surface (the one source/src/qianqian/ actually calls, not the 2026-11-17 `/api/*` paths) is **revived**. Server-accepted anonymously: song search (`type=1`), album search (`type=3`), playlist category directory, category lists, playlist detail (20 tracks for id 309558), song info (with a static-CDN `.lrc` URL) and album info. Two server-side changes shaped the port: tracklink now rejects the upstream recipe that excludes `rate` from the signed text (`errno 22001 签名错误:sign无效`) and accepts `rate` signed normally; `/v1/album/info` trackList rows carry `assetId` with no `TSID` key, so the upstream track parser drops every row (musid-dli extends the id probe, documented). One face is dead live: playlist keyword search (`type=6`) answers `errno 23001 不被支持的搜索类型(10000)`; the port keeps upstream parity (refusal parses to an empty result). VIP tracks (`isVip:1`) answer path-less tracklink metadata, so anonymous stream resolution exhausts into `NotFound` for paid tracks while free tracks (`isVip:0`) serve full files with size/md5/expireTime.
- Verdict (2026-11-19): **implemented and live-verified** (partial): `src/source_qianqian.rs` behind `source-qianqian`, offline fixtures in `tests/fixtures/qianqian` (trimmed live captures plus formula-derived signature goldens, see the fixture README), smoke `examples/qianqian_smoke.rs` (search/stream/lyrics/playlist/album/link all OK anonymously; the optional `playlist_search` module records the live refusal). Artists stay unimplemented: the upstream `artist: true` flag is phantom (no artist function in the 8 files).

## JOOX

- Upstream: 2 files, ~714 lines. OpenJOOX v2 `search_type`, v3 playlist, JSONP `web_get_songinfo` and `web_lyric`.
- Viability probe (2026-11-17): OpenJOOX v2 answers `9007001 parameters type error` for every documented parameter shape (plain, plus country/lang), and `web_get_songinfo` returns an empty body. The anonymous surface has been closed since the audit.
- Verdict (2026-11-17): deferred. Same reasoning as Qianqian: no live endpoint to verify against.
- **Re-probe and resolution (2026-11-19, anonymous read-only GETs, ≤3 per endpoint, no credentials)**: the service is **partially revived**. Live-accepted anonymously: OpenJOOX v2 song search (`type=0`, 30 real tracks for 晴天, envelope `error_code 0`, free tracks `vip_flag 0 is_playable true` alongside VIP ones; only `type=0` works, `type=4` still answers the recorded `9007001 parameters type error`) and `web_lyric` (JSONP `MusicJsonCallback({code:0,...})` with the **`lyric` field now base64-encoded**, a live server change vs upstream api.rs:350-354 which reads plain text; 69 LRC lines decode for the probe track). Still dead anonymously: `web_get_songinfo` answers HTTP 404 with an empty body (stream/track-detail face) and `/openjoox/v3/playlist` answers nginx 404 on the upstream cache host (403 on the api host; `/openjoox/v2/playlist` answers nginx 404 too), so the playlist/album container faces and song-link resolution are unusable anonymously. The live search body carries **no `section_list`**, so the upstream playlist search face parses to an empty result (upstream parity, never an error).
- Verdict (2026-11-19): **implemented** (partial, live status per face): `src/source_joox.rs` behind `source-joox`, offline fixtures in `tests/fixtures/joox` (trimmed live captures of the search/lyric bodies plus the verbatim nginx 404 of the dead container endpoint and clearly marked schema-derived songinfo vectors, see the fixture README), smoke `examples/joox_smoke.rs` (search/lyrics/link-routing all OK anonymously on 2026-11-19; the optional `playlist_search`/`stream`/`playlist`/`album` modules record the live refusals). All six upstream faces (search/stream/lyrics/playlist+playlist_search/album/link) are ported at upstream parity; the dead-endpoint faces surface the refusal as `Error::Network` instead of pretending success. The upstream `JOOX_COOKIE` env-var injection maps onto the musid-dli SessionStore (cookies under the `joox` provider id ride the OpenJOOX requests).

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
4. ~~Qianqian~~ done (2026-11-19: the service revived on the audited `/v1/*` surface and the port landed, see above).
5. ~~JOOX~~ done (2026-11-19: the service partially revived on the audited surface and the port landed with per-face live status, see above).

## Independent dual-source re-probe of record (M4 full-check batch, 2026-11-19)

Both container sources were re-probed anonymously a second time, independent of the migration probes recorded in the sections above, as part of the M4 full-check batch; both revival verdicts were re-confirmed and no verdict changed. The evidence below is the probe record of that batch.

**Qianqian: revived.** Command: `python3 -I /tmp/qq_scripts/qq_probe.py`, a probe script replicating the voicefox signing recipe (MD5 over the parameters plus timestamp sorted by key ascending, joined as `k=v&` and suffixed with the fixed secret, appid `16073360`, the UA and `Referer: https://music.91q.com/player` of voicefox crypto.rs:11-15 and crypto.rs:18-59). One GET per endpoint, no credentials, 15 s timeout, 5 requests total (each within the 3-per-endpoint budget), gzip bodies decompressed by magic number, raw responses kept under `/tmp/qq_probe/`; the endpoint inventory came from the voicefox source (song.rs:88 `GET /v1/search` with word/type/pageNo/pageSize/appid, parse.rs:96-97 `GET /v1/song/info` with TSID/appid, url.rs:46 `GET /v1/song/tracklink`, song.rs:49-70 forced-gzip handling). Results:

- `GET https://music.91q.com/` answered 200 text/html (25.6 KB, gzip): the site is alive.
- `GET https://music.91q.com/v1/search?appid=16073360&pageNo=1&pageSize=10&timestamp=1795045243&type=1&word=%E5%91%A8%E6%9D%B0%E4%BC%A6&sign=f433b77e92958f23621c0cd1504761a3` (word=周杰伦, a 134-character signed query string) answered 200 application/json in 227 ms: `{state:true, errno:22000, errmsg:"", elapsed_time, ip, data:{total, haveMore, searchMode, typeTrack[7], typeAlbum, typeArtist, typeResources}}`, and `typeTrack[0]` carries the complete 29-key song structure the voicefox parse needs (TSID/title/artist[{artistCode,gender,name}]/albumTitle/albumAssetCode/duration/pic/rateFileInfo/lyric/isVip/releaseDate); sample row TSID `T10064987120`, 菊花台, 中国武警合唱团, 辉煌之声.3 传奇, duration 341, `rateFileInfo` covering the 64/128/320/3000 tiers including the lossless 3000 tier.
- `GET https://music.91q.com/v1/song/info?TSID=T10064987120&appid=16073360&timestamp=1795045243&sign=8b8df9c1e8d016ba79afa8bd166eb66e` (TSID taken from the search result above, not a placeholder) answered 200 application/json in 198 ms: `{state:true, errno:22000, data:[1 item]}` with `data[0]` carrying the same 29 keys as the search row, so the song-detail endpoint parses into the same song structure.
- Dead-path re-check: `GET /api/search/advanced?word=test` and `GET /wsearch/song?word=test` both answered 404 with an HTML error page, unchanged from the 2026-11-17 record.

Conclusion: revived, consistent with the 2026-11-19 migration re-probe recorded in the Qianqian section above. The 2026-11-17 deferral was specific to the old `/api/*` and `/wsearch/*` path family, while the signed `/v1/*` endpoint family that the voicefox source actually calls answers anonymously with no session state and a still-valid signature. Two scope notes of record: the responses force gzip regardless of `Accept-Encoding` (verified live with `identity`: the wire body still carries `Content-Encoding: gzip` and needs magic-number decompression, matching voicefox song.rs:49-70), and the tracklink endpoint (voicefox url.rs:46) was outside this probe's budget: only the search and song-info endpoints were verified here, and the tracklink live findings recorded in the Qianqian section belong to the migration probe.

**JOOX: revived (search and lyrics), playback direct links still dead.** Method: anonymous curl GETs, no credentials or cookies, upstream-style non-credential identity headers (UA + `X-Forwarded-For: 203.0.113.1` + Referer) to reproduce the recorded request shapes, `-m 15` timeout, 2-3 requests per endpoint with one parameter shape each, 10 requests total, raw responses kept under `/tmp/joox_probe/`; the endpoint inventory came from voicefox joox/api.rs:21-24 (OPENJOOX_SEARCH/OPENJOOX_PLAYLIST/SONG_INFO_API/LYRIC_API, parameter shapes at api.rs:120-121,146,172,262-269,334-337; joox/mod.rs holds only www.joox.com front-end links). Results:

- `GET https://cache.api.joox.com/openjoox/v2/search_type?country=sg&lang=zh_cn&key=%E5%91%A8%E6%9D%B0%E4%BC%A6&type=0` answered 200 application/json (17789 B): `{"next_page":1,"has_more":true,"tracks":[[...]],"error_code":0,"error":""}`, tracks nested array-of-arrays carrying the full parse field set (id `boNzgrt_gYV99EjfVOfw8Q==`, name 蒲公英的約定, album_id/album_name, artist_list[].name, play_duration 247, images[].url under image.joox.com, `is_playable` false for the sampled track, vip_flag), matching voicefox api.rs:195-217 parse_song_info with the api.rs:150-168 unwrapping; `python -I json.load` parses the body.
- The second shape `country=hk&lang=zh_TW&key=hello&type=0&sin=0&ein=30` also answered 200 (17851 B, same shape, Hello/OMFG). The recorded `9007001 parameters type error` no longer applies to the search face.
- `GET https://cache.api.joox.com/openjoox/v3/playlist` answered 404 with a 146 B nginx HTML page (content-type nominally application/json) in both shapes (literal `id=test`, then a base64-shaped id); no real playlist id was available, so the container endpoint is recorded dead on 2 requests.
- `web_get_songinfo` answered 404 with a 0 B body in all three shapes (the literal `test` songid, a base64-shaped songid, then the real song id `boNzgrt_gYV99EjfVOfw8Q==` from the search result), so the kbps_map/r320Url direct-link face is not restored and anonymous playback stays unresolved.
- `web_lyric` is alive: bad params answer `MusicJsonCallback({"code":-20004,"msg":"parse param error"})` inside a 200 shell (61 B), and the real music id answered 200 (1663 B) with `code:0` and a base64-encoded `lyric` field that decodes to LRC (`[ti:蒲公英的約定] / [ar:周杰倫] / [al:我很忙]`), the same base64 live change the migration probe recorded.

Conclusion: revived, consistent with the 2026-11-19 migration verdict recorded in the JOOX section above: the search face answers 200 with parseable song structures and `web_lyric` returns valid content, while `web_get_songinfo` and the OpenJOOX v3 playlist face stay dead, so anonymous playback direct links (songinfo/kbps_map) remain unresolved. If a follow-up integration is ever needed: search and lyrics can be wired, stream direct links need a different entry point.
