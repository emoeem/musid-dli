# QQ Music upstream audit (VoiceFox, pre-migration research)

Audited: 2026-11-16. Source: `/media/emo/code/voicefox/source/src/tx/` (crate `lx-source`). Read-only audit; VoiceFox was not modified. All findings must be re-verified against the current VoiceFox tree before implementation.

Provider name: QQ Music (VoiceFox source id `tx`, display name `QQ`).

## Migration survey record

| Item | Finding |
|---|---|
| Upstream directory | `voicefox/source/src/tx/` (mod, search, url, lyric, playlist, leaderboard, login, session, parse, crypto) |
| Main source files | `tx/mod.rs` (TxSource), `tx/search.rs`, `tx/url.rs`, `tx/lyric.rs`, `tx/playlist.rs`, `tx/leaderboard.rs`, `tx/login.rs`, `tx/session.rs`, `tx/parse.rs`, `tx/crypto.rs` |
| Registration | `SourceManager::new` registers `Arc::new(TxSource::new())` (`manager.rs:133`); sync adapter `TxSyncProvider` (`sync.rs:388`) |
| Upstream API origin | Protocol attributed to lx-music `musicSdk/tx/` (module doc, `tx/mod.rs:3`) |
| Search capability | Yes; unsigned GET, risk-control quirk on HTTP 500 |
| Song detail | Yes; unsigned GET by songmid or numeric songid |
| Stream URL | Yes; unsigned POST `musicu.fcg` (`vkey.GetVkeyServer/CgiGetVkey`) |
| Quality support | Low128, High320, Flac, Flac24 (hires) |
| Lyrics | Yes; base64 LRC + translation + romanization + QRC; single signed endpoint |
| Playlists | Yes; plaza/detail/search/categories/user/favorites and write APIs (create/add/remove) |
| Album | Yes; `AlbumSongList` module |
| Artist | No dedicated endpoint; trait default searches by name and filters by singer |
| Charts | Yes; HTML scrape for board discovery + JSON fallback; `ToplistInfoServer/GetDetail` for entries |
| Link parsing | songDetail/{mid}, ?songmid=, ?songid=, playlist/{disstid}, taoge/{disstid}, ?id=/?disstid=/?dissid= |
| Auth | QQ QR + WeChat QR; cookie-based; no token refresh |
| Cookie needs | identity cookie (`uin`/`wxuin`) plus key cookie (`qqmusic_key`/`qm_keyst`); anonymous requests get empty `sip` and no playable URL |
| Region limits | Mainland-oriented CDN; risk control on unfamiliar IPs; szu./shu. fallback hosts for the login exchange |
| Dependencies | reqwest, serde_json, sha1, base64, urlencoding, regex; no JS runtime for this provider |
| Test coverage (upstream) | Unit: zzc_sign golden vector, search 500-shape, filename mapping, link shapes, playlist parse shapes, login flows, session predicate. Live tests are `#[ignore]`-gated smoke scripts without assertions |
| License | Repo is MIT (`voicefox/LICENSE`); tx files carry no SPDX headers, only module docs |
| Maintenance status | Active in VoiceFox; constants are lx-music-era snapshots |
| Migration risk | Medium: protocol surface small and signable in pure Rust; cookie lifecycle, risk control, HTML scraping and hardcoded client versions are the fragile parts |

## Key protocol facts (to be verified again at implementation time)

### Search (unsigned)

`GET https://c.y.qq.com/soso/fcgi-bin/client_search_cp?p={page}&n={limit}&w={keyword}&format=json&new_json=1&cr=1&aggr=1&lossless=1`, `Referer: https://y.qq.com/`. Response: `code==0`, `data.song.totalnum`, `data.song.list[]`. Per item: `mid`, numeric `id`, `title`, `singer[].name`, `album.name/mid`, `interval`, `file.media_mid`, `file.size_128mp3/size_320mp3/size_flac/size_hires` (quality ladder from positive sizes). Cover: `https://y.gtimg.cn/music/photo_new/T002R500x500M000{album_mid}.jpg`. Risk control returns HTTP 500 with an empty body; the provider must treat that shape as an error, not a JSON parse failure.

### Song detail (unsigned)

`GET https://c.y.qq.com/v8/fcg-bin/fcg_play_single_song.fcg?songmid={mid}&format=json` (or `songid={id}` to resolve mid). Parses `data[0]`.

### Stream URL (unsigned POST)

`POST https://u.y.qq.com/cgi-bin/musicu.fcg`, `User-Agent: QQMusic 14090508(android 12)`, `Referer: https://y.qq.com/`. Body: `req_0.module = "vkey.GetVkeyServer"`, `method = "CgiGetVkey"`, `param = {guid, songmid[], songtype[], uin:"0", loginflag:1, platform:"20", filename[]}`, plus `comm {uin:0, format:"json", ct:24, cv:0}`. `guid` is client-generated (VoiceFox uses the process id). Filename = quality prefix + `media_mid` + extension; prefix ladder: Flac24: `AI00/Q001/Q000/F000 (.flac), O801 (.ogg), M800/M500 (.mp3)`; Flac: `Q001/Q000/F000 (.flac), O801 (.ogg), M800/M500 (.mp3)`; High320: `M800/M500`; Low128: `M500`. Response: `req_0.data.midurlinfo[]` matched by `filename`, each with `purl` and `vkey`; `req_0.data.sip[0]` is the CDN prefix (empty sip means no playable URL, typically anonymous or region-blocked). Final URL: `{sip}{purl}?guid={guid}&vkey={vkey}&uin=0&fromtag=120093`. No expiration metadata is returned upstream; URLs are simply re-resolved when they go stale.

### Lyrics (the only signed endpoint)

`POST https://u.y.qq.com/cgi-bin/musicu.fcg?sign={zzc_sign(body)}`, body `comm {ct:"11", cv:"14090508", v:"14090508", tmeAppID:"qqmusic", OpenUDID:"0", tmeLoginType:"0", uin:"0"}`, `req.module = "music.musichallSong.PlayLyricInfo"`, `method = "GetPlayLyricInfo"`, `param {songID, crypt, qrc, trans, roma}`. Live verification (2026-11-17) corrected the original upstream reading: with `crypt:1, qrc:1` the response `lyric` field is a hex-encoded encrypted QRC blob that needs 3DES QRC decryption (out of scope for the first slice); with `crypt:0, qrc:0, trans:1, roma:1` the fields are plain base64 LRC/translation/romanization and decode cleanly. musid-dli therefore requests `crypt:0, qrc:0`. zzc_sign: `h = sha1_hex(body)`; indexes are 0-BASED into the 40-char digest and out-of-range indexes are skipped (`h[40]` never contributes): `part1 = h[23],h[14],h[6],h[36],h[16],h[40],h[7],h[19]`; `part2 = h[16],h[1],h[32],h[12],h[19],h[27],h[8],h[5]`; `b[i] = scramble[i] XOR byte(h[2i:2i+2])` with scramble `[89,39,179,150,218,82,58,252,177,52,186,123,120,64,242,133,143,161,121,179]`; `part3 = base64(b)` minus `\ / + =` characters; result `zzc + part1 + part3 + part2`, lowercase. Golden vector: `zzc_sign("{\"test\":\"hello\"}") == "zzc8cbd808brhsjkhrcde4fogzsjczmgmobi8c73809f"` (44 chars total; part1 is 7 chars because index 40 is skipped).

### Playlists, albums, charts

All `musicu.fcg` POSTs are unsigned with cookie attached:
- Plaza: `playlist.PlayListPlazaServer/get_playlist_by_tag`, `{id:10000000, sin:36*(page-1), size:36, order:5, cur_page}`.
- Detail: `music.srfDissInfo.aiDissInfo/uniform_get_Dissinfo`, `{disstid, song_begin, song_num:1000}` paged up to 10 pages.
- Album: `music.musichallAlbum.AlbumSongList/GetAlbumSongList`, `{albumMid, albumID:0, begin, num, order:2}`. The response has no dedicated album metadata object (and `music.musichallAlbum.AlbumInfoServer/GetAlbumInfo` answers 40000 anonymously, live-verified); album title, artists and cover must be derived from the first `songInfo.album` object plus the list-level `totalNum`.
- User playlists (login): `fcg_user_created_diss` and `fcg_get_profile_order_asset.fcg` (JSONP, stripped), 100/page up to 20 pages; dirid 201 is the favorites list via `music.srfDissInfo.DissInfo/CgiGetDiss`.
- Write APIs (login): create (`create_playlist.fcg`, form-encoded), add (`fcg_music_add2songdir.fcg`, `midlist/typelist=13`), remove (`fcg_music_delbatchsong.fcg`, numeric `ids/types=3`); error codes `1000`/`301` mean login expired, `21` means duplicate name.
- Categories: `fcg_get_diss_tag_conf.fcg`; by-tag lists: `fcg_get_diss_by_tag.fcg`; playlist search: `client_music_search_songlist` (JSONP).
- Charts: board discovery scrapes `https://c.y.qq.com/node/pc/wk_v15/top.html` with regex `data-listname="..." data-tid=".../(\d+)" data-date="..."`; JSON fallback `fcg_myqq_toplist.fcg` (plain JSON, despite the .fcg suffix); entries via `musicToplist.ToplistInfoServer/GetDetail`. Live verification (2026-11-17): `GetDetail` accepts an empty `period` and returns the current issue (toplist.code 0, 100 songs), so musid-dli skips the HTML scrape and its regex dependency entirely.
- Artist: no dedicated upstream endpoint; VoiceFox's trait default searches by artist name and filters by singer match. musid-dli must either keep that fallback or mark artist_albums as Unsupported; do not invent an endpoint.

### Link parsing

Recognized: `songDetail/{mid}` path, `?songmid=`, `?songid=`, `playlist/{disstid}` and `taoge/{disstid}` paths, `?id=`/`?disstid=`/`?dissid=` (numeric). Hash-router `#/path?a=b` shapes tolerated. JSONP wrappers (`strip_jsonp`) appear on several legacy fcg endpoints and must be handled; musid-dli additionally guards the strip so plain JSON bodies (some fcg endpoints answer without a shell) pass through untouched, and it recognizes `albumDetail/{mid}` paths, which upstream does not.

### Auth (two QR channels)

- QQ: `xlogin` warmup (appid 716027609) collecting `pt_login_sig`; QR PNG from `ptqrshow` (cookie `qrsig`, 180s); poll `ptqrlogin` with `ptqrtoken = hash33(qrsig)` where `h = (h<<5)+h+c & 0x7fffffff`; statuses 0 success/65 expired/66 waiting/67 scanned parsed from `ptuiCB(...)`; success walks up to 10 manual redirect hops collecting Set-Cookie; login requires `uin` or `qqmusic_key` cookie.
- WeChat: appid `wx48db31d50e334801`; QR page on open.weixin.qq.com, uuid scraped from HTML; long poll `lp.open.weixin.qq.com` with stateful `last=404` semantics (408 waiting/404 scanned/402 expired/403 refused/405 confirmed); code exchange via unsigned `musicu.fcg` `music.login.LoginServer/Login` with `Cookie: login_type=2` and fallback hosts `szu./shu.`; writes `uin`+`wxuin`, `qqmusic_key`+`qm_keyst`, `W_X_`-prefixed musickey.
- No token refresh exists upstream; expired keys only surface as write-path errors 1000/301. musid-dli should implement an explicit expired state (as NetEase does) instead of silently rotting.

## Implementation status (2026-11-17)

Implemented and live-verified in musid-dli: zzc_sign (golden vector), search, song detail, stream URL ladder, crypt:0 lyrics, playlists (all read paths plus user lists behind a login guard), albums, charts (empty-period GetDetail), song/playlist/album links, QQ scan login and the WeChat scan login channel (uuid extraction, stateful long-poll, credential exchange with fallback hosts). Not implemented, with reasons:
- QRC 3DES decryption: the 24-byte key cannot be verified offline, and guessing crypto keys would violate the no-faked-results rule; `crypt:0` plain LRC covers the readable path.
- Artist lookup: no upstream endpoint exists; the capability stays false rather than pretending.
- Playlist write APIs (create/add/remove): out of the read-only library scope for now.

## Feasibility verdict for musid-dli

- Pure Rust: fully feasible; no JS runtime, no WebView, no DES/AES/TEA. Crypto needed: SHA1 + base64 + a 20-byte XOR table, and hash33.
- Feature flag: `source-qqmusic`, mirroring `source-netease` structure (get_json/eapi equivalent becomes unsigned `musicu.fcg` POST + plain GET helpers with cookie support).
- Hard parts ranked: (1) cookie lifecycle and the two QR flows, (2) risk-control shapes (empty-body HTTP 500, JSONP shells, opaque 500s), (3) HTML scraping for chart discovery, (4) hardcoded version constants (cv/ct/appids/UA) that will rot.
- Test plan: port the zzc_sign golden vector, hash33, filename ladder mapping, link shapes, JSONP stripping and 500-shape validation as fixture unit tests before any network code; live smoke via the netease_smoke module pattern (search, track, stream, lyrics, playlist, chart, album, artist, link, auth, user).
