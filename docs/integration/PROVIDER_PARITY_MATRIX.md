# Provider parity matrix: VoiceFox source/src vs musid-dli

Audit date: 2026-11-20; this revision re-anchors every musid-dli line
reference against the working tree after the 2026-11-19 M1/M2/M3 milestone
batch and the 2026-11-20 parity slices landed (all uncommitted on top of
HEAD 919f1de), refreshes the §3/§8 baseline numbers from this round's runs,
and folds the M1 transport / M2 error-taxonomy + auth-facet / M3
model-extension facts into §4/§5. Documented slices only; no new live
claims were probed for this revision. The qianqian row and §6.4/§7 were
re-audited 2026-11-19 for the source-qianqian migration slice (anonymous
re-probe plus the dated smoke run recorded in docs/IMPLEMENTATION_STATUS.md),
and the joox row, §5, §6.4 and §7 were re-audited the same day for the
source-joox migration slice (anonymous re-probe recorded in
docs/PROVIDER_EVALUATION.md and docs/IMPLEMENTATION_STATUS.md). This
revision is also the M4 full-check record: the re-anchoring and the §3/§8
refresh above are that batch's matrix finding (11 findings closed: the
qianqian and joox deferred-status corrections, the five M3 producer
backfills for StreamInfo.fallback_endpoints, netease checksum_algorithm,
the local AudioSpec scan producer, QrLoginSession.image_png and the migu
collection fields, the fivesing LinkResolver registration, and the apple
and youtube-music M2 error-category follow-ups), and the batch's
independent 2026-11-19 dual-source re-probe (qianqian: 5 anonymous signed
GETs; joox: 10 anonymous curl GETs; both revival verdicts re-confirmed,
full evidence of record in docs/PROVIDER_EVALUATION.md) is summarized in
§6.4; the matrix pass itself ran no endpoints and modified no business
code. Scope: the built-in Provider directory of the VoiceFox
source crate (`/media/emo/code/voicefox/source/src/`, read-only) compared
against the musid-dli provider adapters (`/media/emo/code/musid-dli/src/`,
working tree at HEAD 919f1de plus uncommitted modifications; voicefox at
branch main, HEAD dd116b1, unchanged since the 2026-11-18 audit, so the
voicefox line anchors stand as first written and were spot-re-verified
this round). Both git reads were limited to `status`, `log`,
`remote`. No business code was modified on either side; the only file this
audit produced is the one you are reading.

Sources of record for every claim below:

- voicefox capability bits: the per-provider `capabilities()` implementations
  in `source/src/<id>/mod.rs` (line refs in the table), plus the trait surface
  in `voicefox/core/src/traits/source.rs` (`MusicSource`, `SourceCapabilities`
  at line 42).
- musid-dli capability bits: the `ProviderCapabilities` blocks in
  `src/source_*.rs` (line refs in the table) and the trait surface in
  `src/provider/traits.rs`.
- musid-dli wiring: each provider's `register()` and its registration
  capability test (line refs in the table; named
  `registration_exposes_implemented_capabilities`, with netease's variant
  named `registration_exposes_only_implemented_capabilities` at
  src/source_netease.rs:1388), plus `src/registry.rs` register slots.
- Live-verification claims: the dated smoke records in `docs/HANDOFF.md`
  (log table lines 154-165) and `docs/IMPLEMENTATION_STATUS.md` (provider
  rows and dated log entries). This audit re-ran none of the live smokes;
  smoke re-runs belong to the outer script (see section 3 for what was run).

## 1. Column semantics

| Column | voicefox trait surface | musid-dli trait surface |
|---|---|---|
| search | `MusicSource::search` (core/src/traits/source.rs:119) | `TrackSearch::search` (src/provider/traits.rs:123) |
| track | song detail by id / share link, embedded per provider (no dedicated trait slot) | no dedicated trait slot; delivered through `LinkResolver` song branch or the smoke `track` module |
| stream | `MusicSource::get_song_url` (core/src/traits/source.rs:126) | `StreamResolver::resolve` (src/provider/traits.rs:127) |
| lyrics | `MusicSource::get_lyric` (core/src/traits/source.rs:128) | `LyricsProvider::lyrics` (src/provider/traits.rs:131) |
| playlist | `get_playlist_categories` / `get_playlists` / `search_playlists` / `get_playlist_detail` (core/src/traits/source.rs:141-166) and `get_user_playlists` (:264) | `CollectionProvider` + `PlaylistProvider` (src/provider/traits.rs:134-152) |
| album | `get_album_songs` (core/src/traits/source.rs:222) | `AlbumProvider` (src/provider/traits.rs:153) |
| artist | `get_artist_songs` / `get_artist_albums` (core/src/traits/source.rs:163, :186) | `ArtistProvider` (src/provider/traits.rs:160) |
| chart | `get_leaderboard_boards` / `get_leaderboard` (core/src/traits/source.rs:252, :255) | `ChartProvider` (src/provider/traits.rs:175) |
| link | `MusicSource::parse_link` (core/src/traits/source.rs:137) | `LinkResolver::resolve_link` (src/provider/traits.rs:188) |
| auth | `login` / `qr_login` / `wechat_login` caps, `create_qr_login(_kind)` / `check_qr_login` / `logout` / `is_vip_account` (core/src/traits/source.rs:272-300) | `QrAuthenticationProvider` + `AuthenticationProvider` (src/provider/traits.rs:462-468); the per-descriptor auth facet is `AuthCapability` (:35) via `auth_capability()` (:76) |

## 2. Status legend

- `LV`: implemented + live-verified. A dated anonymous (or stated-scope) smoke
  run is recorded in docs/HANDOFF.md or docs/IMPLEMENTATION_STATUS.md. The
  record is quoted here; this audit did not re-run it.
- `FV`: implemented + fixture-verified. Behaviour pinned by offline fixtures
  in tests/fixtures/<provider>; the live path is not verified.
- `UNV`: implemented + unverified. Compiles and is registered, but neither a
  fixture nor a live record covers the column.
- `deferred`: consciously not implemented, with the in-file reason cited.
- `not migrated`: the provider does not exist in musid-dli (deferred with a
  recorded reason).
- `n/a`: no real surface on either side (stub or absent upstream).
- CI-verified: see section 3. CI (`/.github/workflows/ci.yml`) runs fmt,
  clippy, tests in three feature modes, an isolated feature matrix, MSRV
  1.89.0, tri-platform builds, docs and a deps gate. CI verifies compile plus
  unit/fixture tests only; it never touches live endpoints, so a `LV` claim
  always comes from a manual smoke record, never from CI.

## 3. Baseline checks run in this audit

| Command | Result |
|---|---|
| `cargo test --locked --no-default-features` | 69 unit + 1 integration passed, 0 failed |
| `cargo test --locked --features source-netease,source-qqmusic,source-kugou,source-kuwo,source-migu,source-bilibili,source-jamendo,source-apple,source-fivesing --lib registration` | 12 registration-matching tests passed, 491 filtered out, 0 failed |
| `cargo test --locked --features lx-js-bridge --lib capabilities` | 2 passed, 119 filtered out, 0 failed |

The twelve registration-matching tests are the nine per-provider tests (one
per provider, asserting descriptor bits against
`registry.<slot>_provider(ID)`) plus the three registry
registration-semantics tests the M2 registry-capabilities surface added
(`capability_queries_reflect_live_registration_not_history`,
`descriptor_auth_upgrade_is_sticky_across_registration_order`,
`providers_with_capability_filters_by_live_registration`). The lx
capabilities filter matches `capabilities_match_the_contract` plus the
registry `legacy_register_overwrites_slot_and_or_merges_capabilities` test
whose name contains "capabilities". The nine-source build proves every
provider's capability block, `register()`
wiring and registry slots agree; it does not exercise live
endpoints. Feature-wide suites and smoke re-runs are the outer script's job
and were deliberately not run here.

## 4. Directory map (one row per voicefox builtin provider)

| Provider | voicefox source dir | voicefox declared caps (caps() line) | musid-dli module | musid-dli caps block | registration test | Overall musid-dli status |
|---|---|---|---|---|---|---|
| wy (NetEase) | source/src/wy/ (10 files) | playlists, playlist_search, playlist_categories, album, artist, leaderboard, link_parse, login, qr_login, user_playlists (wy/mod.rs:174) | src/source_netease.rs | all 9 bits true (:443) | :1388 | implemented+live-verified |
| kg (Kugou) | source/src/kg/ (10 files) | same as wy (kg/mod.rs:61) | src/source_kugou.rs | all 9 bits true, artists = search fallback (:648) | :2602 | implemented+live-verified |
| kw (Kuwo) | source/src/kw/ (9 files) | playlists, playlist_search, playlist_categories, album, artist, leaderboard, link_parse (kw/mod.rs:49) | src/source_kuwo.rs | 7 bits true, authentication false, descriptor auth facet `Anonymous` (M2) (:1918) | :3454 | implemented+live-verified (partial, see matrix) |
| tx (QQ Music) | source/src/tx/ (10 files) | kw set + login, qr_login, wechat_login, user_playlists (tx/mod.rs:61) | src/source_qqmusic.rs | 8 bits true, artists false (:760) | :2414 | implemented+live-verified (partial) |
| mg (Migu) | source/src/mg/ (9 files) | kw set (mg/mod.rs:49) | src/source_migu.rs | 7 bits true, authentication false (:1388) | :2395 | implemented+live-verified (partial) |
| bili (Bilibili) | source/src/bili/ (5 files) | playlists, album, artist, leaderboard, link_parse, login, qr_login (bili/mod.rs:413) | src/source_bilibili.rs | track_search, stream_resolve, authentication, playlists, charts true; lyrics/albums/artists/link_resolve false (:907) | :1884 | implemented+live-verified (partial) |
| fivesing (5sing) | source/src/fivesing/ (2 files) | playlists, playlist_search, link_parse (fivesing/mod.rs:70) | src/source_fivesing.rs | track_search, stream_resolve, lyrics, playlists, link_resolve true; authentication false with the auth slot registered on the explicit `None` facet (M2) (:494) | :1139 | implemented+live-verified |
| jamendo | source/src/jamendo/mod.rs (611 lines) | playlists, playlist_search, album, link_parse (jamendo/mod.rs:356) | src/source_jamendo.rs | track_search, stream_resolve true; playlists, albums, link_resolve explicitly false (:360) | :634 | implemented+live-verified (containers honestly refused) |
| apple (Apple Music) | source/src/apple/ (2 files) | playlists, playlist_search, playlist_categories, album, link_parse (apple/mod.rs:47) | src/source_apple.rs | track_search, stream_resolve (preview only), lyrics, playlists, albums, link_resolve true; authentication false with the LoggedOut-fold auth impl unregistered (M2) (:586) | :912 | implemented+live-verified (preview contract) |
| qianqian | source/src/qianqian/ (8 files) | playlists, playlist_search, playlist_categories, album, artist, link_parse (qianqian/mod.rs:48) | src/source_qianqian.rs | track_search, stream_resolve, lyrics, playlists, albums, link_resolve true; artists/charts/authentication false (:732) | :1616 | implemented+live-verified (partial, see matrix; migrated 2026-11-19) |
| joox | source/src/joox/ (2 files) | playlists, playlist_search, album, link_parse (joox/mod.rs:103) | src/source_joox.rs | track_search, stream_resolve, lyrics, playlists, albums, link_resolve true; artists/charts/authentication false (:706) | :1690 | implemented+live-verified (partial, see matrix; migrated 2026-11-19: search/lyrics anonymous-live, stream/track/playlist/album faces live-refused by dead endpoints) |
| soda | source/src/soda/ (4 files) | playlists, playlist_search, link_parse (soda/mod.rs:128) | absent | absent | absent | not migrated (deferred) |
| js (LX custom sources) | source/src/js/ (engine.rs, js_source.rs, loader.rs, wrapper.js) | no own caps; wraps lx-music scripts (js_source.rs:228-335) | src/lx_js_provider.rs + src/lx_js.rs + src/lx_js_engine.rs (feature lx-js-bridge) | track_search, stream_resolve, lyrics true (lx_js_provider.rs:67) | :835 capabilities_match_the_contract | implemented+fixture-verified (host wiring pending) |
| local | source/src/local/ (scanner.rs, metadata.rs, mod.rs; 1077 lines) | all false, `SourceCapabilities::default()` (asserted at manager.rs:1355-1357) | src/local.rs (default feature) | not a `MusicProvider`; `LocalLibraryProvider` + `scan_audio_files` / `read_metadata` (local.rs:35-117), plus the measured-audio-spec producer (`LocalMetadata.audio` filled from lofty, local.rs:118-163, 2026-11-19 slice m3-audio-spec-zero-producers) | n/a | implemented (library scan parity + the §3.3 AudioSpec scan producer, fixture-pinned over tests/fixtures/local), no provider row |

Extra row outside the voicefox builtin directory: `src/source_youtube_music.rs`
(RustyPipeProvider, feature source-youtube-music, caps track_search +
stream_resolve at :227). voicefox has no builtin youtube module; it references
googlevideo only in the host download engine (voicefox/app/src/download/engine.rs:42).
Status: implemented+unverified; live search/playback (Botguard/PO token) is
recorded human-required and unscheduled (docs/IMPLEMENTATION_STATUS.md:26).
Stream column note (2026-11-19): every non-chosen audio stream rides the
structured `fallback_endpoints` slot with its own bitrate-derived tier,
declared size and the shared player expiry (MODEL_ALIGNMENT_DESIGN.md §2.4).
M2 error-category wiring (2026-11-20, slice
m2-youtube-error-categories-unwired): the nine-provider M2 round did not
cover this extra row (unclaimed: no voicefox builtin module, no provider row
in §4) and all four of its `Error::Provider` catch-alls (client build :60,
music search :244, search continuation :255, player resolve :286) stayed
legacy until this slice. They now classify through `classify_rustypipe_error`
(:121): reqwest transport text → `Network`, RustyPipe ladder-exhausted
429/5xx (its own internal retry ladder, `rustypipe::error::Error::
should_retry`) → `Unavailable`, unmapped statuses and response-contract
violations (InvalidData/WrongResult/BadRequest/Other residue) →
`InvalidResponse` (code slot stays `None`, HTTP statuses are not platform
API codes, joox/qianqian/apple precedent), PO-token Botguard failures →
`Unavailable`, the declared auth-refusal 401 (`AuthError::NoLogin`) →
`AuthenticationRequired`; the extraction-level NotFound and Paid/DRM
refusals keep the legacy `NotFound`/`Unsupported` semantics alongside the
untouched caller-side `InvalidInput` guard (:277), DRM-field `Unsupported`
(:288) and empty-streams `NotFound` (:296). The provider id rides the typed
slot on every M2 variant; the pre-migration `error.to_string()` texts are
byte-exact in the detail slot except the reqwest full-request-URL suffix
(:184), stripped per the ErrorDetail contract. Pinned by eight pure-seam
tests in src/source_youtube_music.rs (the RustyPipe error enums are
`#[non_exhaustive]` but variant-constructible); the live path stays
unverified, search/playback human-required.

M1/M2/M3 milestone wiring (landed 2026-11-19, uncommitted, plus the
2026-11-20 follow-up slices; design records in
docs/integration/TRANSPORT_DESIGN.md rev 4, AUTH_REGISTRY_DESIGN.md §9 and
MODEL_ALIGNMENT_DESIGN.md):

- M1 transport unification (ten slices): `src/transport.rs` is the
  pub(crate) injectable transport module (declared at src/lib.rs:41 behind
  the ten-feature cfg(any(...)) gate over the nine native providers plus
  `download`), providing the cloneable `Http` handle, `ClientProfile`
  presets, the centralized retry/backoff engine reusing the `retry.rs`
  classifiers and the `MockTransport` test seam. All nine native provider
  rows above are migrated (netease get_json/eapi, qqmusic raw_get and
  musicu_inner with the redirect-none login client, kugou raw_get, kuwo
  raw_get, migu both clients plus every single-shot face, bilibili
  request_json; jamendo/apple/fivesing gained unified retry from zero),
  while platform signing/decryption, cookie whitelists and failover walks
  stay provider-side. The js bridge and the youtube extra row predate M1
  and keep their own clients.
- M2 error taxonomy and auth facets (twelve slices): the unified
  `ErrorDetail` taxonomy with the total `category()` classifier
  (src/error.rs, root-exported at src/lib.rs:114) replaced the
  `Error::Provider` catch-alls across the nine providers, the lx-js bridge
  and the youtube extra row (the §4 paragraph above records the youtube
  follow-up; the apple catch-all cleanup is slice
  m2-apple-error-categories-unwired, 2026-11-20). The registry gained the
  per-descriptor auth facet (`record()` derives
  `entry.auth = provider.auth_capability()` at src/registry.rs:155, sticky
  for the Qr upgrade only). Facet facts behind the §5 auth column: kuwo
  overrides `auth_capability()` to the explicit `AuthCapability::Anonymous`
  (src/source_kuwo.rs:1937-1939) while `capabilities.authentication` stays
  false (pinned at :3385) and its web-token `AuthenticationProvider` impl
  (:2132) is not registered into the registry auth slot; fivesing's dead
  auth impl is registered (`register_authentication` at
  src/source_fivesing.rs:96, one of five registration call sites with
  netease :107, qqmusic :156, kugou :107, bilibili :165) with the explicit
  `AuthCapability::None` facet (:510-512); apple maps `login_status` to the
  explicit `AuthState::LoggedOut` fold (src/source_apple.rs:791-794)
  without a registry auth slot; migu's dead SessionStore was removed (no
  `AuthenticationProvider` impl remains).
- M3 model extensions (five slices): StreamInfo gained `size_is_advisory`,
  `checksum_algorithm` and the structured `fallback_endpoints` slot beside
  the kept legacy `fallback_urls` (per-provider producers recorded in the
  §5 stream cells); Track gained the `AudioSpec` `audio` field (producer:
  the local scan only, §6.5; every provider parse site stays `audio: None`,
  the kuwo/netease backfill is an optional follow-up needing live
  fixtures); the two-direction source namespace table landed at
  src/models/namespace.rs; the voicefox-lyric-shape conversion mirrors
  landed at src/lyrics_convert.rs (feature lyrics-conversions); collection
  models gained `Playlist.play_count/creator/link/extra`,
  `PlaylistCategory.provider/extra` and `Chart.updated_description`, with
  the migu faces as the first payload-backed producer (2026-11-20 slice,
  §5 mg cells) and the auth-model side extending `QrLoginStatus` to nine
  states, `QrLoginSession.image_png` (src/provider/traits.rs:432),
  `QrLoginKind` (:405) and `ProviderProbeResult` in src/health.rs.

## 5. Per-column parity matrix (musid-dli status)

| Provider | search | track | stream | lyrics | playlist | album | artist | chart | link | auth |
|---|---|---|---|---|---|---|---|---|---|---|
| wy | LV | LV | LV (anonymous; session untested; 2026-11-19 M3 field parity: the eapi stream parse names its digest with `checksum_algorithm: Some(Md5)`, fallback/other sites stay `None` with no digest data, MODEL_ALIGNMENT_DESIGN.md §2.3, fixture-pinned over the resolve parse path) | LV | LV (user playlists login-gated, untested) | LV | LV | LV | LV | LV (QR to first poll; phone confirmation pending) |
| kg | LV | LV (2026-11-17 record) | LV (anonymous refusal verified; URL delivery needs session, unverified) | LV (KRC word timing) | LV (user playlists login-gated) | LV (2026-11-17 record) | LV (search-based fallback) | LV | LV (2026-11-17 record) | LV (QR to first poll; phone confirmation pending) |
| kw | LV | FV (live probe answered NotFound twice) | LV (anonymous 320k/128k; lossless web tier unverified, human-required; 2026-11-19 candidate parity: mobile+web race, the loser URL rides the structured `fallback_endpoints` slot, and the mobile-exhaustion web fallback reports High/Mp3, MODEL_ALIGNMENT_DESIGN.md §2.7; race request counts live-unverified) | LV (plain LRC) + FV (lrcx word path) | LV | FV (fixtures mirror 2026-11-18 live captures) | deferred (no upstream function) | FV (no chart smoke module) | FV (live probe answered NotFound twice) | anonymous web-token session only, no account login either side; M2: the descriptor auth facet is the explicit `AuthCapability::Anonymous` (src/source_kuwo.rs:1937-1939) with the `authentication` bool false pinned (:3385), and the web-token `AuthenticationProvider` impl (:2132) stays unregistered in the registry auth slot |
| tx | LV | LV | UNV (URL delivery needs login; anonymous refusal 10006 verified live) | LV (crypt:0 plain; QRC 3DES out of scope) | LV (user playlists login-gated) | LV | not implemented (no upstream endpoint) | LV | LV | LV (QQ + WeChat QR; phone confirmations pending) |
| mg | LV | FV (link song branch) | LV (anonymous 128k; listenSong.do fallback dead anonymously, kept; 2026-11-19: resolve candidates ride the structured `fallback_endpoints` slot with their own format tier and paired size, MODEL_ALIGNMENT_DESIGN.md §2.4) | LV (MRC word timing) | LV (2026-11-20 M3 collection-field parity, slice m3-collection-model-fields-zero-producers: searched playlists carry play_count/creator/link from playNum/musicNum/userName-shaped rows, the detail and link faces carry creator/cover from the get_detail_with_meta chains plus the canonical web link, and the category branch stamps category_id into extra; fixture-pinned over tests/fixtures/migu, MODEL_ALIGNMENT_DESIGN.md §5.1) | LV | deferred (no upstream function) | LV (2026-11-20: the desc chain also fills `Chart.updated_description`, the upstream `LeaderboardInfo.update` slot, MODEL_ALIGNMENT_DESIGN.md §5.1) | FV (three link endpoints not live-verified) | not implemented (needs a real account, human-required) |
| bili | FV (anonymous keyword search risk-controlled, verified live; WBI construction fixture-pinned) | LV (BV/av reference expansion) | LV (DASH 320k anonymous; 2026-11-19: the DASH backupUrl mirrors beyond the primary ride the structured `fallback_endpoints` slot with the DASH Referer/UA set, MODEL_ALIGNMENT_DESIGN.md §2.4) | n/a (no upstream lyric API; `Unsupported` at src/source_bilibili.rs:1093-1099) | LV (login-gated favorites; categories/search `Unsupported`) | not implemented (voicefox flag phantom) | not implemented (voicefox flag phantom) | FV (no chart smoke module) | LV | LV (QR create, poll, and phone scan confirmed by user; favorites behind login verified) |
| fivesing | LV | LV (link song branch) | LV (320k three-tier; 2026-11-19: the selected tier's backup URLs ride both the legacy `fallback_urls` slot and the structured `fallback_endpoints` slot with the tier, MODEL_ALIGNMENT_DESIGN.md §2.4) | LV | LV (metadata only; `playlist_tracks` `Unsupported`, JS-rendered) | n/a | n/a | n/a | LV (2026-11-17 smoke links, re-run 2026-11-18; registered 2026-11-20, see 6.1) | registered (M2 2026-11-19): the constant-LoggedOut `AuthenticationProvider` impl serves the registry auth slot with the explicit `AuthCapability::None` facet (register_authentication at src/source_fivesing.rs:96, facet override :510-512, pinned by `auth_slot_is_registered_with_an_explicit_none_facet` :1164); no login flow either side |
| jamendo | LV (endpoint intermittently unreachable) | n/a (containers hang, refusal live-verified) | LV | n/a (`Unsupported`) | LV (search_playlists; playlist-by-id refused live-verified, since M2 as `Unavailable` (the `container_unavailable` refusal, was legacy `Unsupported`)) | n/a (refused live-verified; since M2 `Unavailable` container refusal, was legacy `Unsupported`) | n/a | n/a | n/a (parses kind, then refused live-verified; since M2 `Unavailable` container refusal, was legacy `Unsupported`) | n/a |
| apple | LV | LV | LV (30-second previews only, by contract) | LV | LV | LV | n/a | n/a | LV | no login flow either side; M2 2026-11-19: the `AuthenticationProvider` impl answers the explicit `AuthState::LoggedOut` fold (src/source_apple.rs:791-794, test `login_status_is_the_explicit_logged_out_state_fold` :948) and is not registered into the registry auth slot |
| qianqian | LV | FV (link song branch; live track link smoke-verified) | LV (anonymous 128/320 full files for free tracks with M3 backfill; VIP tracks answer path-less metadata and exhaust into NotFound, live-pinned) | LV (plain LRC via the static taihe CDN) | LV (categories, category lists, detail+tracks; playlist keyword search dead live: type=6 answered errno 23001, ported with upstream parity to an empty result, see 6.4) | LV (album/info; live rows carry assetId without TSID, a documented musid-dli deviation parses them) | deferred (upstream `artist: true` is a phantom flag: no artist function exists in source/src/qianqian/, same class as tx/mg/kw in 6.2) | n/a (no upstream chart face) | LV (song/playlist link kinds) | n/a (no login surface either side) |
| joox | LV (2026-11-19 re-probe: 30 tracks anonymously; only `type=0` accepted, no `section_list` in the live body) | FV (songinfo shape schema-derived from upstream reads; the live face is dead: `web_get_songinfo` answers HTTP 404 empty, live-pinned 2026-11-19) | FV (kbps ladder over the documented shape; live face dead anonymously: HTTP 404 surfaces as `Network`, live-pinned 2026-11-19) | LV (2026-11-19 re-probe: 64 LRC lines; the live `lyric` field is base64-encoded, a documented deviation decoded with plain-text parity) | FV (container metadata + section shapes; live face dead: `/openjoox/v3/playlist` answers nginx 404, live-pinned 2026-11-19) | FV (albums ride the same v3 playlist face; live dead nginx 404, live-pinned) | n/a (no upstream face) | n/a (no upstream face) | FV (song/playlist/album/bare-id router fixture-pinned; song links live-refused via the dead songinfo face, playlist/album links live-refused via the dead v3 playlist face, 2026-11-19) | n/a (no login surface; the upstream `JOOX_COOKIE` env var maps onto the SessionStore cookie injection) |
| soda | not migrated (all ten columns) | not migrated | not migrated | not migrated | not migrated | not migrated | not migrated | not migrated | not migrated | not migrated |
| js (lx) | FV (fake-engine unit tests) | n/a (no contract slot) | FV (fake-engine unit tests) | FV (fake-engine unit tests) | n/a (no contract slot) | n/a | n/a | n/a | n/a | n/a (no login flow by contract) |

## 6. Findings and deltas

### 6.1 musid-dli side gaps

- CLOSED (2026-11-20, slice fivesing-link-resolver-unregistered): 5sing link
  resolution is implemented as `impl LinkResolver for FivesingProvider`
  (src/source_fivesing.rs:680) and was live-exercised by the 2026-11-17 smoke
  (`fivesing_smoke` has a `link` module; docs/HANDOFF.md:162, re-run
  2026-11-18 with link track resolution). `register()` now calls
  `register_link_resolver` (src/source_fivesing.rs:95), the capability block
  sets `link_resolve: true` (:500), and the registration test (:1139) asserts
  both the descriptor bit and `registry.link_provider(ID)`. voicefox declares
  `link_parse: true` (source/src/fivesing/mod.rs:70) with `parse_link` at
  :156: the contract is now matched on the registry surface, gap closed.
- Bilibili `LyricsProvider` exists but deliberately answers `Unsupported`
  with the capability false ("Bilibili exposes no lyric API;
  src/source_bilibili.rs:1093-1099") and is not registered. This mirrors
  voicefox, which ships no bili lyric module. Recorded for completeness; not
  a gap.
- Kugou and Kuwo: `kugou_smoke.rs` currently carries no `link` or `albums`
  module (doc comment at examples/kugou_smoke.rs:7 lists search lyrics
  playlist chart artist stream auth user), while docs/HANDOFF.md:156 records
  albums and links as smoke-verified on 2026-11-17. The 2026-11-18 re-run
  (`kugou_smoke search lyrics playlist chart artist`, IMPLEMENTATION_STATUS
  log) did not repeat them. Treat the kg album/link `LV` cells as recorded
  once, not re-confirmed; the code is registered and fixture-pinned
  (tests/fixtures/kugou link_targets.json).

### 6.2 voicefox side phantom capability flags

Four voicefox providers declare capability bits with no matching
implementation, so the voicefox UI can surface entries that error upstream.
musid-dli declares these bits false and records why:

- tx `artist: true` (tx/mod.rs:67) with no `get_artist_songs` anywhere in
  source/src/tx/. musid-dli: no `ArtistProvider`, recorded "no upstream
  endpoint exists; capability stays false" (docs/IMPLEMENTATION_STATUS.md:29).
- mg `artist: true` (mg/mod.rs:55) with no artist function in source/src/mg/;
  the only hits are the flag, `singerList` parsing (mg/song.rs:55) and the
  playlist creator derivation (mg/parse.rs:140-147). musid-dli: deferred,
  same finding (docs/IMPLEMENTATION_STATUS.md:32); the derivation itself is
  carried on this side since 2026-11-19 (the album face lands it in
  `Album.artists`, and the playlist faces carry the get_detail_with_meta
  chains, see the §5 mg playlist cell).
- kw `artist: true` (kw/mod.rs:55) with no artist function in source/src/kw/;
  the only hit is the cover fallback `fetch_artist_pic_url` (kw/url.rs:120).
  musid-dli: deferred (docs/IMPLEMENTATION_STATUS.md:31).
- bili `album: true` / `artist: true` (bili/mod.rs:416-417) with no
  `get_album_songs` or `get_artist_songs` in source/src/bili/. musid-dli:
  both bits false (src/source_bilibili.rs:907-916).

### 6.3 Honest-narrowing deltas (musid-dli declares less than voicefox)

- Jamendo: voicefox declares playlists / playlist_search / album /
  link_parse (jamendo/mod.rs:356); musid-dli keeps `playlists`, `albums`,
  `link_resolve` false because the container endpoints (`/api/tracks`,
  `/api/albums`, `/api/playlists`) hang anonymously (live-verified; comment
  at src/source_jamendo.rs:364, recorded at docs/PROVIDER_EVALUATION.md:11).
  Keyword playlist search itself is implemented and live-verified. The trait
  impls exist and answer `Unavailable` (`container_unavailable` at
  src/source_jamendo.rs:184, refusal impls :444-542; the M2
  recategorization of the legacy `Unsupported` refusal, with the refusal
  text preserved in the detail slot and the capability bits unchanged).
- Apple Music: `stream_resolve: true` but every URL is a 30-second preview
  (comment at src/source_apple.rs:589-603; upstream contract with
  `PREVIEW_HINT`, docs/PROVIDER_EVALUATION.md:44-46). Full tracks are DRM
  and out of contract on both sides.
- Kuwo: no account authentication on either side (voicefox declares no
  login; musid-dli asserts `!authentication` at src/source_kuwo.rs:3385,
  with the M2 descriptor facet naming the mechanism that does exist,
  `AuthCapability::Anonymous` at :1937-1939). Kuwo
  session cookies can only be attached through the SessionStore, and the
  web-signed lossless tier confirmation is human-required
  (docs/HUMAN_TEST_CHECKLIST.md section 6).

### 6.4 Container-source migration status: qianqian and joox (migrated 2026-11-19), soda (not migrated)

qianqian left this group on 2026-11-19: the dual-source re-probe commissioned
for the migration found the service revived and the slice landed
(`src/source_qianqian.rs`, feature `source-qianqian`). joox left it the same
day: the re-probe performed for the slice found the service **partially
revived** and the port landed (`src/source_joox.rs`, feature `source-joox`),
with the dead faces ported at upstream parity and live-refused (see the §5
row). soda is still a user-enabled source in VoiceFox and musid-dli has no
port. The V-phase adapter layer must preserve the VoiceFox self-implemented
path (`source/src/soda/`) in the host until and unless a musid-dli port
lands; musid-dli records it as deferred with the following in-place reasons
(do not re-derive):

- qianqian (historical record): docs/PROVIDER_EVALUATION.md:15-21. The
  signing recipe (MD5 of sorted pairs plus fixed secret, appid 16073360) is
  portable, but the 2026-11-17 viability probe found the search paths
  (`/api/search/advanced`, `/wsearch/song`) answering 404 or encrypted blobs
  anonymously: endpoints moved or need session state. **Resolution
  (2026-11-19)**: the anonymous re-probe of the audited `/v1/*` surface
  (the one source/src/qianqian/ actually calls) answered HTTP 200
  `errno 22000` for song search, album search, the playlist category
  directory, category lists, playlist detail, song info and album info, and
  the port landed the same day (module docs in src/source_qianqian.rs cite
  the probe; docs/PROVIDER_EVALUATION.md and the provider row in
  docs/IMPLEMENTATION_STATUS.md carry the live findings). Three live findings
  shaped the port: the tracklink signing now requires `rate` inside the
  signed text (upstream excluded it: `errno 22001 签名错误:sign无效`); VIP
  tracks answer path-less tracklink metadata anonymously (the ladder
  exhausts into `NotFound`, upstream parity); playlist keyword search
  (`/v1/search?type=6`) is refused live (`errno 23001
  不被支持的搜索类型(10000)`) and the ported face keeps upstream parity
  (refusal parses to an empty result), so that cell is live-refused rather
  than live-verified. The upstream `artist: true` flag stayed phantom
  (no artist function in the 8 files) and is declared false like the tx/mg/kw
  cases in §6.2.
- joox: docs/PROVIDER_EVALUATION.md:23-27. OpenJOOX v2 answered `9007001
  parameters type error` for every documented parameter shape and
  `web_get_songinfo` returned an empty body; the anonymous surface had
  closed. **Resolution (2026-11-19)**: the anonymous re-probe performed for
  this slice found the service **partially revived** and the port landed the
  same day (module docs in src/source_joox.rs cite the probe). Live findings:
  search (`/openjoox/v2/search_type?type=0`) is alive with real tracks (only
  `type=0` is accepted; `type=4` still answers the recorded `9007001`), but
  the live body carries **no `section_list`**, so the upstream playlist
  extraction from search parses to an empty result (upstream parity, the
  playlist_search cell is live-empty rather than live-verified); `web_lyric`
  is alive but the `lyric` field is **base64-encoded** (upstream reads it as
  plain text: a documented deviation, decoded with plain-text parity);
  `web_get_songinfo` (stream/track detail) answers HTTP 404 with an empty
  body and `/openjoox/v3/playlist` (playlist/album containers) answers nginx
  404 on the cache host and 403 on the api host, so the stream/track/
  playlist/album faces are ported at upstream parity and live-refused
  (fixtures `playlist_dead_404.html` and the schema-derived
  `songinfo_stream.txt` pin the shapes). The upstream `JOOX_COOKIE` env-var
  cookie injection maps onto the musid-dli SessionStore (cookies stored under
  the `joox` provider id ride the OpenJOOX requests). The upstream caps
  (playlists/playlist_search/album/link_parse, joox/mod.rs:103) are kept true
  since all four faces are ported and call real endpoints; the per-column
  live status is recorded in the §5 row.
- soda: docs/IMPLEMENTATION_STATUS.md:12 and :36, and item 4 (:46) "Soda
  stays out until a host-side CENC decryption contract exists". Protected
  tracks come back as CENC-encrypted MP4s that must be decrypted to a local
  file with playAuth (source/src/soda/play.rs), which conflicts with
  musid-dli's StreamInfo-only contract (the library returns URLs and never
  materializes local audio). musid-dli has no soda module at all; upstream
  itself notes soda has no playlist-category directory
  (source/src/soda/mod.rs:128-134, asserted at manager.rs test).

Independent re-probe of record (2026-11-19, M4 full-check batch): both
sources were anonymously re-probed a second time, independent of the two
migration probes above, and both revival verdicts re-confirmed; the full
evidence is in docs/PROVIDER_EVALUATION.md and only the shape is repeated
here. qianqian: 5 signed GETs, one per endpoint, no credentials
(`python3 -I /tmp/qq_scripts/qq_probe.py` replicating the voicefox MD5
recipe with appid 16073360): homepage 200; `/v1/search?type=1`
(word=周杰伦) 200 `errno 22000` with 7 typeTrack rows carrying the complete
29-key song structure (sample TSID T10064987120 菊花台, duration 341 s,
rateFileInfo including the 3000 lossless tier); `/v1/song/info` 200 with
the same key set; the two 2026-11-17 dead paths (`/api/search/advanced`,
`/wsearch/song`) still 404. Tracklink was outside that probe's budget (the
tracklink findings above stay attributable to the migration probe), and the
forced-gzip response behaviour was re-confirmed (`Accept-Encoding: identity`
still returns `Content-Encoding: gzip`). joox: 10 anonymous curl GETs
(upstream-style non-credential identity headers): `search_type` 200 in two
parameter shapes (`error_code 0`, real tracks with the full parse field
set, no `section_list`), `/openjoox/v3/playlist` 404 nginx in both shapes,
`web_get_songinfo` 404 empty in all three shapes including the real song id
from the search result, and `web_lyric` 200 (`code:0`) with the
base64-encoded `lyric` field decoding to LRC for the real music id. Both
re-probes agree with the per-face live statuses recorded in §5 and §7; no
verdict changed.

### 6.5 js and local

- js: voicefox wraps lx-music custom source scripts (source/src/js/,
  extraction rules in js_source.rs). musid-dli's equivalent is
  `JsBridgeProvider` (src/lx_js_provider.rs), which translates the JSON-lines
  `search` / `musicUrl` / `lyric` contract into `TrackSearch` /
  `StreamResolver` / `LyricsProvider`; it is unit-tested against a fake
  engine (`cargo test --locked --features lx-js-bridge`, 41 unit + 1
  integration recorded 2026-11-18, docs/IMPLEMENTATION_STATUS.md log; the
  capability-contract test re-run in this audit passed). No online smoke
  exists or is possible anonymously; loading a real user script and managing
  the engine process lifecycle stays a host-side step (recorded slice
  lx-host-engine-wiring, not executed). The V-phase adapter must keep the
  VoiceFox js path working in the host until that wiring lands.
- local: voicefox `LocalSource` is a full `MusicSource` with in-memory search
  (source/src/local/mod.rs:677-700) and all-false capabilities
  (asserted at manager.rs:1355-1357). musid-dli's `src/local.rs` (default
  feature) provides filesystem scan and metadata reading
  (`LocalLibraryProvider`, `scan_audio_files`, `read_metadata`,
  local.rs:35-117) and no provider trait row. Parity is at the library level;
  playlist/player behaviour stays host-side. Since 2026-11-19 (parity slice
  m3-audio-spec-zero-producers) the scan also carries the measured audio
  specs: `read_metadata` fills `LocalMetadata.audio: Option<AudioSpec>` from
  lofty (zero-value filter, container losslessness rule with the MP4
  bit-depth heuristic, local.rs:118-163), pinned over the committed
  tests/fixtures/local audio fixtures (tests/fixtures/local/README.md records
  the ffmpeg provenance and the probed lofty ground truth); this makes the
  local scan the record producer of `Track.audio` (MODEL_ALIGNMENT_DESIGN.md
  §3.3). No musid-dli provider fills `Track.audio` (all parse sites stay
  `audio: None`): the kuwo N_MINFO / netease level backfill stays an optional
  provider slice needing live fixtures, LOCAL_LIBRARY_PARITY.md L13 records
  the specs half as closed while L14 (tier classification) stays a gap.

## 7. Verification-state summary

| State | Providers / columns |
|---|---|
| implemented + live-verified (dated smoke records exist) | wy (all columns, anonymous scope), kg (all except track-detail nuance), kw (search/stream/lyrics-plain/playlist), tx (search/track/lyrics/playlist/album/chart/link + QR), mg (search/stream/lyrics/playlist/album/chart), bili (track/stream/playlist/link/auth), fivesing (all implemented columns), jamendo (search/stream + verified refusals), apple (all implemented columns), qianqian (search/stream-free/lyrics/playlist/album/link, anonymous scope, 2026-11-19 smoke; playlist keyword search live-refused), joox (search/lyrics/link-routing anonymous-live 2026-11-19; the stream/track/playlist/album faces are live-refused by dead endpoints and recorded as such in §5) |
| implemented + fixture-verified only (live path unverified) | kw track (songinfoandlrc fixture), kw lrcx word lyrics (formula-derived vector), kw album (live-capture mirrors), kw chart, kw link, mg track (link song branch), mg link, bili chart, js bridge (fake engine), qianqian numeric-albumid2psid swap (shape port, no live sample), qianqian stream preview/trail fallback (no live trail capture), joox stream kbps ladder (schema-derived songinfo shape, no live capture), joox container metadata/section shapes (dead live, refusal pinned) |
| implemented + unverified | tx stream URL delivery (needs account), kg stream URL delivery (needs session), youtube music (search/playback) |
| deferred (reasons on file) | kw artist, mg artist, tx artist, qianqian artist (upstream flag phantom: no artist function in any of the four source trees), mg login (needs real account), QRC 3DES (no verifiable key reference), Kuwo lossless web-tier confirmation (needs entitlement session) |
| not migrated (deferred, in-place records) | soda (qianqian and joox left this row 2026-11-19) |

## 8. Reproduction

Commands executed for this audit (library-level, no live endpoints):

```
cargo test --locked --no-default-features
# 69 passed / 0 failed (+1 qr_terminal_png integration test)

cargo test --locked --features source-netease,source-qqmusic,source-kugou,\
source-kuwo,source-migu,source-bilibili,source-jamendo,source-apple,\
source-fivesing --lib registration
# 12 passed / 0 failed (9 per-provider registration tests + 3 registry
# registration-semantics tests), 491 filtered out

cargo test --locked --features lx-js-bridge --lib capabilities
# 2 passed / 0 failed (capabilities_match_the_contract + the registry
# overwrite/merge test), 119 filtered out
```

Git provenance (read-only commands only): musid-dli HEAD 919f1de
(`ci: add GitHub Actions workflow ...`), origin
https://github.com/emoeem/musid-dli.git, working tree carries the uncommitted
batch listed by `git status --short`: the 2026-11-19 M1/M2/M3 milestone
landing (transport.rs, the M2 error/auth/registry surface, the M3 model
extensions), the 2026-11-20 parity slices (fivesing link registration,
m2-apple and m2-youtube error categories, m3 migu collection producers,
m3-audio-spec local producer) and the 2026-11-18 provider batch; voicefox
HEAD dd116b1 on branch main (unchanged). The full feature matrix, clippy and
smoke re-runs are executed
by the outer script and by CI (/.github/workflows/ci.yml, gate job `ci-gate`
requiring fmt, clippy, test, features, msrv, build, docs, deps); CI verifies
library-level facts only and never live endpoints.
