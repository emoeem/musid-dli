# MODEL_ALIGNMENT_DESIGN.md

M3 milestone design: serde-compatible model extensions and the conversion
contracts between musid-dli `src/models/`, the download engine
(`src/download.rs`), and the VoiceFox host model set. Written 2026-11-19 from a
live read of both trees (musid-dli editable, voicefox read-only) and the
2026-11-19 inventory recorded in the M3 ask. Supersedes nothing; extends
`MODEL_MAPPING.md` (M0 baseline) with the drift corrections that inventory
recorded (line anchors after M1/M2, `Track.isrc` reverse gap, G8 nine-state
correction, `toggle_source` dead-slot downgrade, kuwo fallback regression).

Landing status (2026-11-19, documentation pass): M3 is landed and gate-passed,
five slices closed: s1 `stream-semantics`, s2 `track-conversions`, s3
`lyrics-conversions`, s4 `collection-models`, s5 `auth-model-gaps` (commands
and counts per slice in §9; slice sections §2 to §6; feature rows in
`FEATURE_DEPENDENCY_MATRIX.md` §1). Everything in the §8 register stays
deferred with its owner slice; the third review round (pass 2) was
documentation-only plus the one code comment fix recorded in §9.

Every row below cites the file and line read this session. Baseline reads that
ground the design:

| Read | Purpose |
|---|---|
| `src/models/*.rs` (all 8 files, 234 lines total) | Current model shapes |
| `src/download.rs:60-81` `DownloadRequest`, `:953-975` `verify_size`, `:978-1004` `verify_md5_sync`, `:1082-1101` `candidate_download_urls`, `:441-520` candidate loop (per-candidate metadata reads `:470-496`) | Engine contract the models must feed |
| `voicefox/core/src/traits/source.rs:80-103` `SongUrl`, `:13-17` `SearchResult`, `:24-34` `ParsedLink`, `:42-70` `SourceCapabilities`, `:303-311` `normalize_artist_name` | Upstream shapes |
| `voicefox/core/src/model/{song,source,lyric,playlist,leaderboard,login}.rs` | Upstream shapes |
| `voicefox/source/src/{tx/url.rs:103-140, kg/url.rs:135-155+295-315, mg/url.rs:405-520, kw/url.rs:237-275, apple/mod.rs:80-98, fivesing/mod.rs:100-118, jamendo/mod.rs:405-420, bili/url.rs:60-70, js/js_source.rs:322}` | Advisory parity anchors |
| `src/provider/traits.rs:1-56, 103-108, 178-188, 343-370` | Provider faces (unchanged this milestone) |

## §0 Scope and slice order

Implemented in this milestone (M3 core, all inside `src/models/` plus one
additive adapter in `src/download.rs`):

| Slice | Implemented |
|---|---|
| s1 stream-semantics | `StreamInfo` gains `size_is_advisory`, `checksum_algorithm`, `fallback_endpoints`; `FallbackEndpoint` + `ChecksumAlgorithm` types; advisory parity at the qqmusic/kugou/migu/apple/fivesing/jamendo resolve sites; `StreamInfo` to `DownloadRequest` conversion (`download_request_from_stream`, feature `download`) |
| s2 track-semantics | `AudioSpec` + `Track.audio` slot; `models::namespace` provider alias table; `Track::sorted_qualities` normalization helper (BTreeSet to Vec contract) |
| s3 lyrics-semantics | No model change needed (both directions are conversion work, §4); `LyricFormat` already carries the provenance the conversions need. Implemented in `src/lyrics_convert.rs` behind the `lyrics-conversions` gate feature (implies `lyrics`): field-faithful mirrors of the voicefox shapes + both conversion directions + the `YrcWord`↔`LyricWord` mapping |
| s4 collection-models | `Playlist` gains `play_count`/`creator`/`link`/`extra`; `PlaylistCategory` gains `provider`/`extra`; `Chart` gains `updated_description` |
| s5 auth-model-gaps | `QrLoginStatus` reaches the G8-corrected nine states with `is_transient`/`is_terminal` and upstream snake_case serde; `QrLoginSession` gains `image_png` + `kind` (`QrLoginKind`, both `#[serde(default)]`); the `QrLoginPoll` ↔ voicefox `QrLoginResult` fold rule is pinned as `QrLoginPoll::user_name`; `ProviderProbeResult` (G10) lands next to the breaker snapshot in `src/health.rs`. Surface: `src/provider/traits.rs` + `src/health.rs` + the five QR session construction sites (netease/kugou/bilibili/qqmusic ×2) |

Gate evidence (landed 2026-11-19, per the M3 record): each slice passed
test/clippy at its own gate feature: s1 `stream-semantics`, s2
`track-conversions`, s3 `lyrics-conversions`, s4 `collection-models`, s5
`auth-model-gaps`; per-slice commands and counts in §9.

Designed but deferred (each carries its anchor; see §8): kuwo candidate
parity regression, engine consumption of per-endpoint candidate metadata,
provider producers for `isrc`/`audio`/`updated_description`/collection
detail fields, `ProviderArtwork` face. (`image_png` left this list
2026-11-19: the QQ create sites fill it from the bytes already in hand,
slice `m3-qr-image-png-not-backfilled`; netease/kugou/bilibili stay
`None` per their H5-link returns, §6.)
(s3 conversion functions moved from "deferred, host-side" to implemented
in `src/lyrics_convert.rs`: voicefox types still do not exist here, so the
module carries mirrors of the host shapes; the §4 rules are the contract.)

## §1 Serde compatibility strategy

Rules that every model change in this milestone obeys. The hard gate: JSON
produced by the pre-M3 crate must deserialize into the post-M3 types with
identical semantics (additive-only migration).

- **R1 additive fields only, every new field `#[serde(default)]`.** A missing
  key deserializes to the documented default. Applied: `StreamInfo`
  (`size_is_advisory`, `checksum_algorithm`, `fallback_endpoints`), `Track`
  (`audio`), `Playlist` (`play_count`, `creator`, `link`, `extra`),
  `PlaylistCategory` (`provider`, `extra`), `Chart` (`updated_description`).
- **R2 no renames, no removals, no `deny_unknown_fields`.** Verified by grep
  before design: no struct under `src/models/` or `src/` sets
  `deny_unknown_fields`, so unknown keys (including future voicefox-only keys
  on host-piped JSON) are ignored, not errors.
- **R3 new enums reach the wire only through defaulted fields.**
  `ChecksumAlgorithm` and `FallbackEndpoint` are constructed only inside
  `fallback_endpoints`/`checksum_algorithm`, both defaulted, so old JSON
  cannot name them. Serde casing: `#[serde(rename_all = "snake_case")]` on
  enums that cross the host boundary (`ChecksumAlgorithm` variants are already
  single lowercase words; the attribute is still applied so future
  multi-word variants cannot drift). `StreamQuality`/`AudioFormat`/`LyricFormat`
  keep their existing (default) casing: renaming them would violate R2.
- **R4 defaults carry the legacy semantics, not zero values:**
  `size_is_advisory` default `false` = today's exact-size behavior;
  `checksum_algorithm` default `None` = interpret `checksum` as MD5 (today's
  only verified algorithm, `verify_md5_sync` `download.rs:978-1004`);
  `fallback_endpoints` default empty = the `fallback_urls` string slot keeps
  today's meaning. A legacy JSON therefore behaves byte-identically through
  the engine.
- **R5 no-default-features boundary preserved.** `src/models/` stays
  feature-free: new model code uses only `serde` + `std` (verified: `cargo
  test --locked --no-default-features`, §9). The only
  `StreamInfo`-consuming addition outside `src/models/` is
  `download_request_from_stream` inside `src/download.rs` behind
  `#[cfg(feature = "download")]` (DownloadRequest lives there and
  `DownloadRequest` is not a serde type, so no wire-format impact).
- **R6 no platform-private fields in public models.** Provider-specific
  routing/detail data rides the generic `extra: BTreeMap<String, String>`
  slots (Playlist, PlaylistCategory, Track: the Track slot already exists,
  `track.rs:90`). `FallbackEndpoint` carries only generic transport metadata
  (url/headers/expiry/size/quality tier); platform shapes (e.g. bilibili
  `backupUrl`, migu cross-tier variants) stay inside the provider modules.
- **R7 no `Default` impls on `StreamInfo`/`Track`.** The inventory notes
  voicefox `SongUrl` derives `Default` (`traits/source.rs:79`) while musid-dli
  models do not. Kept as-is deliberately: literal exhaustiveness is the review
  gate that forces every construction site to consider each new field. This
  milestone exercised it; the site counts below are reproducible by counting
  each inserted marker line over `src/` excluding `src/models/` and
  `src/download.rs` (compiler-enforced exhaustiveness, §9):
  StreamInfo 17 sites via `size_is_advisory` (14 production sites in 11
  provider files plus 3 test mocks `cache.rs:112,133`, `resolve.rs:269`);
  Track 36 sites via `audio: None,`; Playlist 21 sites via `play_count: None,`;
  PlaylistCategory 11 sites via `provider: Some(ID.to_owned()),`;
  Chart 6 sites via `updated_description: None,`.
- **R8 wire-format identity with voicefox where the same object crosses the
  host boundary.** `AudioSpec` field names match voicefox `AudioProperties`
  (`source.rs:256-266`) key-for-key (`bitrate`, `sample_rate`, `bit_depth`,
  `lossless`) so a host can move a serialized `audio` object across without a
  key map. `Track.audio` uses the voicefox key `audio` (`song.rs:25`). The
  s5 `QrLoginStatus` serializes `snake_case` (`login.rs:13`), pinned in §6.
  Everything else keeps musid-dli's own names, mapped
  explicitly in the tables below.

Forward direction note: serialization output of the new types contains the
new keys. Old readers (pre-alignment voicefox or musid-dli) ignore unknown
keys on deserialize (R2), so the new-to-old direction degrades additively,
never errors.

## §2 Slice s1: StreamInfo stream semantics (landed 2026-11-19, gate feature `stream-semantics`)

### §2.1 New fields and types (`src/models/stream.rs`)

| New item | Definition | Serde | Default semantics |
|---|---|---|---|
| `StreamInfo.size_is_advisory` | `bool` | `#[serde(default)]` | `false` = exact size (current engine behavior, `verify_size` `download.rs:953-975` byte-exact branch) |
| `StreamInfo.checksum_algorithm` | `Option<ChecksumAlgorithm>` | `#[serde(default)]` | `None` = MD5 (R4); mirrors voicefox where the field is literally `md5` (`traits/source.rs:94`) |
| `StreamInfo.fallback_endpoints` | `Vec<FallbackEndpoint>` | `#[serde(default)]` | empty = use legacy `fallback_urls` slot only |
| `FallbackEndpoint` | `{ url: String, headers: Vec<(String,String)>, expires_at_unix: Option<u64>, content_length: Option<u64>, quality: Option<StreamQuality> }` | whole struct only reachable via the defaulted vec | field naming follows musid-dli convention (`content_length`, not voicefox `size`, for symmetry with `StreamInfo.content_length` `stream.rs:100`) |
| `ChecksumAlgorithm` | `enum { Md5, Sha1, Sha256 }` | `#[serde(rename_all = "snake_case")]`, `Default` = `Md5` | `Md5` is the only algorithm the engine verifies today |

`FallbackEndpoint` deliberately has **no** per-endpoint `size_is_advisory`:
advisory is a property of how a source declares sizes, not of one CDN
replica; the stream-level flag applies to every candidate (voicefox has the
same stream-level granularity, `traits/source.rs:89-92`).

Legacy compatibility read-side: `fallback_urls: Vec<String>`
(`stream.rs:50`) is **kept**, not deprecated. Producers fill either slot; a
producer that fills both gets the ordering of §2.4.

### §2.2 Advisory parity (who fills `size_is_advisory`)

Mirrored from the voicefox resolve sites read this session; musid-dli resolve
sites listed with the expression now emitted:

| Provider | voicefox anchor | voicefox value | musid-dli site / value |
|---|---|---|---|
| netease | `wy/url.rs:252,277` | `false` | `source_netease.rs:535,597`, stays `false` |
| qqmusic | `tx/url.rs:122` `matches!(actual_quality, Flac \| Flac24)` | lossless tiers | `source_qqmusic.rs:859`: `matches!(candidate.1, StreamQuality::Lossless \| StreamQuality::HiRes)` |
| kugou | anonymous v3 `kg/url.rs:147` `false`; logged-in v5 `kg/url.rs:308` `true` | per endpoint | `source_kugou.rs:723-733,752`: match split, v5 branch `true`, v3 branch `false` |
| kuwo | `kw/url.rs:266` `false` | `false` | `source_kuwo.rs:2002,2032,2062` (three resolve arms since the 2026-11-19 candidate-parity race), stays `false` |
| migu | `mg/url.rs:502-504` `declared_size.is_some() && matches!(quality, Flac \| Flac24) && achieved_quality == quality` | formula | `source_migu.rs:1577-1581`: same formula over musid locals (`declared_size.is_some() && matches!(options.quality, Lossless \| HiRes) && achieved_quality == options.quality`) |
| bilibili | `bili/url.rs:66` `false` | `false` | `source_bilibili.rs:1076`, stays `false` |
| jamendo | `jamendo/mod.rs:414` `true` (size `None`) | `true` | `source_jamendo.rs:440`: `true` |
| apple | `apple/mod.rs:94` `true` (size `None`) | `true` | `source_apple.rs:624`: `true` |
| fivesing | `fivesing/mod.rs:112` `true` (size `None`) | `true` | `source_fivesing.rs:590`: `true` |
| youtube-music | n/a (musid-only provider) | (none) | `source_youtube_music.rs:240`: `false` (declared `content_length: Some(chosen.size)` is exact per stream) |
| lx-js | `js_source.rs:322` `false` | `false` | `lx_js_provider.rs:307`: `false` |

Note the three `true`-with-`size: None` providers (jamendo/apple/fivesing):
the flag is inert while the size stays `None` (`verify_size` short-circuits on
`expected == 0`, `download.rs:954-956`); it is carried so that a future size
declaration from those sources degrades to advisory rather than to a false
integrity failure, exactly voicefox's belt-and-braces shape.

### §2.3 Checksum algorithm semantics

- Model: `checksum` stays the raw digest string (netease lowercases on both
  sides: `wy/url.rs:137`, `source_netease.rs:529`); `checksum_algorithm`
  names how to verify it.
- Engine contract: only MD5 is verified (`verify_md5_sync`,
  `download.rs:978-1004`, MD5 only by construction). Therefore
  `download_request_from_stream` copies the checksum into `DownloadRequest.md5`
  **only when** the effective algorithm is MD5 (absent or `Md5`); a declared
  `Sha1`/`Sha256` checksum is **dropped**, never reinterpreted as MD5. The
  drop is pinned by test (`download::tests::non_md5_checksum_is_dropped_not_reinterpreted`)
  and listed in §2.5 as a known lossy step. Hosts that want SHA verification
  do it post-download; wiring `Sha256` into `verify_*` is an engine-slice
  decision, not silently smuggled through the MD5 gate.
- Helper on the model so hosts share the exact rule:
  `StreamInfo::md5_checksum(&self) -> Option<&str>`, `src/models/stream.rs:130-150`.
- **Producer backfill (2026-11-19, parity slice
  `m3-checksum-algorithm-not-backfilled-netease`).** NetEase is the only
  provider whose resolve response carries a real digest, and the eapi stream
  site now names it explicitly: `source_netease.rs:542` emits
  `checksum_algorithm: Some(ChecksumAlgorithm::Md5)` iff the response's
  `md5` field is a JSON string, derived from the same `as_str()` read as
  `checksum` itself, so the two slots can never disagree. The dead-fallback
  outer-URL site (`source_netease.rs:598`) keeps `None`: no digest, nothing
  to name. All other production construction sites keep `None` too (no
  digest data to carry; qianqian's `filemd5` site documents the same choice
  in place). Pinned over the real resolve parse path by
  `source_netease::tests::fixture_stream_response_pins_md5_checksum_algorithm`
  (`tests/fixtures/netease/song_enhance_player_url_v1.json`, mock transport);
  the fallback arm's `None` pair is pinned inside
  `resolve_fallback_head_probe_is_single_shot_with_own_timeout`. Behavior is
  unchanged for the engine (`md5_checksum()` folds `Some(Md5)` and `None`
  identically); the backfill makes the declared semantics explicit.
- **Whitespace semantics: a deliberate behavior change, not an engine
  rule match** (review P2). The engine skips only an *exactly-empty*
  expectation (`expected.is_empty()`, `download.rs:979-981`); under the old
  host-side field copy a whitespace-padded or whitespace-only `checksum`
  reached the MD5 gate and **failed** the hash compare. The adapter trims
  first: whitespace-padded digests now verify (`'  abc123  '` becomes
  `'abc123'`, pinned by `download_request_from_stream_maps_every_field`) and
  whitespace-only checksums now skip the check (pinned by
  `md5_checksum_only_for_md5_algorithm`). Both directions
  (fail-to-verify and fail-to-skip) are improvements, but hosts comparing
  behavior against the pre-M3 mapping must know they exist.

### §2.4 Structured fallback endpoints and the ordering contract

Why: both trees' fallback lists are bare `Vec<String>` and the engine applies
the primary's `headers`/`expected_size`/`md5` to **every** candidate
(voicefox `engine.rs:294-377` mirrors musid `download.rs:441-520`: one
`req.headers`/`req.expected_size`/`req.md5` read inside the candidate loop,
`download.rs:470-496`).
Candidates that switch streams (youtube-music fallback URLs are different
bitrates, `source_youtube_music.rs:211`; migu cross-tier candidate
construction spans `source_migu.rs:1470-1496`) therefore break the size gate once metadata
diverges. `FallbackEndpoint` gives the model a place for per-URL metadata;
consuming it inside the engine loop is the follow-up engine slice (§8):
the model lands first so that migration needs no second serde break.

Ordering contract (unchanged from the pinned engine behavior,
`candidate_download_urls` `download.rs:1082-1101`, tests
`download.rs:1549-1603`): primary URL first, then extras in append order,
deduplicated keeping the first occurrence, each followed by its NetEase
node rewrite. `StreamInfo::ordered_fallback_urls()` (new, on the model) is
the slot merger feeding that function:

1. `fallback_urls` entries in order (legacy slot),
2. `fallback_endpoints[].url` in order (structured slot),
3. trim, drop empties, dedup against earlier entries (primary URL not
   included here; `candidate_download_urls` dedups it in).

A producer should fill one slot per resolve; filling both is defined (legacy
strings first) so no candidate can be silently lost.

**Cross-slot duplicate rule, fixed now for the engine slice** (review P3):
`ordered_fallback_urls` dedups across both slots keeping the legacy-first
attempt order and returns bare strings, so on a URL present in both slots
the function's output cannot express which side's metadata would survive.
The merge rule for the future engine consumption is pinned here: the
**structured `FallbackEndpoint` metadata wins** (the legacy string slot
carries no metadata, so dropping the duplicate string loses nothing), while
attempt order stays legacy-first. Consequently the engine slice must merge
a *structured* candidate list (endpoints, marked with which slot supplied
them), not re-derive metadata from the string output of
`ordered_fallback_urls` (§8 register).

**Producer state (backfilled 2026-11-19, parity slice
m3-fallback-endpoints-not-backfilled; the "hazard dormant / no producer
fills" note above is void)**: five producers fill the structured slot from
data already in hand at their resolve sites, each extending its existing
stream fixture/mock tests to pin the slot:

| Producer | Structured slot content | Legacy slot | Slot pins |
|---|---|---|---|
| fivesing | the selected tier's backup URLs (`squrl_backup`/`hqurl_backup`/`lqurl_backup`), quality = the selected tier, no headers/size/expiry on the response | same URLs (dual fill) | `resolve_routes_audio_links_through_the_migrated_face` |
| migu | every non-primary/primary candidate URL with its format tier (`format_type_quality`) and the size paired with its own URL variant, no headers/expiry | same URLs, same order (dual fill) | `resource_post_is_idempotent_and_retries_503`, `resolve_carries_cross_tier_candidates_with_their_own_metadata` |
| youtube-music | every non-chosen audio stream: bitrate-derived tier, declared size, the shared player expiry; no headers | same URLs (dual fill) | `fallback_endpoints_carry_the_per_stream_tier_size_and_expiry` |
| bilibili | the DASH `backupUrl`/`backup_url` mirrors beyond the primary (`baseUrl` or first backup): DASH Referer/UA headers, selected tier; legacy `durl` carries none | empty (one slot) | `dash_fixture_selects_by_requested_quality`, `dash_fixture_backup_url_prefers_camel_case`, `dash_fixture_falls_back_to_legacy_durl` |
| kuwo | the web-signed play URL when the mobile ladder wins (okhttp UA, undeclared tier = `quality: None`), or the mobile tier URL when the web URL wins the lossless primary | empty (one slot) | `resolve_lossless_web_primary_carries_the_mobile_candidate`, `resolve_mobile_primary_carries_the_web_candidate`, `resolve_serves_the_mobile_tier_through_the_unified_layer` |

The remaining production construction sites (`source_apple.rs`,
`source_jamendo.rs`, `lx_js_provider.rs`, `source_netease.rs:542,598`,
`source_kugou.rs`, `source_qqmusic.rs`) stay `vec![]` honestly: their
resolve paths hold no backup URL data to carry (single-URL responses, and
voicefox's equivalents carry none either), so there is nothing to backfill
and no dead field is manufactured. Dual-fill producers keep the engine's
behavior byte-identical (the cross-slot dedup keeps the legacy-first
attempt order); the structured slot is the data source for the engine
slice.

### §2.5 `StreamInfo` to `DownloadRequest` conversion

`pub fn download_request_from_stream(stream: &StreamInfo, dest: PathBuf) -> DownloadRequest`
in `src/download.rs` (feature `download`). `dest` stays caller-owned:
`StreamInfo` is a resolution result with no filesystem target, and the engine
API takes `dest` per request today (`download.rs:60-64`).

| StreamInfo field | DownloadRequest field | Rule | Loss? |
|---|---|---|---|
| `url` | `url` | verbatim | no |
| `headers` | `headers` | verbatim | no |
| `content_length` | `expected_size` | verbatim | no |
| `size_is_advisory` | `size_is_advisory` | verbatim | no |
| `checksum` + `checksum_algorithm` | `md5` | copy iff effective algorithm is MD5 (§2.3); whitespace-padded digests are trimmed, whitespace-only becomes `None` (behavior change vs the naive copy, §2.3) | non-MD5 dropped, by design and pinned |
| `fallback_urls` + `fallback_endpoints` | `candidate_urls` | `ordered_fallback_urls()` (§2.4); the engine still adds NetEase rewrites + dedup via `candidate_download_urls` | per-endpoint `headers`/`content_length`/`expires_at_unix`/`quality` are **not** representable in `DownloadRequest`: the engine consumes one metadata set per request (§2.4); pinned as the known engine gap |
| `max_chunk_size` | `max_chunk_size` | `None` becomes `0` (0 = strict chunking off, `download.rs:75-80`) | no (`Option` to `0` is bijective for the engine's reading) |
| `quality` | (none) | engine is quality-blind (`MODEL_MAPPING.md` §3) | no: host carries it |
| `duration` | (none) | engine has no duration use | no: host carries it |
| `format` | (none) | no engine use | no: host carries it |
| `expires_at_unix` | (none) | engine has no expiry concept; `StreamCache` owns freshness (`cache.rs:40-49`) | no: cache layer carries it |
| `index_range`/`init_range` | (none) | §2.6 decision (a) | no: descriptive, host carries |

Non-goal: no `impl From<StreamInfo> for DownloadRequest`. `dest` has no
default, and a trait impl would invite `.into()` on a type whose most
important field the caller must choose. A named function keeps the call site
greppable (the inventory's G2 asks for exactly one adapter, not an implicit
conversion).

### §2.6 Range/DASH consumption decision

Chosen: **(a) `ByteRange` stays descriptive metadata for playback hosts.**
`index_range`/`init_range` (`stream.rs:118-121`, filled only by youtube-music,
`source_youtube_music.rs:197-205`) describe DASH segment addressing for a
player that demuxes segments itself; the engine's fetch model is
whole-resource Range/multipart (`download.rs:441-520`) plus strict bounded
chunking (`max_chunk_size > 0`, `download.rs:470-496`): it has no
segment-timeline loop. Neither tree reads DASH `SegmentBase` sizes/md5;
the primary-URL pick is still `baseUrl` (or first backup) only on both sides
(voicefox `bili/url.rs:152-161`), but since the 2026-11-19 parity slice
musid-dli carries the parsed backupUrl mirrors of the selected item as
structured `fallback_endpoints` (`source_bilibili.rs:779-828`), so the
"both take baseUrl + first backup only, rest dropped" defect is closed on
this side (the engine still consumes only the primary URL).
Implementing (b) bounded-Range segment fetch is an engine slice with its own
test surface; until one exists, `download_request_from_stream` neither reads
nor drops these fields (they simply do not participate), and this section is
the pinned record that DASH init/index ranges are not download inputs. No
segment-level metadata migration is done (per inventory decision).

### §2.7 kuwo candidate parity: implemented (2026-11-19, parity slice m3-fallback-endpoints-not-backfilled)

Inventory finding (pre-slice): voicefox races mobile + web and keeps the
loser web URL as a candidate (`kw/url.rs:240-256`), musid-dli's two resolve
paths returned `fallback_urls: vec![]` (then `source_kuwo.rs:1940,1963`).
Implemented in this slice: `KuwoProvider::resolve` now runs the web-signed
fetch and the mobile tier ladder concurrently (`futures::join!`, web future
polled first) and the loser URL rides as a structured `FallbackEndpoint`
candidate instead of being dropped. musid keeps its tier preference rather
than upstream's fixed mobile-first rule (the anonymous mobile endpoint
cannot serve lossless, `mobile_tiers` downgrades those requests), so
Lossless/HiRes requests resolve on the web endpoint with the mobile tier URL
as the candidate (carrying the tier it actually serves and the okhttp UA its
CDN requires), and every other tier resolves on the mobile ladder with the
web URL as the candidate (okhttp UA, undeclared tier = `quality: None`).
The web URL also carries the resolve itself when every mobile tier fails
(upstream "网页直链既做候选地址，也在移动端失败时兜底"); on that newly
reachable arm the report follows upstream's documented rule, High/Mp3,
because the web API declares no tier (`kw/url.rs:244-247`), instead of the
registered lossless report below. Live-smoke verification of the race and
its request-count change (every resolve now also sends the web request) is
recorded human-required in PROVIDER_PARITY_MATRIX.md §5.

Second kuwo deviation, registered by review (P4): the tier report. musid's
web-direct path reports `quality: Lossless` + `format: Flac` unconditionally
whenever a lossless tier was requested and the web URL resolved
(`source_kuwo.rs:1933-1934` pre-slice anchors), while voicefox's equivalent
web-direct path reports `High320` + mp3 because the web API does not declare
a tier (comment "网页接口不声明档位，按 mp3 处理", `kw/url.rs:244-247`).
This is pre-M3 provider behavior (M1/M2-era port, not introduced here);
whether musid's web endpoint truly serves lossless needs a live smoke to
adjudicate. The parity slice keeps the lossless-requested web-primary report
unchanged (the registered deviation stays) and applies the upstream rule
only to the newly reachable mobile-exhaustion fallback arm above; the §8
register row stays until a live smoke adjudicates the lossless claim.

## §3 Slice s2: Track semantics (landed 2026-11-19, gate feature `track-conversions`; §3.1 adapter code stays host-side by design)

### §3.1 `SongInfo` / `Track` conversion (host-side adapter spec)

The voicefox types do not exist in this crate and musid-dli must not depend
on voicefox (no path dependency either way; `MODEL_MAPPING.md` §8 note), so
the conversion is a **spec** here; the code lands in the host adapter. Every
row is normative for that adapter.

| voicefox `SongInfo` (`song.rs:12-36`) | musid-dli `Track` (`track.rs:65-91`) | Rule |
|---|---|---|
| `id: String` + `source: SourceId` | `id: TrackId { provider, id }` | `source` through the namespace table §3.2; local songs keep the path in `id` and `"local"` semantics per LOCAL_LIBRARY_PARITY.md, not through this table |
| `name` | `title` | rename |
| `singer: String` ("A、B") | `artists: Vec<String>` | split on the `normalize_artist_name` separator family `、 , & / \|` (`traits/source.rs:303-311`), trim, drop empties; reverse joins `"、"` (`song.rs:17` format comment) |
| `album_name: String` | `album: Option<String>` | empty string maps both ways to `None` (voicefox treats empty as unknown, `traits/source.rs:196-204`) |
| `album_id: String` | `album_id: Option<String>` | same empty-to-None rule |
| `duration: Duration` | `duration: Option<Duration>` | `ZERO` maps to `None` |
| `cover_url` | `artwork_url` | rename |
| `qualities: BTreeSet<Quality>` | `available_qualities: Vec<StreamQuality>` | tier map §3.4; vec is deduplicated, tier-ascending, Unknown last: pinned in-crate by `Track::sorted_qualities` |
| `audio: Option<AudioProperties>` | `audio: Option<AudioSpec>` | §3.3 |
| `extra: HashMap<String,String>` | `extra: BTreeMap<String,String>` | keys are free strings and carry over; BTreeMap gives deterministic serde output (existing Track convention) |
| `toggle_source` | (none) | **not migrated.** Inventory G6 downgrade: upstream dead slot, declared `song.rs:31`, assigned `None` at `song.rs:52`, `runtime/src/storage.rs:82`, `source/src/local/metadata.rs:129`, never persisted (`storage.rs:21`). Semantics live in `resolve_with_alternatives` + `MatchCandidate.score` (`resolve.rs:170-225`, `matcher.rs:12-75`); "identity after switch" stays host bookkeeping |
| `file_path`/`file_ext` | `LocalTrack.path` / `LocalMetadata.format` | out of this table; LOCAL_LIBRARY_PARITY.md owns it |
| (no voicefox field) | `isrc` (`track.rs:82`) | see §3.5 |

### §3.2 Source namespace table (`src/models/namespace.rs`)

Implemented in-crate as the two-direction lookup over one pinned table
(anchors: `SourceId::as_str` values `voicefox core/src/model/source.rs:46-62`
vs the `const ID` sites `source_netease.rs:28`, `source_qqmusic.rs:35`,
`source_kugou.rs:33`, `source_kuwo.rs:43`, `source_migu.rs:42`,
`source_bilibili.rs:51`, `source_jamendo.rs:24`, `source_apple.rs:31`,
`source_fivesing.rs:26`, `source_youtube_music.rs:60,96`,
`lx_js_provider.rs:33`):

| voicefox `SourceId` | musid-dli provider string |
|---|---|
| `wy` | `netease` |
| `tx` | `qqmusic` |
| `kg` | `kugou` |
| `kw` | `kuwo` |
| `mg` | `migu` |
| `bili` | `bilibili` |
| `jamendo` | `jamendo` |
| `apple` | `applemusic` |
| `fivesing` | `fivesing` |
| `soda`, `qianqian`, `joox`, `local` | none (coverage gap / LOCAL_LIBRARY_PARITY.md) |
| (no voicefox source) | `youtube-music`, `lx-js` (reverse gap) |

API: `voicefox_source_to_provider(&str) -> Option<&'static str>` and
`provider_to_voicefox_source(&str) -> Option<&'static str>`. Strict on both
sides: unknown strings return `None`; musid-dli provider strings are valid
input only to the reverse direction. `SourceId::detect_from_link`
(`source.rs:172-203`) stays host-side (M0 conclusion re-verified: musid-dli
`LinkResolver` is provider-internal, `provider/traits.rs:186-188`).

### §3.3 `AudioSpec`: measured audio properties (G5)

New `src/models/track.rs` type + `Track.audio: Option<AudioSpec>`
(`#[serde(default)]`; wire key `audio` = voicefox `song.rs:25`). Field names
match voicefox `AudioProperties` key-for-key (R8):

| Field | Type | voicefox source |
|---|---|---|
| `bitrate` | `Option<u32>` (kb/s) | `source.rs:258-259` |
| `sample_rate` | `Option<u32>` (Hz) | `:260-261` |
| `bit_depth` | `Option<u8>` (bit) | `:262-263` |
| `lossless` | `bool` (`#[serde(default)]`) | `:264-265` |

`AudioSpec::label()` ports the measured-spec label pipeline
(`source.rs:269-281`): lossless gives `"{depth}/{rate_kHz}"` (or
`"{rate_kHz}kHz"` when depth unknown), lossy gives `"{bitrate}K"`, kHz
formatted integer-when-whole / one decimal otherwise (`format_khz`,
`source.rs:285-291`). The Track-level `quality_label` fallback chain (measured
label, then highest tier label, then `-`; `song.rs:62-70`) stays host-side: it
needs the voicefox-facing tier labels (`Quality::label`, `source.rs:236-244`)
which are UI strings, not model data. Producer: local sources only (inventory
decision). The local-scan producer landed 2026-11-19 (parity slice
m3-audio-spec-zero-producers): `local::read_metadata` fills
`LocalMetadata.audio: Option<AudioSpec>` from lofty on every parsed file
(`src/local.rs:93-117`, spec assembly
`audio_spec_for`/`audio_spec_from_parts`/`is_lossless`,
`src/local.rs:118-163`; port of `source/src/local/metadata.rs:74-83,
138-151`: each field filters zero values to `None` while `lossless` is
judged from the **unfiltered** depth, so an MP4 whose ALAC/FLAC branch parsed
a zero still counts lossless; containers Flac/Ape/WavPack/Wav/Aiff are
lossless, Mp4 is lossless iff a depth was parsed, everything else lossy).
Fixture-pinned over committed ffmpeg-built audio files
(tests/fixtures/local: mp3 CBR 128k, flac 16/44.1, flac 24/96, m4a AAC) plus
a runtime WAV stub through `LocalLibraryProvider::scan`. No musid-dli
provider fills `Track.audio` still: all parse sites stay `audio: None` and
hosts copy `LocalMetadata.audio` into `Track.audio`; the provider-side
backfill (kuwo N_MINFO bitrate ladder, netease quality level ladder) stays
an optional follow-up slice needing live fixtures (§8 register).
Deliberately **not** an `extra` string key (inventory: typed slot, not
G11-style keys).

### §3.4 Quality tier map

`StreamQuality` tier order is declaration order with Unknown last
(`stream.rs:2-12`); voicefox order is `QUALITY_ORDER` high to low
(`source.rs:248-253`). Mapping (also drives §4/§5 conversions):

| voicefox `Quality` | musid-dli `StreamQuality` | Note |
|---|---|---|
| `Low128` | `Low` | voicefox default is `Low128`, musid default is `Unknown`: defaults do **not** map to each other; adapters must map values, never default-fill |
| `High320` | `Standard` | netease 320k sits at `High` in musid ladders (`source_netease.rs:414-437`): the ladder tiers are provider decisions; this table maps the **model** enums, one tier each way, and is the only place allowed to do so |
| `Flac` | `Lossless` | |
| `Flac24` | `HiRes` | |
| (no voicefox equivalent) | `High` | musid extra tier |
| (no voicefox equivalent) | `Unknown` | musid extra, sorts last |

### §3.5 Canonical identity (ISRC) decision

`Track.isrc` (`track.rs:82`) is a musid-dli reserve slot with zero producers
(all providers fill `None`; matcher hard gate `matcher.rs:38-44`: ISRC equal
scores 100, unequal rejects; search cache key `search.rs:98-99`). Decision:
**keep the field and the hard-gate semantics, do not require providers to
fill it in M3 core.** Requiring netease/qq `parse_track` ISRC extraction is
provider-slice work needing live fixtures per platform; until a producer
exists the hard gate is inert (both sides of the comparison are `None`-ish
match candidates), which is today's actual behavior. Documented as
host-injectable: a host that knows an ISRC (e.g. from a local library or an
external matcher) may set it and the matcher honors it. Cross-source matching
score systems are **not interchangeable**: voicefox is pure metadata
heuristics (duration ±5 s three-stage retain `manager.rs:1142-1238`, score
`singer30+name30+duration20+contains10` at `:1284-1303`), musid-dli is
`rank_alternatives`/`match_score` (`matcher.rs:12-75`, title40/artist30/
duration25/album5 + bigram similarity). Adapters exchange *candidates*, never
scores.

### §3.6 Local extra keys (G11)

`EXTRA_FILE_MODIFIED_UNIX_NANOS` (`song.rs:8`) and `file_size`/`cue_*`/
`import_url` extra keys translate to `LocalMetadata`/`ContainerTrack` fields
per LOCAL_LIBRARY_PARITY.md at the adapter; no model change here (musid-dli
producers emit none of these keys today; CUE data rides `ContainerTrack`,
`formats.rs:21-35`).

### §3.7 SearchResult ↔ SearchPage (G12)

Shapes read live: voicefox `SearchResult { items: Vec<SongInfo>, total: u32,
has_more: bool }` (`traits/source.rs:13-17`), musid-dli `SearchPage {
items: Vec<Track>, total: Option<u64>, has_more: bool }`
(`provider/traits.rs:104-108`). No model change; same host-side adapter rule
as §3.1:

- `items`: SongInfo↔Track per §3.1.
- Forward (voicefox→musid): `total = Some(u64::from(total))`; u32 to u64 is
  lossless, `has_more` verbatim.
- Reverse (musid→voicefox): `Option<u64>` has no honest `u32` for `None`.
  `None` means "total unknown", not "no results": musid producers emit it
  with possibly non-empty items (`source_bilibili.rs:887-890`, the BV/av
  reference branch), so `None` must **not** become `0`. Pinned adapter rule:
  fall back to `items.len()` (u32-clamped), the same fallback musid's own
  bilibili parse applies when the API omits `numResults`
  (`source_bilibili.rs:931-932`), or surface "unknown" in the UI
  (MODEL_MAPPING.md §6 G12, "None handling required at the UI boundary"). A
  `Some(total)` above `u32::MAX` saturates, never truncates. `has_more`
  verbatim both ways.

## §4 Slice s3: lyrics conversions (landed 2026-11-19 in `src/lyrics_convert.rs`, gate feature `lyrics-conversions`)

Both directions are adapter code (voicefox `LyricData`
`lyric.rs:5-17` does not exist in this crate as a shared type): implemented
in `src/lyrics_convert.rs` behind the `lyrics-conversions` gate feature
(`Cargo.toml:57`, implies `lyrics`) as field-faithful mirrors of the host
shapes (`LyricData`, `LyricLine`, `YrcLine`, `YrcWord`; every mirror field
`#[serde(default)]` per R1), so a host pipes its own structs/JSON in instead
of re-implementing the rules below. musid-dli already has the
richer word model (`LyricWord`/`LyricLine.words`, `lyrics.rs:18-37`) and the
format provenance (`LyricFormat`, `lyrics.rs:5-16`) that upstream only
implies.

### §4.1 voicefox `LyricData` to musid-dli `LyricDocument`

| voicefox | musid-dli | Rule |
|---|---|---|
| `lyric: String` | `original` | verbatim string |
| `tlyric` | `translation` | verbatim |
| `rlyric` | `romanization` | verbatim |
| `lxlyric: Option<String>` (raw YRC/QRC) | `lines[].words: Vec<LyricWord>` | **parse**, do not stuff the raw string: `LyricWord { text, start_ms, duration_ms }` from YrcWord `{ text, start, duration }` (`lyric.rs:39-43`); line `start_ms` from YrcLine.timestamp (`lyric.rs:32-35`); `format` = `Qrc`/`Yrc`/`Krc` per the producer |
| `raw_lrc` | no dedicated slot | **do not add one.** Provenance rides `format` + `original`: the host's `.lrc` writer path (`standard_lyric` `download.rs:1327` + `looks_like_timed_lrc` `download.rs:1350-1365`) only trusts bracketed-timestamp text, so a raw private-format payload must arrive with `format != Lrc` (or `PlainText`) to never be written as `.lrc`. The conversion's obligation: set `format` from the real source format, never default it |
| (no `lines` upstream) | `lines[].text` | derive from the LRC parse of `lyric` when the producer has one; end_ms optional (`lyrics.rs:28-29`), voicefox derives duration from the next line (`lyric.rs:26-27`): on conversion, leave `end_ms = None` rather than inventing durations the parser did not compute |

Implementation contract pinned in `src/lyrics_convert.rs::lyric_data_to_document` (tests
`lyrics_convert::tests`):

- `lxlyric` parse failures are **hard errors** (`Error::InvalidInput`), never silent
  degradation: the raw string has no slot to survive in. Two failure shapes: unparsable
  payload, and a payload that parses without word-level timing (the rendered KRC hybrid
  voicefox kugou emits, `source/src/kg/lyric.rs:170-183`, which lyrics-helper detects as
  plain LRC because its line headers are `mm:ss.xx` not the `[ms,dur]` headers
  `KRC_LINE` requires, `lyrics-core type_helper.rs:35-36`). The documented retry path is
  the same call with `lxlyric = None`, which degrades explicitly to line text.
- `raw_lrc` is promoted to `original` only when `lyric` is blank; otherwise it is
  dropped (the M0-pinned lossy point for this direction). A blank `lyric` plus
  non-LRC `raw_lrc` arrives with the caller's real `format`, so the engine's
  `looks_like_timed_lrc` guard keeps private JSON out of `.lrc` files.
- Word-timed lines and LRC lines merge on equal `start_ms` (the `merge_lyric`
  convention, `download.rs:1293-1324`); a non-blank word-line text wins, LRC-only
  lines keep `end_ms = None`, word-timed lines keep their payload-computed `end_ms`.
- A `lyric` that does not parse as LRC (plain unsynced text) is graceful: the string
  survives verbatim in `original`, `lines` stays empty.

### §4.2 musid-dli `LyricDocument` to voicefox `LyricData` (host compatibility direction)

| musid-dli | voicefox | Rule |
|---|---|---|
| `original` | `lyric` | the engine's `.lrc` adjudication (`standard_lyric`, `download.rs:1327-1348`): verbatim when the original carries bracketed `mm:ss.xx` timestamps, otherwise the parsed lines rendered as LRC (word texts substituted for blank line texts, the explicit word-timing degradation); empty when nothing is usable |
| `original` | `raw_lrc` | the unprocessed payload rides voicefox's "未处理的原始歌词" slot verbatim whenever it is non-blank (producer convention: wy `source/src/wy/lyric.rs:81`, kg `source/src/kg/lyric.rs:184`) |
| `translation`/`romanization` | `tlyric`/`rlyric` | verbatim |
| `lines[].words` | `lxlyric` | stays `None`: no word-timing serializer (YRC/QRC text emission) in M3 core and a synthesized string would be a fake one; the loss is **explicit** (drop word timing, keep line text into `lyric`), never a fake string |
| `lines[].start_ms` | `LyricLine.timestamp` | verbatim |
| `lines[].end_ms` | `LyricLine.duration` | voicefox duration = next line's timestamp minus this one (`lyric.rs:26-27`); last line keeps the document's own convention; `end_ms: None` lines get duration 0 + the next-line rule |
| `lines[].translations`/`lines[].romanization` | (none) | voicefox has no per-line slots in `LyricData`; only the document-level `translation`/`romanization` map through. Per-line entries are dropped explicitly (pinned here, not silent) |
| `format` | (none) | voicefox has no slot; provenance is lost in this direction, pinned as the M0 lossy note |

Not migrated: `LyricState` (`lyric.rs:46-56`) and player snapshots (M0
exclusion, re-verified). musid-only `LyricSearchResult` (`lyrics.rs:52-61`)
stays.

The `end_ms` to duration mapping is `src/lyrics_convert.rs::lyric_lines_from_document`
(`end_ms - start_ms` when present, else next line's `start_ms - start_ms`, else 0;
saturating arithmetic, blank line texts fall back to word-text concatenation matching
`download.rs:1332-1344`).

## §5 Slice s4: collection models (landed 2026-11-19, gate feature `collection-models`)

### §5.1 Implemented field additions

| Struct | New field | Serde | voicefox anchor |
|---|---|---|---|
| `Playlist` (`models/playlist.rs:4-29`) | `play_count: Option<u64>` | default | `voicefox playlist.rs:16` |
| | `creator: Option<String>` | default | `:17-19` |
| | `link: Option<String>` | default | `:20-22` |
| | `extra: BTreeMap<String,String>` | default | `:23-25` (HashMap to BTreeMap, R6; extra carries provider detail-page routing) |
| `PlaylistCategory` (`:32-48`) | `provider: Option<String>` | default | voicefox `source: SourceId` (`playlist.rs:53`): musid names the namespace `provider` (TrackId convention); namespace table §3.2 maps the strings |
| | `extra: BTreeMap<String,String>` | default | `playlist.rs:60-61` |
| `Chart` (`models/chart.rs:4-15`) | `updated_description: Option<String>` | default | `LeaderboardInfo.update` (`leaderboard.rs:9`, "更新频率" text). Separate field, **not** a reuse of `description` (inventory recommendation): description stays free text, update cadence stays structured |

Producers (M3 landing + the 2026-11-20 migu producer slice): the M3 landing
kept every construction site mechanical (`None`/empty at all 21 Playlist, 11
PlaylistCategory, 6 Chart sites), and `PlaylistCategory.provider` was filled
at every site already at landing (`Some(ID)`, the R7 marker line; it never
had a zero-producer window). The migu slice landed the first payload-backed
producers:

| Field | Producer site | Upstream anchor | Pin |
|---|---|---|---|
| `Playlist.play_count` | `migu_parse_searched_playlist` | `mg/playlist.rs:148` (`playNum`, numeric or string) | `searched_playlist_fixture_pins_play_count_creator_and_link` |
| `Playlist.creator` | `migu_parse_searched_playlist` | `mg/playlist.rs:149-153` (`userName`/`ownerName`, find-then-filter) | same |
| `Playlist.link` | `migu_parse_searched_playlist`, `playlist()`, the link playlist branch | `mg/playlist.rs:154`, `:92` | same + `playlist_face_carries_creator_cover_and_link_from_the_detail_page` |
| `Playlist.creator` + `artwork_url` | `parse_playlist_page` chains threaded into `playlist()` and the link branch | `mg/playlist.rs:82-91` (`userName/ownerName/nickName`; `imgUrl/picUrl/cover`) | `playlist_page_fixture_parses_rows_totals_and_titles` (both chain orders + the find-then-filter quirk) |
| `Playlist.extra` | `playlists()` category branch stamps `category_id` | `mg/playlist.rs:227-231` | `category_playlists_stamp_category_id_and_carry_the_searched_fills` |
| `Chart.updated_description` | `collect_boards`, same desc chain as `description` | `mg/leaderboard.rs:100-104,111` (`LeaderboardInfo.update`) | the two `rank_index_*` tests |

Scope notes, each grounded in a read this session: upstream's single text
slot on `LeaderboardInfo` is named `update`, so migu's `updated_description`
carries that desc chain while the pre-M3 `description` mapping stays
(additive, R1/R4), meaning the two slots hold the same text for migu by
design. The square-recommend hot lists stay `None`-filled everywhere: upstream
`collect_playlists` literally writes `play_count: None, creator: None,
link: None` (mg/playlist.rs:281-284) and the payload carries no such fields.
The album container's first-row-singer creator derivation (mg/parse.rs:140-147,
the slice's named first case) has no `Playlist` to land in on the musid-dli
album faces: `AlbumProvider`/`ResolvedLink::Album` carry the `Album` model,
where the same derivation already lands in `Album.artists`
(parse_album_page); the album web link (mg/parse.rs:149) stays dropped
because the `Album` model has no link slot. The remaining providers'
collection-field producers stay open for their own slices (§8).

### §5.2 Album / Artist conventions (no field change)

- `Album`: voicefox single `artist: String` (`playlist.rs:79-85`) vs musid
  `artists: Vec<String>`. Adapter rule: forward `artists = vec![artist]`
  unless empty then `vec![]`; reverse `artists.join("、")`, empty becomes the
  empty string. musid `track_count` is a reverse-extra (voicefox has none):
  passes through as `None` toward voicefox.
- `Artist`: 1:1 (`playlist.rs:88-93` vs `models/artist.rs:4-9`); rename
  cover_url to artwork_url only.

### §5.3 `ParsedLink` / `ResolvedLink`

voicefox `ParsedLink` Song/Playlist/Album with songs vectors
(`traits/source.rs:24-34`) vs musid-dli `ResolvedLink`
Track/Playlist/Album/Artist (`provider/traits.rs:179-184`) + second-call
fetch (`playlist_tracks`/`album_tracks`, `traits.rs:135-137,154-156`).
Adapter decision: **drop the songs vector, resolve lazily**. voicefox
Playlist/Album variants map to musid `Playlist`/`Album` (the musid Playlist
has `tracks` but link resolution leaves it empty today, all 20 literal sites
are `tracks: vec![]`), and the host refetches via the collection traits.
Rationale: prefetching would double-fetch for hosts that follow the musid
trait flow, and the musid `Artist` variant has no voicefox counterpart to
fill. The songs loss is explicit at the adapter, not silent (the host cannot
mistake an empty `tracks` for "playlist is empty": `track_count` carries the
declared size).

### §5.4 Capability mapping (informational table)

| voicefox `SourceCapabilities` (`traits/source.rs:42-70`) | musid-dli `ProviderCapabilities` (`provider/traits.rs:5-22`) |
|---|---|
| `playlists` | `playlists` |
| `playlist_search`, `playlist_categories` | both inside `playlists` (musid is one bit; granularity lives in the trait split `PlaylistProvider`) |
| `album` | `albums` |
| `artist` | `artists` |
| `leaderboard` | `charts` |
| `link_parse` | `link_resolve` |
| `login` | `authentication` |
| `qr_login` | registry descriptor `AuthCapability::Qr` (`provider/traits.rs:32-54`), not a bool |
| `wechat_login`, `vip_account`, `user_playlists` | no slot: auth-slice decisions (§6); `user_playlists` exists as a trait method (`traits.rs:149`) but no capability bit |
| (none) | `download` (dead bit for registry predicates, AUTH_REGISTRY_DESIGN.md §5.6) |

No struct change: the bits are trait surface, and AUTH_REGISTRY_DESIGN.md §5
owns the auth facets.

### §5.5 Artwork face decision

voicefox has `get_cover_url()` (`traits/source.rs:129-130`) plus
`SongUrl.cover_url`; musid-dli has no cover method and keeps artwork on
`Track.artwork_url` (G7). Decision: **no `ProviderArtwork` face in M3
core.** The relocation is documented (a resolve result does not carry the
cover; a host resolving on a matched alternative must take artwork from the
Track it displays, not from the resolve). Adding a trait face is
provider-slice work (every provider would need a live implementation; an
empty default face would violate the no-empty-shells rule, tasks.md §1.3#7).

## §6 Slice s5: QR login + health probe (implemented 2026-11-19, gate feature `auth-model-gaps`)

The change surface is `src/provider/traits.rs` enum + the five QR session
construction sites (netease/kugou/bilibili/qqmusic ×2; the registry Stub
returns errors only and constructs no session) + one additive struct in
`src/health.rs`, not `src/models/`; that is why this slice is separate from
s1-s4. Spec as frozen, now landed:

1. **`QrLoginStatus` nine states** (G8 corrected): add `NetworkError`,
   `InvalidSession` to the existing seven (`provider/traits.rs:350-358`) to
   match voicefox `login.rs:14-33`. Add `#[serde(rename_all = "snake_case")]`
   (upstream wire format, `login.rs:13`). Classification helpers ported:
   `is_transient` = {NetworkError, RiskControl, ServerError} (QR still valid,
   keep polling), `is_terminal` = {Success, Expired, Failed, InvalidSession}
   (`login.rs:36-49`). `From<&QrLoginStatus> for AuthState`
   (`provider/traits.rs:330-340`) extends: `NetworkError` folds to
   `PendingQrScan` (poll continues), `InvalidSession` folds to `LoggedOut`
   (fresh QR needed); the §2.2 transition table of AUTH_REGISTRY_DESIGN.md
   needs no new states, both fold into existing edges.
2. **`QrLoginSession`**: add `kind: QrLoginKind` (Standard/WeChat,
   `login.rs:55-73`) and `image_png: Option<String>` (base64, QQ=PNG /
   WeChat=JPEG, `login.rs:86-89`), both `#[serde(default)]`; existing
   `key`/`url`/`expires_in` (`provider/traits.rs:344-348`) unchanged.
   `QrLoginKind` serializes `snake_case` (`login.rs:56`); `Standard` default
   keeps old stored sessions loadable (R1).
3. **Poll folding**: `QrLoginPoll { status, message, user_id, display_name }`
   (`traits.rs:360-365`) vs voicefox `QrLoginResult { status, message,
   user_name }` (`login.rs:99-106`). Fold rule: `user_name =
   display_name.or(user_id)`: prefer the human-readable name, fall back to
   the account id, never concatenate (the display name is what the UI shows;
   voicefox has a single slot). Reverse direction cannot split `user_name`
   back into the pair: pinned as lossy by design.
4. **Health probe**: new independent struct, not an extension of
   `ProviderHealthSnapshot` (`health.rs:28-31`, a circuit-breaker snapshot).
   Spec: `ProviderProbeResult { provider: String, ok: bool, latency_ms: u64,
   result_count: Option<u32>, detail: String }`: voicefox `SourceHealth`
   (`source.rs:5-12`) with `result_count` optional (musid probe paths that do
   not search have no count; voicefox's probe is always a search). None of
   these fields belong on the snapshot: different lifetimes (probe = one
   request, snapshot = breaker state).
5. Multi-account models are already landed by M2 (`ProviderSession`
   expires_at/updated_at/region, AUTH_REGISTRY_DESIGN.md §3.2-3.3); instance
   keys stay §5.7 wiring. This milestone changes neither registry API nor
   session file format.

Landed deltas over the frozen spec (implementation session 2026-11-19,
each verified by a run below):

- `WeChat` serializes `we_chat`, not `wechat`: upstream applies
  `rename_all = "snake_case"` (`login.rs:56`) to the same variant name, so
  the wire value is identical on both sides by construction (§1 R8). The
  first test run pinned `wechat` and failed (`we_chat` observed); the
  expectation was corrected to the upstream value, not the enum renamed.
- `image_png` producer fill landed 2026-11-19 (slice
  `m3-qr-image-png-not-backfilled`, §8 register): the two QQ Music create
  sites had the image bytes in hand (`create_qr_login` receives the PNG
  from ptqrshow, `create_wechat_login` the JPEG from the qrconnect qrcode
  endpoint), so both now fill `image_png` with the same base64 the `url`
  data URI carries: one encode, two slots, with `url` keeping the data URI
  for legacy hosts. The netease/kugou/bilibili create points keep `None`
  per their actual returns: those APIs hand back only an H5 link (the QR
  content the host renders), no image bytes exist in hand, and synthesizing
  one would be a fake producer. Pinned by the extended `create_qr_login_*`
  tests and the new `create_wechat_login_fills_image_png_from_the_jpeg_bytes`
  test.
- `ProviderProbeResult` lives in `src/health.rs` beside the snapshot it
  deliberately does not extend; `result_count: Option<u32>` with
  `#[serde(default)]` covers non-search probe paths (§6.4).
- `AUTH_REGISTRY_DESIGN.md` §2.3's fold bullet is extended in place with
  `NetworkError` to `PendingQrScan` and `InvalidSession` to `LoggedOut`;
  no new `AuthState` variant, no new transition edge.

## §7 Round-trip test plan (what is pinned where)

| # | Test | Location | Pins |
|---|---|---|---|
| T1 | `legacy_json_without_new_fields_loads_with_documented_defaults` (StreamInfo/Track/Playlist/PlaylistCategory/Chart golden legacy JSON) | inline `#[cfg(test)]` in each models file | R1/R4: pre-M3 JSON deserializes; every new field equals its documented default |
| T2 | `round_trip_preserves_every_field` per extended struct (fully populated, incl. `FallbackEndpoint` with headers/expiry/size/quality, `ChecksumAlgorithm::Sha256`, `AudioSpec` full) | same | R1: to_value then from_value then struct equality; also proves serde `Duration` and `Vec<(String,String)>` shapes |
| T3 | `fallback_slot_ordering_and_dedup_is_pinned` (`StreamInfo::ordered_fallback_urls`) | `models/stream.rs` | §2.4: legacy strings first, then endpoints; trim/empty/dedup; primary not included |
| T4 | `md5_checksum_only_for_md5_algorithm` | `models/stream.rs` | §2.3: `None` gives checksum, `Md5` gives checksum, `Sha1`/`Sha256` give `None`, whitespace gives `None` |
| T5 | `download_request_from_stream_maps_every_field` + `non_md5_checksum_is_dropped_not_reinterpreted` + `structured_endpoints_reach_the_candidate_list` | `src/download.rs` tests (feature `download`) | §2.5 field table row by row; None to 0 chunk; endpoint URLs appear in engine candidate order (via `candidate_download_urls`) with NetEase rewrite still applied; nothing applied twice |
| T6 | `namespace_table_is_pinned_both_ways` + unknown-string rejection + `sorted_qualities` dedup/Unknown-last | `models/namespace.rs`, `models/track.rs` | §3.2/§3.1: the table is the only mapping authority; BTreeSet to Vec normalization contract |
| T7 | `audio_spec_label_matches_upstream_pipeline` (lossless `16/44.1`, lossy `320K`, whole vs fractional kHz, None cases) | `models/track.rs` | §3.3 port of `source.rs:269-291` |
| T8 | `qr_login_status_transient_and_terminal_tables_are_pinned` (all nine states, disjointness, the two in-progress states) | `provider/traits.rs` | §6.1: G8 classification, voicefox `login.rs:36-49` |
| T9 | `qr_login_status_wire_format_is_upstream_snake_case` + `qr_login_kind_default_and_wire_format_are_pinned` (nine status literals, `we_chat`, round-trip each) | `provider/traits.rs` | §6.1/§6.2 + §1 R8: upstream casing, `login.rs:13`/`login.rs:56` |
| T10 | `qr_login_session_legacy_json_loads_with_documented_defaults` (pre-s5 key/url/expires_in JSON and upstream JSON incl. the `source` key) + `qr_login_session_round_trip_preserves_every_field` (upstream wire keys) | `provider/traits.rs` | §6.2 + §1 R1/R2/R4: `#[serde(default)]` slots, unknown keys ignored |
| T11 | `qr_login_poll_round_trip_and_user_name_fold_rule` (display_name wins, user_id fallback, both-None stays None) | `provider/traits.rs` | §6.3: missing identity never collapses to an empty string |
| T12 | `qr_login_status_folds_lossily_per_table` (extended with `NetworkError`/`InvalidSession`) | `provider/traits.rs` | §6.1: the nine-to-seven `AuthState` fold |
| T13 | `probe_result_round_trip_preserves_every_field` + `probe_result_json_without_result_count_loads_with_none_default` | `health.rs` | §6.4: voicefox `SourceHealth` wire names, `Option` result_count default |
| T14 | `mp4_losslessness_is_decided_by_the_parsed_bit_depth` + `losslessness_outside_mp4_follows_the_container` + `unresolved_zero_fields_filter_out_but_keep_the_container_losslessness` + the four committed-fixture tests (`spec_mp3_cbr128`/`spec_flac_16_441`/`spec_flac_24_96`/`spec_m4a_aac`, tests/fixtures/local) + `scan_carries_the_audio_spec_on_every_track` | `local.rs` tests | §3.3: the local scan producer reads real lofty measurements (`LocalMetadata.audio`); container losslessness rule, zero filter and the MP4 depth heuristic pinned; the spec flows through `LocalLibraryProvider::scan`/`read_track` |

Test JSON stays inline string constants (these are unit/fixture tests, not
network tests: tasks.md §15.4 separation).

## §8 Not migrated / deferred register

| Item | Anchor | Owner slice |
|---|---|---|
| Engine consumption of `FallbackEndpoint` per-URL metadata (headers/size/expiry per candidate) | §2.4; engine loop `download.rs:441-520` | download-engine slice |
| Cross-slot duplicate metadata survival: structured `FallbackEndpoint` wins over a duplicate legacy string; engine must merge a structured candidate list, not `ordered_fallback_urls` string output | §2.4 (five producers fill `fallback_endpoints` since 2026-11-19: fivesing, migu, youtube-music, bilibili, kuwo; fivesing/migu/youtube-music dual-fill so the rule is live for them) | download-engine slice |
| kuwo candidate parity (web URL as candidate) | §2.7; `kw/url.rs:240-256`; implemented 2026-11-19 (race shape, loser as structured candidate) | done; live smoke of the race still human-required |
| kuwo tier report: musid web path reports `Lossless`/`Flac` unconditionally, voicefox reports `High320`/mp3 (web API declares no tier) | §2.7; `kw/url.rs:244-247` vs `source_kuwo.rs` web-primary arm (kept) and the mobile-exhaustion fallback arm (upstream rule applied) | kuwo provider slice + live smoke (pre-M3 behavior kept for the lossless-requested web-primary arm, needs live adjudication) |
| s3 conversion code (both lyric directions) | §4 | implemented in `src/lyrics_convert.rs` (`lyrics-conversions`); the host consumes the mirror types instead of re-implementing the rules |
| s1 host-side `SongInfo` / `Track` adapter code | §3.1 | host adapter |
| `ParsedLink`↔`ResolvedLink` adapter (songs vector dropped, resolve lazily via the second-call collection traits) | §5.3 | host adapter |
| `SearchResult`↔`SearchPage` adapter (G12 total narrowing) | §3.7 | host adapter |
| `QrLoginSession.image_png` producer fill (QQ PNG / WeChat JPEG bytes already in hand at `source_qqmusic.rs` create sites) | §6 | done 2026-11-19 (slice `m3-qr-image-png-not-backfilled`): both QQ create sites fill it from the in-hand bytes (`url` keeps the data URI); netease/kugou/bilibili keep `None` per their actual H5-link returns (§6 landed deltas) |
| Producers: `isrc`, `audio` (local scan), `play_count`/`creator`/`link`/`extra`, `updated_description` | §3.5, §3.3, §5.1 | `audio` (local scan) landed 2026-11-19 (slice m3-audio-spec-zero-producers): `LocalMetadata.audio` filled from lofty, fixture-pinned (tests/fixtures/local, T14); provider `Track.audio` parse sites stay `None` and the kuwo N_MINFO / netease level backfill remains an optional provider slice needing live fixtures; the migu collection producers landed 2026-11-20 (slice m3-collection-model-fields-zero-producers): `Playlist.play_count/creator/link/extra` and `Chart.updated_description` carry payload-backed migu fills per the §5.1 producer table, fixture-pinned over tests/fixtures/migu; the other providers' collection producers and `isrc` stay open (per-provider slices) |
| `ProviderArtwork` face | §5.5 | provider slice (needs real implementations) |
| `SourceId::detect_from_link` host routing | §3.2 | host (already declared host-side in M0) |
| `AudioProperties` label to UI strings (`Quality::label` fallback chain) | §3.3 | host UI |
| `Sha1`/`Sha256` verification in the engine | §2.3 | download-engine slice |
| `wechat_login`/`vip_account` capability slots | §5.4, §6 | M3 auth slice |
| `SongUrl.cover_url`/`qualities` relocation (G7) | §5.5 | documented, no field migration |
| voicefox `Quality::default()` (`Low128`) vs musid default (`Unknown`) | §3.4 | adapters map values explicitly |

## §9 Verification record

Commands executed in the implementing session (repo root
`/media/emo/code/musid-dli`, all exit 0); the full gate (fmt/clippy/feature
matrix as CI composes them) remains the outer script's responsibility:

| Command | Result |
|---|---|
| `cargo test --locked --no-default-features models::` | 15 passed, 0 failed (models tests T1/T2/T3/T4/T6/T7) |
| `cargo test --locked --no-default-features --features download download::` | 30 passed, 0 failed (engine suite incl. T5, existing tests intact) |
| `cargo test --locked --no-default-features` | 61 lib + 1 integration passed |
| `cargo test --locked --all-features` | 534 lib + 1 integration passed, 0 failed |
| `cargo check --locked --no-default-features` | ok (R5 boundary) |
| `cargo check --locked --no-default-features --features "download,source-netease,source-qqmusic,source-kugou,source-kuwo,source-migu,source-bilibili,source-jamendo,source-apple,source-fivesing,source-youtube-music,lx-js-bridge"` | ok (10 source features + lx bridge + download compile together) |
| `cargo clippy --locked --all-features` | 0 warnings (after fixing `is_multiple_of` in `format_khz`) |
| `cargo fmt --all` | applied before delivery |

Review pass 2 (2026-11-19): reviewer grep re-verification caught 5 stale
anchors (kuwo tier 1933-1934, kuwo fallback_urls 1940,1963, §3.1 isrc
to track.rs:82, youtube 206, migu 1463-1486), one stale code comment
(stream.rs FallbackEndpoint doc, now download.rs:441-520), and two
evidence claims reworded to reproducible commands (§2.4 producer count
with the 13-production/7-test split; §1 R7 site counts via marker-line
greps). Review pass 1: all musid-dli line anchors re-verified by grep
against the delivered tree and refreshed (P1); §2.3 re-records the
whitespace-trim semantics as a behavior change instead of an engine-rule
match (P2); §2.4/§8 register the cross-slot duplicate metadata rule (P3);
§2.7/§8 register the kuwo tier-report deviation (P4). voicefox anchors are
unchanged (read-only tree).

Gate-feature pass (2026-11-19): the s1 gate command is
`cargo test --locked --features stream-semantics`, which requires the feature
to exist, so `Cargo.toml` gained `stream-semantics = ["download"]`
(lines 43-47; the adapter lives behind `download`, so the gate feature implies
it). This is the only Cargo change in the milestone, no dependency moves,
lockfile untouched; enumerated in `FEATURE_DEPENDENCY_MATRIX.md` §1. The same
pass fixed three `clippy::needless_option_as_deref` lints in the `stream.rs`
test assertions (`md5_checksum()` already returns `Option<&str>`, so the
`.as_deref()` calls were no-ops; invisible to the plain-clippy run above,
surfaced by `--all-targets`, which is the CI clippy composition). Re-run after
both changes: `cargo test --locked --features stream-semantics` = 142 lib + 1
integration passed, 0 failed; `cargo test --locked --no-default-features
models::` = 15 passed; `cargo test --locked --no-default-features --features
download download::` = 30 passed; `cargo test --locked --all-features` = 534
lib + 1 integration passed, 0 failed; `cargo check --locked
--no-default-features --features stream-semantics` ok (CI features-job
isolation); `cargo clippy --locked --all-targets --all-features -- -D
warnings` clean; `cargo fmt --all` applied and `--check` clean.

S2 gate-feature pass (2026-11-19): the s2 gate command is
`cargo test --locked --features track-conversions`, which requires the feature
to exist, so `Cargo.toml` gained `track-conversions = []` (nothing gated,
nothing implied: the s2 deliverables, `AudioSpec`, `Track.audio`,
`models::namespace`, `Track::sorted_qualities`, are core models that always
compile). No dependency moves, lockfile untouched; enumerated in
`FEATURE_DEPENDENCY_MATRIX.md` §1. The same pass added §3.7: the s2 ask's G12
item (`SearchResult`↔`SearchPage`, `total` u32 vs `Option<u64>`) had no
pinned spec row; both shapes were re-read this session and the adapter rule
is now pinned (no model change, no code change).

S3 gate-feature pass (2026-11-19): the s3 ask implements the §4 lyric
conversions, so `Cargo.toml` gained `lyrics-conversions = ["lyrics"]`
(line 57; the raw YRC/QRC/KRC word payloads parse through the lyrics facade,
`crate::lyrics::parse_document`, so the gate feature implies `lyrics`) and
`lib.rs` gained the gated `pub mod lyrics_convert` (lines 48-53). No
dependency moves, lockfile untouched; enumerated in
`FEATURE_DEPENDENCY_MATRIX.md` §1. New module `src/lyrics_convert.rs` (~640
lines): mirror types (`LyricData`/`LyricLine`/`YrcLine`/`YrcWord`, voicefox
field names, every field `#[serde(default)]`), `lyric_data_to_document`,
`document_to_lyric_data`, `lyric_lines_from_document`, the
`YrcWord`↔`LyricWord` and `YrcLine`↔`models::LyricLine` `From` impls, and
byte-level mirrors of `standard_lyric`/`looks_like_timed_lrc`/`format_lrc_line`
(`download.rs:1327-1372`, which live behind the `download` feature this
module must not depend on). Fixture shapes probed live this session via a
scratch test (deleted after): LRC parses with `end_ms = None` (no invented
durations), YRC/QRC/KRC parse into words with payload-computed `end_ms`, the
kugou rendered KRC hybrid detects as plain LRC with zero words (pinned as the
hard-error shape, §4.1). Commands executed (repo root `/media/emo/code/
musid-dli`, all exit 0): `cargo test --locked --features
lyrics-conversions` = 97 lib (84 existing + 13 new) + 1 integration passed,
0 failed; `cargo clippy --locked --features lyrics-conversions --all-targets
-- -D warnings` clean (one `collapsible_if` fixed by collapsing to the
repo-standard let-chain); `cargo check --locked --no-default-features
--features lyrics-conversions` ok (CI features-job isolation);
`cargo test --locked --no-default-features` = 61 lib + 1 integration passed
(R5 boundary: the module is not compiled without the feature);
`cargo fmt --all` applied and `--check` clean; narrow re-run after fmt = 13
passed. The s3 conversion decisions the spec left open are pinned in §4.1
(lxlyric hard-error contract, raw_lrc promotion rule, merge convention,
graceful plain-text lyric) and §4.2 (lyric adjudication row, raw_lrc row,
per-line translations/romanization drop row, duration mapping note).
Not run here: the full outer gate (`cargo clippy --all-features` matrix,
feature matrix job) remains the outer script's responsibility.

S4 gate-feature pass (2026-11-19): the s4 gate command is
`cargo test --locked --features collection-models`, which requires the feature
to exist, so `Cargo.toml` gained `collection-models = []` (line 58; nothing
gated, nothing implied: the s4 deliverables, the `Playlist`
play_count/creator/link/extra and `PlaylistCategory` provider/extra and
`Chart` updated_description slots with their inline §7 T1/T2 tests, are core
models that always compile, exactly the s2 shape). No dependency moves,
lockfile untouched; enumerated in `FEATURE_DEPENDENCY_MATRIX.md` §1 (header
range extended to Cargo.toml:18-58). The §5.2-§5.5 decisions (Album/Artist
adapter conventions, `ParsedLink` songs drop, capability granularity table,
no `ProviderArtwork` face) are pinned documentation and ride no code; the
`ParsedLink`↔`ResolvedLink` adapter is now also listed in the §8 register as
host-adapter-owned, matching the s1 `SongInfo`/`Track` row. Anchors re-verified
live this session against both trees: voicefox `Playlist` playlist.rs:9-26
(play_count:16, creator:17-19, link:20-22, extra:23-25), `PlaylistCategory`
playlist.rs:50-62 (source:53, extra:60-61), `Album` playlist.rs:79-85
(artist:84), `Artist` playlist.rs:88-93, `LeaderboardInfo` leaderboard.rs:5-10
(update:9), `ParsedLink` traits/source.rs:24-34, `SourceCapabilities`
traits/source.rs:42-70 (qr_login:60, wechat_login:65, vip_account:69),
`get_cover_url` traits/source.rs:130; musid-dli `ProviderCapabilities`
provider/traits.rs:5-22, `playlist_tracks` :135, `user_playlists` :149,
`album_tracks` :154, `ResolvedLink` :179. Commands executed (repo root
`/media/emo/code/musid-dli`, all exit 0): `cargo test --locked --features
collection-models`; `cargo clippy --locked --features collection-models
--all-targets -- -D warnings` clean; `cargo check --locked
--no-default-features --features collection-models` ok (CI features-job
isolation); `cargo test --locked --no-default-features models::` = 15 passed
(R5 boundary: models feature-free); `cargo fmt --all` applied and `--check`
clean. Not run here: the full outer gate (all-features matrix, feature matrix
job) remains the outer script's responsibility.

S5 implementation pass (2026-11-19): the s5 ask implements §6, so
`Cargo.toml` gained `auth-model-gaps = []` (line 71; nothing gated, nothing
implied: the s5 deliverables, the nine-state `QrLoginStatus` with
`is_transient`/`is_terminal`, `QrLoginKind` + the `image_png`/`kind` slots,
the `QrLoginPoll::user_name` fold, and `ProviderProbeResult`, are core types
that always compile, exactly the s2/s4 shape). No dependency moves
(serde/serde_json are already non-optional), lockfile untouched. Code
surface: `provider/traits.rs` (serde derives + two variants + two helpers +
`QrLoginKind` + two session slots + `user_name()` + the extended
`From<&QrLoginStatus>` fold + tests T8-T12), `health.rs`
(`ProviderProbeResult` + tests T13), and the five QR session construction
sites (netease/kugou/bilibili/qqmusic ×2) gaining `image_png: None` plus
`kind` (`WeChat` on the QQ `create_wechat_login` site, `Standard`
elsewhere); the kugou/bilibili poll matches already had `_` arms and the
smoke example matches break on `Success`/`Expired`/`Failed` with `_`
fallbacks, so no poll-match edits were needed. The first gate run failed 2
of 92 tests: the test had pinned `"wechat"` while serde produces
`"we_chat"`, which is the upstream value for the same derive + variant name
(`login.rs:56`); expectation corrected to upstream (see §6 landed deltas).
Commands executed (repo root `/media/emo/code/musid-dli`, all exit 0):
`cargo test --locked --features auth-model-gaps` = 92 lib + 1 integration
passed, 0 failed (gate); `cargo check --locked --no-default-features
--features "source-netease,source-qqmusic,source-kugou,source-bilibili"` ok;
`cargo test --locked --no-default-features --features
"source-netease,source-qqmusic,source-kugou,source-bilibili"` = 284 lib + 1
integration passed, 0 failed (the gate does not compile the QR provider
modules, so the construction sites are verified separately);
`cargo clippy --locked --features auth-model-gaps --all-targets -- -D
warnings` clean; `cargo clippy --locked --no-default-features --features
"source-netease,source-qqmusic,source-kugou,source-bilibili" --all-targets
-- -D warnings` clean; `cargo clippy --locked --all-targets --all-features
-- -D warnings` clean; `cargo test --locked --all-features` = 555 lib + 1
integration passed, 0 failed; `cargo test --locked --no-default-features` =
69 lib + 1 integration passed (R5 boundary); `cargo fmt --all` applied and
`--check` clean. Not run here: the full outer gate (fmt/clippy/feature
matrix as CI composes them) remains the outer script's responsibility.

Parity slice m3-fallback-endpoints-not-backfilled (2026-11-19): the five
producer fills above landed (fivesing, migu, youtube-music, bilibili dual/
single slot per §2.4 table; kuwo race restructure per §2.7), the dormant
note is void, and the §8 register rows are updated. Commands executed in
the implementing session (repo root `/media/emo/code/musid-dli`; the full
gate remains the outer script's responsibility):

| Command | Result |
|---|---|
| `cargo test --locked --features source-fivesing,source-migu,source-youtube-music,source-bilibili,source-kuwo` | 310 lib + 1 integration passed, 0 failed |
| `cargo clippy --locked --features source-fivesing,source-migu,source-youtube-music,source-bilibili,source-kuwo --all-targets -- -D warnings` | clean (after fixing 3 `field_reassign_with_default` in the new tests) |
| `cargo test --locked --all-features` | 623 lib + 1 integration passed, 0 failed (existing tests intact) |
| `cargo check --locked --no-default-features` | ok (R5 boundary) |
| `cargo fmt --all` then `cargo fmt --all -- --check` | applied and clean |

Not run here: a live kuwo smoke of the race (request counts double per
resolve, the web face now rides every resolve), recorded human-required in
PROVIDER_PARITY_MATRIX.md §5; no online probing was done in this slice.

Parity slice m3-audio-spec-zero-producers (2026-11-19): the §3.3 local-scan
producer landed (`local::read_metadata` fills `LocalMetadata.audio` from
lofty, zero-value filter plus the container losslessness rule with the MP4
bit-depth heuristic, port of `source/src/local/metadata.rs:74-83, 138-151`),
the deferral note in §3.3 is replaced by the landed record, the §7 table
gains T14 and the §8 register row is updated. Ground truth was probed this
session in a scratch crate (outside the repo) with the exact lofty 0.22.4
default options `read_metadata` uses, before the expectations were written;
the four committed audio fixtures were built with ffmpeg 9.0.2 (provenance,
commands, SHA-256 and the probe table in tests/fixtures/local/README.md).
Provider parse sites stay `audio: None` (no construction site changed in
this slice: the only src/ file this slice edits is src/local.rs); the
kuwo N_MINFO and netease level backfill stays an optional follow-up needing
live fixtures. No online probing was done in this slice. Commands executed
in the implementing session (repo root `/media/emo/code/musid-dli`; the full
gate remains the outer script's responsibility):

| Command | Result |
|---|---|
| `cargo test --locked --features local local::` | 8 passed, 0 failed (1 existing + 7 new) |
| `cargo test --locked --features "ncm,local" formats_` | 21 passed, 0 failed (CUE + NCM suites intact, unchanged count) |
| `cargo test --locked --features local` | 99 lib + 1 integration passed, 0 failed (up from 92, the 8 new local tests included) |
| `cargo test --locked --no-default-features` | 69 lib + 1 integration passed, 0 failed |
| `cargo test --locked --all-features` | 631 lib + 1 integration passed, 0 failed (up from 623) |
| `cargo clippy --locked --features local --all-targets` | clean |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | clean |
| `cargo fmt --all` then `cargo fmt --all -- --check` | applied and clean |

Not run here: no provider-side audio backfill (out of slice scope, optional
follow-up); a real-library scan over a user music directory (the voicefox
`#[ignore]` env-gated equivalent, LOCAL_LIBRARY_PARITY.md §5 "real-library
verification" row, still absent on this side). No Cargo.toml or lockfile
change: the producer rides the existing `local` feature and lofty 0.22.4.

Parity slice m3-qr-image-png-not-backfilled (2026-11-19): the §6 landed
delta above is superseded in turn, the §8 register row is closed, and the
five construction sites are split by what is actually in hand: the two QQ
Music create sites (`create_qr_login` xlogin/ptqrshow, `create_wechat_login`
qrconnect) fill `image_png` from the PNG/JPEG bytes the create calls
already receive (one base64 encode, carried by both `image_png` and the
`url` data URI), while netease/kugou/bilibili keep `None` because their
create APIs return only an H5 link (the QR content the host renders) and
no image bytes exist to fill with; each of the three None sites carries an
in-place annotation. The `provider/traits.rs` field doc no longer claims
"all producers keep it None". Tests: the two existing
`create_qr_login_*` tests gained `image_png`/kind assertions, and the new
`create_wechat_login_fills_image_png_from_the_jpeg_bytes` test pins the
WeChat JPEG fill (no create_wechat_login test existed before). Commands
executed in the implementing session (repo root `/media/emo/code/musid-dli`;
the full gate remains the outer script's responsibility):

| Command | Result |
|---|---|
| `cargo test --locked --features source-qqmusic --lib source_qqmusic::tests::create_` | 3 passed, 0 failed (the two extended QQ tests + the new WeChat test) |
| `cargo test --locked --features source-qqmusic` | 186 lib + 1 integration passed, 0 failed (gate) |
| `cargo clippy --locked --features source-qqmusic --all-targets -- -D warnings` | clean |
| `cargo test --locked --no-default-features --features "source-netease,source-qqmusic,source-kugou,source-bilibili"` | 286 lib + 1 integration passed, 0 failed (the three annotation-only sites compile and their suites stay intact) |
| `cargo fmt --all` then `cargo fmt --all -- --check` | applied and clean |

Not run here: live QR creation against the real xlogin/ptqrshow and
qrconnect endpoints (no online probing was done in this slice; the fill is
verified over the mock transports above); no Cargo.toml or lockfile change.

Parity slice m3-collection-model-fields-zero-producers (2026-11-20): the
first payload-backed s4 producers landed for migu per the §5.1 producer
table (searched-playlist play_count/creator/link, detail-page creator/cover
chains threaded into `playlist()` and the link branch with the canonical
web link, the category_id extra stamp, `Chart.updated_description` from the
upstream `update` desc chain), the §8 register row is updated, and the
former "no extraction point in the shared page parser / no model slot"
drops recorded by the 2026-11-18 migu-link-parsing slice are closed for the
playlist faces. `parse_playlist_page` now returns a `PlaylistPage` struct
(title/creator/cover/songs/total); the square-recommend hot-list site and
both album faces keep their pre-existing fills with the upstream-grounded
reasons recorded in §5.1. No online probing was done in this slice. Commands
executed in the implementing session (repo root `/media/emo/code/musid-dli`;
the full gate remains the outer script's responsibility):

| Command | Result |
|---|---|
| `cargo test --locked --features source-migu` | 172 lib + 1 integration passed, 0 failed (4 new tests: the searched-playlist fixture pin, the category_id stamp, the playlist() face fill, plus the extended page/chain pins; existing suites intact) |
| `cargo test --locked --no-default-features` | 69 lib + 1 integration passed, 0 failed (R5 boundary: models feature-free, untouched) |
| `cargo clippy --locked --features source-migu --all-targets -- -D warnings` | clean |
| `cargo fmt --all` then `cargo fmt --all -- --check` | applied and clean |

Not run here: live verification of the searched-playlist/detail fields
against the real endpoints (the fixtures are reconstructed shapes over the
upstream field contract, same derivation class as the existing
playlist_pages vectors; the 2026-11-17/18 live smokes predate the field
fills); no Cargo.toml or lockfile change.
