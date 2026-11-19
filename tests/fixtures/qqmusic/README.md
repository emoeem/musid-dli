# QQ Music response fixtures

Offline fixtures shaped like real QQ Music API responses, used by the
`#[cfg(test)]` module in `src/source_qqmusic.rs` (loaded with `include_str!`).

## Derivation (honesty note)

No raw QQ Music response captures exist in this repository or in the upstream
VoiceFox `tx` module (grep for `fixture` over `voicefox/source/src/tx/` finds
nothing; `voicefox/source/tests/fixtures/` only ships a JS custom-source
script; every upstream tx test is an inline `json!` literal). Every fixture
here is therefore **reconstructed**, not captured:

- **Field shapes** mirror exactly what the parsers read, as recorded in the
  2026-11 audit of `voicefox/source/src/tx/{search,parse,url,lyric,crypto,
  login,session,playlist,leaderboard}.rs` (see `docs/SOURCE_QQ_MUSIC_AUDIT.md`
  and the audit conclusion for slice `qqmusic-fixtures`).
- **Live-verified parameters** recorded in `docs/SOURCE_QQ_MUSIC_AUDIT.md`
  (2026-11-17 anonymous + logged-in runs) drive choices like `crypt:0, qrc:0`
  for the plain-LRC lyric path, the empty `period` accepted by
  `musicToplist.ToplistInfoServer/GetDetail`, and the 1981-song playlist total
  used in `playlist_detail_minimal.json`.
- **Inline vectors** the audit extracted from upstream tests are kept
  verbatim: the user-playlist dual shapes (`dissid/dissname/songnum/listennum/
  logo/nickname` collected vs `tid/diss_name/diss_cover/song_cnt` created),
  the dirid 201/tid 0 favorites-and-system filter, the JSONP shell handling
  and the zzc_sign / hash33 / link-target vectors (those stay in the inline
  unit tests since they are strings, not response bodies).
- All remaining values (mids, names, URLs, sizes, vkeys) are **placeholder
  samples** chosen to exercise every parser branch. They are not verified
  catalog data; a fixture passing does not mean the live endpoint was
  re-verified (`docs/tasks.md` §15.4 separates fixture tests from live smoke
  tests).

## Files and the gotchas they pin

- `search_songs.json`: `client_search_cp` (`new_json=1`) shape, `code 0`,
  items at `data.song.list[]`, total at `data.song.totalnum`, and
  `has_more = page*limit < total`. Pins: empty `mid` and blank-title entries
  are dropped, singers come from `singer[].name`, a missing `album` yields
  `None` everywhere, an empty `file.media_mid` falls back to the song `mid`,
  and quality flags come only from positive `file.size_*` values.
- `song_detail.json`: `fcg_play_single_song.fcg` shape; items live under
  `data[]` and carry `name` (not `title`), which `parse_song` falls back to.
- `stream_vkey_ok.json`: `vkey.GetVkeyServer/CgiGetVkey` response; entries
  are matched by the requested filename, and entries with an empty `purl` or
  empty `vkey` are skipped even though they appear earlier in `midurlinfo`.
- `stream_vkey_empty_sip.json`: the risk-control shape; an EMPTY `sip` entry
  means "not playable anonymously" even with valid `purl`/`vkey` entries, so
  no stream can be picked (`Error::NotFound` at the call site).
- `lyric_plain_lrc.json`: `music.musichallSong.PlayLyricInfo/GetPlayLyricInfo`
  with `crypt:0, qrc:0`; `lyric`/`trans`/`roma` are plain base64 LRC (the
  2026-11-17 live verification superseded upstream's `crypt:1` request whose
  response needs 3DES QRC decryption that has no verifiable key).
- `playlist_detail.json`: `music.srfDissInfo.aiDissInfo/uniform_get_Dissinfo`
  with the triple gate `code == 0 && req.code == 0 && req.data.code == 0`;
  meta comes from `dirinfo.title|dissname`, `dirinfo.picurl` (kept as-is on
  this path, only list items get the http-to-https upgrade), `desc` and
  `total_song_num|dirinfo.songnum`; songs come from `data.songlist`.
- `playlist_detail_minimal.json`: same endpoint with no `dirinfo`; the title
  falls back to `QQ 歌单 {disstid}` (1981-song totals have been observed live).
- `playlist_by_tag.json`: `fcg_get_diss_by_tag.fcg` (category lists) and
  `client_music_search_songlist` (playlist search) feed the same
  `parse_diss_by_tag_item`: `dissid` (numeric or string), `dissname`,
  `imgurl` (http-to-https upgraded), `song_count|song_num`, `introduction`.
- `playlist_categories.json`: `fcg_get_diss_tag_conf.fcg`; groups live at
  `data.categories[]`, items need `usable == 1`, `categoryId` 0 and the
  10000000 sentinel are skipped (the sentinel is pre-seeded as the first
  "全部" entry).
- `user_created_playlists.json`: `fcg_user_created_diss` created shape;
  `dirid` 201 (favorites, fetched separately) and `tid` 0 (QZone/local system
  dirs) entries are dropped.
- `user_collected_playlists.json`: `fcg_get_profile_order_asset.fcg`
  (reqtype=3) collected shape with `dissid/dissname/songnum/listennum/logo/
  nickname`; a collected id duplicating a created id is dropped in the merge.
- `album_songs.json`: `music.musichallAlbum.AlbumSongList/GetAlbumSongList`;
  there is no dedicated album object, so metadata is derived from the first
  `songList[].songInfo.album` plus list-level `totalNum`, and tracks come
  from `songList[].songInfo` (entries with an empty `mid` are dropped).
- `charts_toplist.json`: `fcg_myqq_toplist.fcg` catalogue; boards live at
  `data.topList[]` with `id` + `topTitle`; board id 201 is filtered before
  parsing, `巅峰榜·` prefixes are stripped and a trailing `榜` appended.
- `chart_tracks.json`: `musicToplist.ToplistInfoServer/GetDetail` (an empty
  `period` requests the current issue); entries live at
  `toplist.data.songInfoList[]` and ranks 1..N are assigned after unparseable
  entries are dropped.
