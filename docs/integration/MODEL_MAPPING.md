# MODEL_MAPPING.md

voicefox (lx-core / lx-source) model to musid-dli `src/models/` mapping, read live from both trees on 2026-11-18.
Purpose: integration plan M0 baseline. Every row cites the file read. No business code was modified.

M3 update (2026-11-19): the model alignment milestone landed, five slices closed (gate evidence per slice in `MODEL_ALIGNMENT_DESIGN.md` §9). Sections 1 to 8 below keep their M0 baseline text; section 9 records the per-item M3 disposition, and the `Status:` lines carry short M3 annotations where the M0 wording has moved.

Status vocabulary used below:

| Tag | Meaning |
|---|---|
| implemented | Code exists on both sides or on the musid-dli side as stated |
| has tests | Covered by unit/integration tests executed in this session or present in the tree |
| net verified | Exercise against real provider endpoints is recorded in musid-dli docs or smoke examples |
| CI verified | Covered by a musid-dli CI job (`.github/workflows/ci.yml`) |

Baseline commands run in this session (repo root `/media/emo/code/musid-dli`, all exit 0):

| Command | Result |
|---|---|
| `cargo test --locked --no-default-features` | 19 lib tests + 1 integration test passed |
| `cargo test --locked --no-default-features --features "download,lyrics" download::` | 24 tests passed |
| `cargo test --locked --no-default-features --features "ncm,local" formats_` | 21 tests passed |

## 1. Identifier and source namespace

| voicefox | musid-dli | Notes |
|---|---|---|
| `SourceId` enum, 13 variants (kw, kg, tx, wy, mg, bili, soda, qianqian, joox, jamendo, fivesing, apple, local), `core/src/model/source.rs:16-43` | Free string `TrackId.provider` + `const ID` per source module | musid-dli has no enum. Conversion must go through `as_str()` values |
| `SourceId::as_str()` values | `netease` (`source_netease.rs:28`), `qqmusic` (`source_qqmusic.rs:33`), `kugou` (`source_kugou.rs:33`), `kuwo` (`source_kuwo.rs:40`), `migu` (`source_migu.rs:38`), `bilibili` (`source_bilibili.rs:33`), `jamendo` (`source_jamendo.rs:24`), `applemusic` (`source_apple.rs:31`), `fivesing` (`source_fivesing.rs:26`), `youtube-music` (`source_youtube_music.rs:60,96`), `lx-js` (`lx_js_provider.rs:33`) | Names differ from voicefox: wy vs netease, tx vs qqmusic, apple vs applemusic. A mapping table is required at the adapter boundary |
| Sources in voicefox with no musid-dli provider: soda, qianqian, joox, local (local handled separately, see LOCAL_LIBRARY_PARITY.md) | absent | Documented as a coverage gap, not a field gap |
| musid-dli extras with no voicefox source: `youtube-music`, `lx-js` | present | Reverse gap |

Status: implemented (read both trees). The `detect_from_link` domain table (`core/src/model/source.rs:172-203`) has no musid-dli counterpart; musid-dli `LinkResolver::resolve_link` (`src/provider/traits.rs:123`) is provider-internal, so link routing by domain stays host-side. M3 note (2026-11-19): the name mapping is now implemented in-crate as the two-direction table `src/models/namespace.rs` (`voicefox_source_to_provider`/`provider_to_voicefox_source`, design §3.2); `detect_from_link` still stays host-side.

## 2. SongInfo to Track

voicefox `SongInfo` (`voicefox/core/src/model/song.rs:12-36`) vs musid-dli `Track` (`src/models/track.rs:19-38`).

| voicefox field | musid-dli field | Conversion note |
|---|---|---|
| `id: String` | `id: TrackId { provider, id }` (`track.rs:6-9`) | voicefox local songs use the file path as id; online sources use platform ids. Adapter must supply provider from the originating source, not the path |
| `source: SourceId` | `id.provider: String` | Name translation, see section 1 |
| `name: String` | `title: String` | Rename |
| `singer: String` (joined, format "A、B", `song.rs:17`) | `artists: Vec<String>` (`track.rs:23`) | voicefox stores one joined string. musid-dli sources emit vector entries (e.g. netease `parse_track`). Round trip needs a split/join convention; voicefox `normalize_artist_name` splits on the ideographic comma, comma, ampersand, slash and pipe separators (`core/src/traits/source.rs:303-311`) which is the natural inverse |
| `album_name: String` (empty allowed) | `album: Option<String>` | Empty string maps to None; voicefox uses emptiness as "unknown" in `get_artist_albums` (`traits/source.rs:196-204`) |
| `album_id: String` (empty allowed) | `album_id: Option<String>` | Same empty-to-None rule |
| `duration: Duration` (ZERO allowed) | `duration: Option<Duration>` | ZERO maps to None |
| `cover_url: Option<String>` | `artwork_url: Option<String>` | Rename |
| `qualities: BTreeSet<Quality>` | `available_qualities: Vec<StreamQuality>` | Set to vec; ordering comes from tier ordering (`stream.rs:3-11` Unknown sorts last) |
| `audio: Option<AudioProperties>` | `audio: Option<AudioSpec>` (`track.rs:25`, wire key `audio`) | Closed in M3 (design §3.3); producers: the local scan fills `LocalMetadata.audio` (`src/local.rs:27`) since 2026-11-19, provider parse sites stay `None` |
| `extra: HashMap<String, String>` | `extra: BTreeMap<String, String>` (`track.rs:37`) | Same shape. BTreeMap gives deterministic serde output; keys are free strings, so voicefox keys carry over |
| `toggle_source: Option<Box<SongInfo>>` (`song.rs:31`) | no equivalent | Gap, see section 6 |
| `file_path: Option<PathBuf>` | no equivalent on Track; `LocalTrack.path: PathBuf` (`src/local.rs:30-33`) | Gap for online tracks; local parity handled in LOCAL_LIBRARY_PARITY.md |
| `file_ext: Option<String>` | `LocalMetadata.format: Option<String>` (lowercased extension, `src/local.rs:111-113`) | Approximate: musid-dli stores it only for local metadata, not on Track |

Status: implemented on both sides. Round-trip conversions have no shared code yet; they do not exist in either tree (no adapter crate). M3 note (2026-11-19): the conversion contract is pinned as the host adapter spec (`MODEL_ALIGNMENT_DESIGN.md` §3.1, including the artist-split and empty-to-None rules above); the adapter code itself still lands host-side, so this status stands.

## 3. SongUrl to StreamInfo (and into the download engine)

voicefox `SongUrl` (`voicefox/core/src/traits/source.rs:80-103`) vs musid-dli `StreamInfo` (`src/models/stream.rs:34-57`) and `DownloadRequest` (`src/download.rs:58-78`).

| voicefox SongUrl field | musid-dli StreamInfo field | musid-dli DownloadRequest field | Notes |
|---|---|---|---|
| `url: String` | `url: String` | `url: String` (primary) | Implemented |
| `quality: Quality` | `quality: StreamQuality` | none (engine is quality-blind) | Tier rename, section 4 |
| `duration: Duration` | `duration: Option<Duration>` | none | voicefox ZERO maps to None |
| `cover_url: Option<String>` | no slot; lives on `Track.artwork_url` | none | Field relocation risk, section 6 |
| `qualities: Vec<Quality>` | no slot; lives on `Track.available_qualities` | none | Same relocation |
| `headers: Vec<(String, String)>` | `headers: Vec<(String, String)>` (`stream.rs:48`) | `headers` applied by `apply_headers` (`download.rs:810`) | Implemented end to end |
| `size: Option<u64>` | `content_length: Option<u64>` (`stream.rs:44`) | `expected_size: Option<u64>` | Rename. Both sides use None for "not provided or unreliable" |
| `size_is_advisory: bool` | no slot | `size_is_advisory: bool` (`download.rs:67`) | Gap: advisory flag cannot round trip through StreamInfo, section 6; closed in M3: `StreamInfo.size_is_advisory` (design §2.1) |
| `md5: Option<String>` | `checksum: Option<String>` (`stream.rs:46`) + `checksum_algorithm: Option<ChecksumAlgorithm>` (`stream.rs:107`) | `md5: Option<String>` (`download.rs:69`) | NetEase fills all slots from `song/enhance/player/url/v1`: voicefox doc `traits/source.rs:93-95`; musid-dli `source_netease.rs:529,542` (`checksum: item["md5"]`, `checksum_algorithm: Some(Md5)` iff the digest string is present, M3 backfill 2026-11-19, design §2.3) |
| `candidate_urls: Vec<String>` | `fallback_urls: Vec<String>` (`stream.rs:50`) | `candidate_urls: Vec<String>` + `candidate_download_urls` (`download.rs:956-974`) | Order preserved: primary first, then extras, each followed by its NetEase m7 rewrite (`download.rs:956-1000`). musid-dli port is tested (`download.rs:1423-1476`: `netease_nodes_are_rewritten_to_the_m7_family`, `candidates_put_the_original_url_first`, `platform_candidates_are_kept_in_order_and_deduplicated`); executed this session in the `download::` run |
| `max_chunk_size: u64` (0 = off) | `max_chunk_size: Option<u64>` | `max_chunk_size: u64` | 0/off maps to None. Engine: `download.rs:344-354` strict bounded chunking, no probe, no single-connection fallback |

## 4. Quality and audio format

| voicefox | musid-dli | Note |
|---|---|---|
| `Quality`: Low128, High320, Flac, Flac24; default Low128 (`core/src/model/source.rs:223-233`) | `StreamQuality`: Low, Standard, High, Lossless, HiRes, Unknown (`stream.rs:4-12`) | Default is Unknown (`#[default] Unknown`), voicefox default is Low128. Tier split: voicefox Flac24 vs musid HiRes; voicefox has no Unknown |
| `QUALITY_ORDER` high to low (`source.rs:248-253`) | per provider ladder, e.g. netease `ladder()` (`source_netease.rs:360-379`), bilibili DASH selection (`source_bilibili.rs:674-676`) | voicefox has one global order; musid-dli falls back inside each provider's resolve |
| `AudioProperties { bitrate, sample_rate, bit_depth, lossless }` (`source.rs:256-282`) | `AudioSpec` (`track.rs:25`) | Closed in M3 as `AudioSpec` + `Track.audio` (design §3.3, key-for-key). The local scan reads real measurements: `read_metadata` fills `LocalMetadata.audio` from lofty with the zero filter and the container losslessness rule incl. the MP4 depth heuristic (`src/local.rs:93-117, 118-163`, 2026-11-19 slice, fixture-pinned over tests/fixtures/local); NCM sniffed container format unchanged (`AudioFormat`, `formats_ncm.rs:153-163`) |
| `AudioFormat` (musid only): Mp3, Flac, Aac, M4a, Opus, Ogg, Wav, Unknown (`stream.rs:13-24`) | voicefox has none | musid-dli extra; populated by netease (`source_netease.rs:467-473`), youtube-music, NCM sniff |
| LX bridge mapping: `lx_quality` collapses Lossless and HiRes both to `"flac"`, High to `"320k"`, rest `"128k"` (`src/lx_js.rs:46-52`); `quality_from_lx` unknown falls to Low (`lx_js.rs:54-60`) | tested in `lx_js.rs:86-93` | HiRes downgrade inside the LX bridge is by design; voicefox has no LX bridge model |

Status: implemented both sides; quality fallback "actual result" semantics differ, section 6. M3 note (2026-11-19): the `AudioProperties` gap is closed as `AudioSpec` + `Track.audio` (wire key `audio`, voicefox key-for-key, design §3.3) and the quality tier map is pinned in design §3.4; the semantics-difference note stands.

## 5. Lyrics

voicefox `LyricData` (`core/src/model/lyric.rs:5-17`), `LyricLine` (`lyric.rs:20-28`), `YrcLine`/`YrcWord` (`lyric.rs:30-43`) vs musid-dli `LyricDocument`/`LyricLine`/`LyricWord` (`src/models/lyrics.rs:39-49, 25-37, 18-23`).

| voicefox | musid-dli | Note |
|---|---|---|
| `lyric: String` | `original: String` | Rename |
| `tlyric: Option<String>` | `translation: Option<String>` | Raw string on both sides |
| `rlyric: Option<String>` | `romanization: Option<String>` | Raw string on both sides |
| `lxlyric: Option<String>` (raw YRC/QRC) | `lines[].words: Vec<LyricWord>` (`lyrics.rs:36`) | Raw word-timed text vs parsed words. `LyricFormat` (Krc, Mrc, Qrc, Yrc, Ttml, Spotify, Musixmatch, PlainText, Unknown, `lyrics.rs:5-16`) keeps the provenance that voicefox only implies |
| `raw_lrc: Option<String>` | no dedicated slot | `original` is the nearest slot. musid-dli handles the ambiguity in `standard_lyric` (`download.rs:1201-1222`): raw text trusted only if it contains `mm:ss.xx` brackets (`looks_like_timed_lrc`, `download.rs:1224-1240`), otherwise parsed lines are rendered; private JSON is never written as .lrc |
| `LyricLine.timestamp/duration` | `LyricLine.start_ms/end_ms: Option<u64>` | voicefox derives duration from the next line; musid-dli keeps end_ms optional |
| `LyricState` (`lyric.rs:46-56`, player snapshot) | no equivalent | Out of scope: playback state stays in lx-player/host |
| lyric format enum | `LyricFormat` (`lyrics.rs:5-16`) | musid-dli extra, tested via `lyrics` feature tests |

Status: implemented both sides; conversion is lossy in the direction voicefox to musid-dli for `raw_lrc` provenance and lossy musid-dli to voicefox for per-word timing (voicefox has no words slot outside YrcLine). M3 update (2026-11-19): both directions are implemented in-crate in `src/lyrics_convert.rs` (feature lyrics-conversions) with exactly these lossy points pinned as code and tests (design §4: lxlyric hard-error contract, `raw_lrc` promoted only on a blank `lyric`, per-line translations dropped explicitly, word timing dropped explicitly in the reverse direction).

## 6. voicefox semantics missing in musid-dli (gaps and risks)

Each item names the voicefox site and the musid-dli state.

| # | voicefox semantics | musid-dli state | Risk if unaddressed |
|---|---|---|---|
| G1 | `SongUrl.size_is_advisory` (`traits/source.rs:89-92`): size only fails integrity when actual is shorter | `DownloadRequest.size_is_advisory` exists (`download.rs:67`, verdict `verify_size` `download.rs:827-850`, tested `download.rs:1393-1405`), but `StreamInfo` has no slot | A resolver returning StreamInfo cannot express the advisory flag; the host must carry it out of band, or every source's under-reporting (QQ FLAC minus 15 bytes, voicefox comment `traits/source.rs:91`) fails the size gate |
| G2 | One struct (`SongUrl`) carries stream plus download metadata | No `StreamInfo to DownloadRequest` conversion exists: every provider module plus `cache.rs`/`resolve.rs`/`lx_js_provider.rs` constructs `StreamInfo` directly (grep "StreamInfo {" over `src/`, 14 files), and no `From<StreamInfo> for DownloadRequest` impl exists anywhere (grep run this session, zero hits) | Host re-implements the mapping; fields `content_length to expected_size`, `checksum to md5`, `fallback_urls to candidate_urls`, `headers`, `max_chunk_size` must be listed in one adapter or drift |
| G3 | DASH/Range metadata: voicefox has only `max_chunk_size` | musid-dli adds `index_range`/`init_range` (`ByteRange`, `stream.rs:26-31`) filled only by youtube-music (`source_youtube_music.rs:197-204`), but `download.rs` never reads them (grep of `download.rs` for `index_range`/`init_range`: zero hits) | Byte-range DASH audio cannot be downloaded by the engine; the fields are descriptive only. Playback hosts are fine, download of such streams is not |
| G4 | `SongUrl.expires` does not exist upstream; musid-dli adds `expires_at_unix` | musid-dli extra, filled by netease query param (`url_expiry`, `source_netease.rs:1202-1211`) and youtube-music `player.valid_until` (`source_youtube_music.rs:217`), consumed by `StreamCache` (`cache.rs:40-49`) | Not a gap; reverse direction. Note voicefox QR session has `expires_in` (`core/src/model/login.rs:91`) which musid-dli QrLoginSession also carries (`provider/traits.rs:140`) |
| G5 | `AudioProperties` measured specs (`source.rs:256-282`) | closed in M3: `AudioSpec` + `Track.audio` (design §3.3); the local scan producer landed 2026-11-19 (`LocalMetadata.audio` filled from lofty, fixture-pinned, design §3.3) | specs and label (`AudioSpec::label`, `16/44.1`/`320K`) reproducible in-crate; provider parse sites still `audio: None`, L14 tier classification stays a gap (LOCAL_LIBRARY_PARITY.md) |
| G6 | `SongInfo.toggle_source` persisted match target (`song.rs:31`) | absent; musid-dli keeps alternatives transient via `resolve_with_alternatives` (`resolve.rs:170-183`, `matcher::rank_alternatives`) | Host loses the "which song is this after switch" bookkeeping unless it keeps its own copy |
| G7 | `SongUrl.cover_url` and `SongUrl.qualities` travel with the URL | musid-dli keeps them on Track only | When resolution happens on a different Track than search returned (matched alternatives), the URL result loses the cover the way voicefox returns it |
| G8 | `QrLoginStatus` transient states NetworkError, InvalidSession plus `is_transient`/`is_terminal` (`core/src/model/login.rs:14-49`) | musid-dli enum lacks NetworkError and InvalidSession, has no helpers (`provider/traits.rs:142-150`) | Poll loop must classify transient states itself |
| G9 | `QrLoginSession.image_png` (base64 QR image, QQ/WeChat, `login.rs:86-92`) and `QrLoginKind` (Standard/WeChat, `login.rs:55-73`) | musid-dli QrLoginSession has key/url/expires_in only (`provider/traits.rs:137-140`); no kind | WeChat channel and image QR must be host-side extensions |
| G10 | `SourceHealth` user-facing probe result (ok, latency_ms, result_count, detail, `core/src/model/source.rs:5-12`) | musid-dli `ProviderHealthSnapshot { state, consecutive_failures }` (`health.rs:28-31`) | Different purpose: voicefox reports one probe outcome, musid-dli is a circuit breaker snapshot. No latency/count/detail |
| G11 | voicefox local extra keys (`file_size`, `file_modified_unix_nanos`, `cue_file`, `cue_track`, `cue_start_ms`, `import_url`) on SongInfo.extra | musid-dli Track.extra exists but no producer emits these keys (`src/local.rs` scan fills none; CUE handler uses `ContainerTrack` fields instead, `formats.rs:21-35`) | Cross-mapping of local songs must translate cue_* keys to ContainerTrack fields, see LOCAL_LIBRARY_PARITY.md |
| G12 | `SearchResult.total/has_more` always numeric/bool (`traits/source.rs:13-17`) | `SearchPage.total: Option<u64>` (`provider/traits.rs:40-44`) | None handling required at the UI boundary |

## 7. StreamInfo semantics already defined by src/download.rs (engine contract)

Reference list; all from `src/download.rs` unless stated:

- Candidate order: `candidate_download_urls` (`download.rs:956-974`) puts the primary first, then `candidate_urls`, each followed by its NetEase m8/m801/m804/m704 rewrite (`rewrite_netease_host`, `download.rs:977-1000`). Duplicates dropped, order stable. Tests `download.rs:1423-1476`.
- Failure iteration: `download` loops candidates, resets progress between candidates, keeps last error (`download.rs:315-395`).
- Integrity: size gate only when `verify_size` and `expected_size` set; advisory flag short-circuits only the longer-than-declared direction (`verify_size`, `download.rs:827-850`). MD5 gate after size (`download.rs:373-379`); empty string skips (`verify_md5_sync`, `download.rs:852`).
- Strict chunk mode: `max_chunk_size > 0` requires `expected_size > 0` (`download.rs:344-354`), else the attempt fails with "strict chunked download needs the source-declared size".
- No byte-level resume: staging file `.part` is rewritten from zero per attempt, atomic rename on success (`download.rs:19-22` doc, `staging_path` `download.rs:1002-1008`, rename `download.rs:380-385`).
- Retryable classification: network, io, RangesUnsupported, Http 5xx/408/429 (`DownloadError::is_retryable`, `download.rs:126-138`), backoff 400 ms doubling capped 8 s plus jitter (`sleep_before_retry`, `download.rs:1011-1015`).
- Post-download finish: `embed_tags` (`download.rs:1054-1090`, cover retry without cover), `write_metadata`/`MetadataEdit` (`download.rs:1092-1147`, port of voicefox `source/src/local/metadata.rs write_metadata`), `write_lyric_file`/`merge_lyric` (`download.rs:1149-1199`), `standard_lyric` (`download.rs:1201-1222`), `validate_audio` duration tolerance (`download.rs:1270-1304`).
- Scope boundary: task orchestration, records DB, task UI, WebDAV, filename templates stay host-side per `download.rs:11-17` and `docs/IMPLEMENTATION_STATUS.md` 2026-11-18 entry.

Status: implemented + has tests (24 `download::` tests executed this session, including a local HTTP test server `download.rs:1543-1660`). `docs/IMPLEMENTATION_STATUS.md` P4 row records the engine slice as "implemented and loopback-verified (2026-11-18, feature download)": loopback means the local test server, not a real CDN download; no live-network download smoke is recorded for the engine. CI runs `cargo test --all-features` which includes it (`.github/workflows/ci.yml` jobs `test` and `msrv`). M3 update (2026-11-19): `StreamInfo` gains `size_is_advisory`/`checksum_algorithm`/`fallback_endpoints` and `download_request_from_stream` (`src/download.rs:97`) is the one StreamInfo to DownloadRequest adapter (design §2.5); the `download::` suite counted 30 tests in the M3 gate run.

## 8. Remaining collection and auth models

| voicefox | musid-dli | Notes |
|---|---|---|
| `Playlist` (id, name, source, cover_url, song_count, description, play_count, creator, link, extra, `core/src/model/playlist.rs:9-26`) | `Playlist` (id, title, description, artwork_url, track_count, tracks, `src/models/playlist.rs:4-15`) | Missing in musid-dli: `play_count`, `creator`, `link`, `extra`. Extra fields carry provider detail-page routing in voicefox (`playlist.rs:23-25`) |
| `PlaylistCategory` (adds `source`, `extra`, `playlist.rs:50-62`) | `PlaylistCategory` (id, name, group, count, hot, `src/models/playlist.rs:18-27`) | musid-dli drops source and extra |
| `Album` (id, name, source, cover_url, artist, `playlist.rs:79-85`) | `Album` (id: TrackId, title, artists: Vec, artwork_url, track_count, `src/models/album.rs:4-13`) | voicefox has single `artist: String`; musid-dli vector plus track_count |
| `Artist` (id, name, source, cover_url, `playlist.rs:88-93`) | `Artist` (id: TrackId, name, artwork_url, `src/models/artist.rs:4-9`) | 1:1 |
| `LeaderboardInfo` (id, name, source, update, `core/src/model/leaderboard.rs:4-10`) | `Chart` (id: TrackId, title, description, `src/models/chart.rs:4-9`) + `ChartEntry { rank, track }` (`chart.rs:11-14`) | voicefox `update` (e.g. "updated daily") has no slot; rename update to description or extend Chart |
| `SearchResult` (`traits/source.rs:13-17`) | `SearchPage` (`provider/traits.rs:40-44`) | Section 6 G12 |
| `ParsedLink` enum Song/Playlist/Album (`traits/source.rs:24-34`) | `ResolvedLink` enum Track/Playlist/Album/Artist (`provider/traits.rs:115-119`) | musid-dli adds Artist variant; voicefox Playlist/Album variants carry the songs vector, musid-dli fetches them via a second call (`playlist_tracks`/`album_tracks`) |
| `QrLoginSession/Result` (`core/src/model/login.rs:77-106`) | `QrLoginSession/QrLoginPoll` (`provider/traits.rs:136-158`), `ProviderSession` (`src/session.rs:9-15`), `SessionStore` (`session.rs:26`) | G8/G9. musid-dli never returns cookies to the caller; sources write into SessionStore, matching voicefox's "cookie not through the result" rule (`login.rs:95-97`) |
| `SourceCapabilities` (`traits/source.rs:42-70`) | `ProviderCapabilities` (`provider/traits.rs:5-17`) | musid-dli adds `download`; voicefox adds `wechat_login`, `vip_account`, `qr_login`, `user_playlists`, `playlist_search` granularity |
| voicefox `SongInfo.audio` label pipeline (`song.rs:62-70`, `source.rs:269-291`) | measured label ported as `AudioSpec::label` (`track.rs:37`); the Track-level tier fallback chain stays host-side | G5 model half closed; the measured-label output is now fed by the local scan producer (`LocalMetadata.audio`, 2026-11-19) |

Status: implemented both sides as described; none of these model conversions is exercised by any test today because no adapter code exists (verified: no crate in either repo imports the other; musid-dli `Cargo.toml` has no path dependency and voicefox workspace `Cargo.toml` neither). M3 update (2026-11-19): the collection models gained the voicefox slots (`Playlist` play_count/creator/link/extra, `PlaylistCategory` provider/extra, `Chart` updated_description, all `#[serde(default)]`, design §5.1) and the cross-model adapter decisions are pinned (design §5.2-§5.4); no cross-crate adapter crate exists yet, so this status stands.

## 9. M3 alignment result (2026-11-19)

M3 landed, five slices closed (s1 stream-semantics, s2 track-conversions, s3 lyrics-conversions, s4 collection-models, s5 auth-model-gaps); per-slice gate evidence (test/clippy at each gate feature) and commands are recorded in `MODEL_ALIGNMENT_DESIGN.md` §9. Disposition of the drift recorded in sections 1 to 8 above:

| Item above | M3 disposition (2026-11-19) |
|---|---|
| §1 namespace table | implemented in-crate: `src/models/namespace.rs` two-direction lookup over the pinned table (design §3.2); `detect_from_link` stays host-side |
| §2 SongInfo to Track | conversion contract pinned as the host adapter spec (design §3.1); no in-crate conversion function (voicefox types stay out of this crate) |
| §3 SongUrl to StreamInfo/DownloadRequest | landed: `size_is_advisory`/`checksum_algorithm`/`fallback_endpoints` on `StreamInfo` (design §2.1), advisory parity per provider (§2.2), MD5-only checksum rule (§2.3), per-URL `FallbackEndpoint` metadata with the ordering contract (§2.4), the single adapter `download_request_from_stream` (`src/download.rs:97`, §2.5), Range/DASH `ByteRange` pinned descriptive-only (§2.6) |
| §4 quality tiers, AudioProperties gap | closed as `AudioSpec` + `Track.audio` (wire key `audio`, design §3.3) and the pinned tier map (§3.4); the label pipeline port is `AudioSpec::label`, UI labels stay host-side |
| §5 lyrics | implemented both directions in `src/lyrics_convert.rs` (design §4) |
| §6 G1 (advisory size) | closed: `StreamInfo.size_is_advisory` (design §2.1/§2.2) |
| §6 G2 (one adapter) | closed: `download_request_from_stream` (design §2.5) |
| §6 G3 (DASH/Range download) | unchanged by design: `ByteRange` stays descriptive (design §2.6); per-endpoint engine consumption stays a download-engine slice (design §8 register) |
| §6 G5 (AudioProperties) | closed: `AudioSpec` (design §3.3); the local-scan producer landed 2026-11-19 (parity slice m3-audio-spec-zero-producers: `LocalMetadata.audio` from lofty, fixture-pinned over tests/fixtures/local, design §3.3/§9); the provider-side backfill (kuwo N_MINFO, netease level) stays an optional provider slice needing live fixtures |
| §6 G6 (toggle_source) | not migrated, confirmed dead-slot downgrade (design §3.1) |
| §6 G7 (cover/qualities relocation) | documented relocation, no field migration, no `ProviderArtwork` face (design §5.5) |
| §6 G8 (QR transient states) | closed: nine-state `QrLoginStatus` with `is_transient`/`is_terminal` and upstream snake_case serde (design §6.1) |
| §6 G9 (QR image/kind) | closed: `QrLoginSession.image_png` + `QrLoginKind` (design §6.2); the producer fill landed 2026-11-19 (slice m3-qr-image-png-not-backfilled): both QQ create sites fill it from the in-hand PNG/JPEG bytes, netease/kugou/bilibili stay `None` per their actual H5-link returns (design §6) |
| §6 G10 (probe result) | closed: `ProviderProbeResult` in `src/health.rs` (design §6.4) |
| §6 G11 (local extra keys) | unchanged: adapter translation per `LOCAL_LIBRARY_PARITY.md` (design §3.6) |
| §6 G12 (total narrowing) | adapter rule pinned (design §3.7), no model change |
| §8 collection fields | landed: `Playlist` play_count/creator/link/extra, `PlaylistCategory` provider/extra, `Chart` updated_description (design §5.1); Album/Artist conventions pinned (§5.2); `ParsedLink` songs vector dropped, resolve lazily (§5.3); capability granularity table (§5.4) |

Producers for the new slots stay governed by the design §8 register: `audio` (local scan) landed 2026-11-19 (`LocalMetadata.audio` from lofty, fixture-pinned, slice m3-audio-spec-zero-producers); `image_png` landed 2026-11-19 for the two QQ create sites (slice m3-qr-image-png-not-backfilled; netease/kugou/bilibili stay `None` per their H5-link returns); `isrc`, `play_count`/`creator`/`link`/`extra` and `updated_description` stay deferred to provider/local slices; no provider fills `Track.audio` in this milestone (provider parse sites `audio: None`, hosts copy `LocalMetadata.audio`).
