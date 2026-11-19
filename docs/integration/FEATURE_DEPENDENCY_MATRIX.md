# FEATURE_DEPENDENCY_MATRIX.md

musid-dli feature to dependency to module to binary impact, read live from `Cargo.toml`, `src/lib.rs`, and the source modules on 2026-11-18. Baseline for integration plan M0. No business code modified.

Status vocabulary: implemented / has tests / net verified / CI verified (see MODEL_MAPPING.md header for the baseline commands executed this session).

## 1. Feature definitions (Cargo.toml:18-71, verbatim names)

| Feature | Depends on | Extra deps pulled | Modules compiled | Cargo.toml line |
|---|---|---|---|---|
| default | local, lyrics, session-file | (see rows) | see rows | 19 |
| local | dep:lofty, dep:walkdir, dep:cue-rw, dep:encoding_rs | lofty 0.22, walkdir 2, cue-rw 0.2.1, encoding_rs 0.8 | `local` (`lib.rs:77-78`), `formats_cue` (`lib.rs:68-69`) | 20 |
| ncm | dep:ncm2mp3-core | ncm2mp3-core 0.3.2 (crates.io, pinned) | `formats_ncm` (`lib.rs:71-72`) | 21-23 |
| lyrics | dep:lyrics-helper | lyrics-helper 0.5.1 (default-features = false) | `lyrics` (`lib.rs:26-27`) | 24 |
| lyrics-search | lyrics, lyrics-helper/search | lyrics-helper + its `search` feature | plus gated items in `lyrics.rs` (`lyrics.rs:98-203` cfg blocks) | 25 |
| session-file | dep:tempfile | tempfile 3 | `session_file` (`lib.rs:20-24`) | 26 |
| source-youtube-music | dep:rustypipe | rustypipe 0.11.4 (rustls-tls-webpki-roots) | `source_youtube_music` (`lib.rs:29-30`) | 27 |
| source-netease | reqwest, aes, md-5, hex, tracing, urlencoding | reqwest 0.12 (rustls), aes 0.8, md-5 0.10, hex, tracing, urlencoding | `source_netease` (`lib.rs:32-33`), retry policy branch (`retry.rs:36`) | 28 |
| source-qqmusic | reqwest, sha1, base64, hex, tracing, urlencoding | sha1 0.10, base64 0.22 | `source_qqmusic` (`lib.rs:35-36`), `retry.rs:43` | 29 |
| source-kugou | reqwest, md-5, base64, flate2, tracing, urlencoding | flate2 1 (rust_backend) | `source_kugou` (`lib.rs:38-39`), `retry.rs:50` | 30 |
| source-kuwo | reqwest, md-5, hex, base64, flate2, encoding_rs, tracing, urlencoding, regex | + encoding_rs, regex 1 | `source_kuwo` (`lib.rs:41-42`), `retry.rs:57` | 31 |
| source-migu | reqwest, md-5, base64, flate2, tracing, urlencoding, regex | as listed | `source_migu` (`lib.rs:44-45`) | 32 |
| source-bilibili | reqwest, md-5, tracing, urlencoding | as listed | `source_bilibili` (`lib.rs:47-48`) | 33 |
| source-jamendo | reqwest, sha1, hex, tracing, urlencoding | as listed | `source_jamendo` (`lib.rs:50-51`) | 34 |
| source-apple | reqwest, tracing, urlencoding | as listed | `source_apple` (`lib.rs:53-54`) | 35 |
| source-fivesing | reqwest, tracing, urlencoding | as listed | `source_fivesing` (`lib.rs:56-57`) | 36 |
| lx-js-bridge | (none) | none | `lx_js`, `lx_js_engine`, `lx_js_provider` (`lib.rs:59-66`) | 37 |
| download | tokio/fs, tokio/io-util, reqwest, lofty, md-5, tracing, lyrics | reqwest, lofty, md-5; tokio gains fs and io-util on top of base sync/time/rt (`Cargo.toml:50`) | `download` (`lib.rs:93-94`) | 38-42 |
| stream-semantics | download | none beyond `download`'s (M3 s1 gate feature, `MODEL_ALIGNMENT_DESIGN.md` §0: StreamInfo stream semantics + the `download_request_from_stream` adapter) | `download` (`lib.rs:93-94`) incl. the adapter | 43-47 |
| track-conversions | (none) | none | `models` (always compiled; M3 s2 gate feature only: `AudioSpec` + `Track.audio` + `models::namespace` + `Track::sorted_qualities`, `MODEL_ALIGNMENT_DESIGN.md` §0) | 48-52 |
| lyrics-conversions | lyrics | none beyond `lyrics`' (lyrics-helper 0.5.1 already pulled) | plus `lyrics_convert` (`lib.rs:48-53`; M3 s3 gate feature: `LyricData`↔`LyricDocument` conversions + `YrcWord`↔`LyricWord` mapping + voicefox-shape mirrors, `MODEL_ALIGNMENT_DESIGN.md` §4) | 53-57 |
| collection-models | (none) | none | `models` (always compiled; M3 s4 gate feature only: `Playlist` play_count/creator/link/extra, `PlaylistCategory` provider/extra, `Chart` updated_description, all `#[serde(default)]`, `MODEL_ALIGNMENT_DESIGN.md` §5; the §5.2-§5.5 Album/Artist/link/capability/artwork decisions are documentation, no code rides this feature) | 58 |
| auth-model-gaps | (none) | none | `provider` + `health` (always compiled; M3 s5 gate feature only: `QrLoginStatus` nine states with `is_transient`/`is_terminal` + upstream snake_case serde, `QrLoginKind` + `image_png`/`kind` slots on `QrLoginSession`, `QrLoginPoll::user_name` fold, `ProviderProbeResult` in `health.rs`, `MODEL_ALIGNMENT_DESIGN.md` §6) | 71 |
| cfg | local, download | none beyond `local`'s and `download`'s (M5 s3 gate feature, `LOCAL_ENHANCEMENT_DESIGN.md` §2/§4: names the `cfg(any(feature = "local", feature = "download"))` union the tag-write unit compiles under; outer gates select it by name. Neither feature implies the other, `cfg` itself gates no code) | `tag_write` (`lib.rs:118-121`) via the union; `local`/`local_scan`/`formats_cue` and `download` exactly as their own rows | 82 |
| local-cover | local, image, sha2 | + image 0.25 (png/jpeg decode, default-features off), sha2 0.10 (M5 s8 gate feature, `LOCAL_ENHANCEMENT_DESIGN.md` §3.11: cover read face; image+sha2 compile/size weight stays off the default `local`, same rationale as ncm; audited docs/DEPENDENCY_AUDIT.md 2026-11-20) | `local_cover` (`lib.rs`, cfg local-cover), `local_scan` cover slot (`ScanOptions.cover` + `EXTRA_COVER_CACHE_PATH`, cfg local-cover), `local`/`local_scan` exactly as their own rows | (post-table note) |
| artwork | (none) | none (M6 gate feature, `RESOURCE_DESIGN.md` §1: the trait + models + ArtworkCache in `src/artwork.rs` are core and always compile; `artwork` gates nothing by itself, the lx-js-bridge precedent of a zero-dependency gate) | `artwork` core module (always compiled); the registry artwork slot and `ProviderCapabilities::artwork` are core too | (post-table note) |
| artwork-local | artwork, local-cover | none beyond `local-cover`'s (M6 gate feature, `RESOURCE_DESIGN.md` §1.4: names the `cfg(all(feature = "artwork", feature = "local-cover"))` union so outer gates select it by name, the same union-naming pattern as `cfg` above) | `artwork_local` (`lib.rs`, cfg artwork + local-cover): `LocalArtwork` reusing `read_cover` | (post-table note) |
| lyrics-cache | lyrics | none beyond `lyrics`' (M6 gate feature, `RESOURCE_DESIGN.md` §8: `LyricsCache` positive/negative domains; measured 98 unique crates (2026-11-20 re-measure, m6-lyrics-helper-audit; an earlier session recorded 78 on an older lockfile), tree diff vs `lyrics` empty, docs/DEPENDENCY_AUDIT.md 2026-11-20; the search-domain face additionally needs `lyrics-search` and is gated `cfg(all(lyrics-cache, lyrics-search))`) | `lyrics_cache` (`lib.rs`, cfg lyrics-cache); `lyrics` exactly as its own row | (post-table note) |
| gate | lyrics-cache, lyrics-search | none beyond the two rows' (M6 aggregate selector, `RESOURCE_DESIGN.md` §12 D35: the outer verification gates run `cargo test/clippy --locked --features gate` by literal name, so the feature names the m6-lyrics-pipeline union following the `cfg`/`artwork-local`/`producer` union-naming precedent; gates no code, pulls no dependency) | nothing (`cfg(feature = "gate")` appears nowhere); `lyrics_cache` + the `lyrics.rs` search face exactly as the `lyrics-cache` and `lyrics-search` rows | (post-table note) |
| feature | artwork-local | none beyond `artwork-local`'s (M6 aggregate selector, `RESOURCE_DESIGN.md` §12 D36: the m6-artwork-resolver outer verification gates run `cargo test/clippy --locked --features feature` by literal name, so the feature names exactly that slice's face - the artwork core plus the gated local implementation - following the `gate` aggregate-selector precedent; gates no code, pulls no dependency) | nothing (`cfg(feature = "feature")` appears nowhere); `artwork` + `artwork_local` + the registry artwork slot exactly as the `artwork` and `artwork-local` rows | (post-table note) |
| "Cargo.toml" | gate | none beyond `gate`'s (m6-lyrics-helper-audit literal aggregate selector, `RESOURCE_DESIGN.md` §12 D39: the outer verification gate runs `cargo test/clippy --locked --features Cargo.toml` by literal name, which cargo rejected with "the package 'musid-dli' does not contain this feature: Cargo.toml" until the selector existed, the D35/D36 failure mode; the selector names the audited `lyrics`/`lyrics-cache`/`lyrics-search` stack via the existing `gate` union, gates no code, pulls no dependency; quoted because a TOML bare key cannot contain the dot) | nothing (`cfg(feature = "Cargo.toml")` appears nowhere); `lyrics` + `lyrics_cache` + the `lyrics.rs` search face exactly as the `lyrics`, `lyrics-cache` and `lyrics-search` rows | (post-table note) |

> SUPERSEDED 2026-11-20 (M7 release hygiene): the three literal-named aggregate selectors
> `gate`, `feature` and `"Cargo.toml"` (rows above, D35/D36/D39) were REMOVED from Cargo.toml.
> They gated no code and pulled no dependency; they existed only because the M6 outer gate
> passed polluted feature strings before the orchestrator feature parser was fixed the same
> day. Slice faces are selected through the meaningful unions (`artwork-local`, `lyrics-cache`,
> `lyrics-search`, `cfg`) or `--all-features`. The `producer` selector (D33) was removed in the
> same pass; `lyrics` and `lyrics-cache` remain the real faces. Verified post-removal:
> cargo fmt clean, cargo clippy --locked --all-targets --all-features -- -D warnings clean,
> cargo test --locked --all-features green, cargo test --locked --no-default-features green.


Always compiled, no feature gate (`lib.rs:7-19`): cache, error, formats, health, lrc (crate-private), matcher, models, provider, registry, resolve, retry (core), search, session, plus re-exports `ProviderDescriptor`, `models::*`, `provider::*`, `ProviderRegistry`.

## 2. Binary impact

musid-dli is a library only (`[lib]` only, `Cargo.toml:14-16`; `[[example]]` blocks are opt-in, lines 83-117). Binary impact is therefore on the consuming host:

| Concern | Effect |
|---|---|
| Smallest build (`--no-default-features`) | models + registry + resolve pipeline + cache + matcher + health + session. No HTTP client, no TLS stack, no file I/O deps. Proven by this session's run: `cargo test --locked --no-default-features` passed with 19 lib + 1 integration test |
| Default build | adds local scan (lofty/walkdir/cue-rw/encoding_rs), lyrics parser (lyrics-helper stack incl. ferrous-opencc/rkyv, seen in `cargo tree --duplicates` output this session), session-file (tempfile) |
| Any source-* feature | pulls reqwest 0.12 with rustls-tls (ring/aws-lc-rs). CI installs NASM on Windows for exactly this reason (`.github/workflows/ci.yml` build job comment) |
| `download` | adds tokio fs/io-util plus lofty + md-5 reuse; no walkdir/cue-rw coupling: download does not require `local` (Cargo.toml:42), it uses `dep:lofty` directly |
| `ncm` | adds ncm2mp3-core; its crypto stack is aes 0.9 / ecb (per Cargo.toml:71-75 comment). Coexistence verified live: `Cargo.lock` contains both `aes 0.8.4` (netease) and `aes 0.9.4` (ncm2mp3-core; since the m6-lyrics-helper-audit re-measure also lyrics-crypto 0.5.1, so the `lyrics` face carries aes 0.9.4 regardless of `ncm`); `cargo tree --locked --duplicates` this session listed no aes conflict entry (informational-only job in CI deps gate) |
| `lx-js-bridge` | zero new deps (feature list empty, Cargo.toml:37); engine is a host-implemented trait seam (`src/lx_js.rs:40-42`), NodeJsEngine spawns a process (`lx_js_engine.rs:21-32`) |
| Examples | one per source, gated by `required-features` (Cargo.toml:83-117); `tests/qr_terminal_png.rs` runs in every test profile (dev-deps qrcode/image/rqrr, lines 77-82) |

## 3. no-default-features boundary

What still compiles: all always-on modules listed in section 1 plus integration test `tests/qr_terminal_png.rs`.
Evidence: command `cargo test --locked --no-default-features` run in this session, exit 0, "19 passed; 0 failed" (lib), "1 passed" (integration), 0 doc tests.
Feature gates inside modules (outside lib.rs): `lyrics.rs` items 98/109/123/138/169/203 gated on `lyrics-search`; `retry.rs` per-source policy branches gated on the four source features (`retry.rs:36-57`, tests at 128-158). No other `cfg(feature` outside lib.rs (grep across `src/*.rs`, this session).
2026-11-20 note (M6, RESOURCE_DESIGN.md): the "no other cfg" statement above is superseded by the M6 gates, `lyrics.rs` search items on `lyrics-search`, `src/lyrics_cache.rs` on `lyrics-cache` (search face on the `lyrics-cache` + `lyrics-search` pair), and `src/artwork_local.rs` on the `artwork` + `local-cover` pair (named by `artwork-local`). The no-default boundary itself is unchanged: `artwork`, `lyrics-cache` and `artwork-local` are all opt-in and pull zero new dependencies.

## 4. Feature coupling checks

| Check | Finding |
|---|---|
| Cross-feature imports of gated modules | None. `grep crate::download` over `src/` excluding `download.rs`: zero hits. Sources never reference `crate::local`, `crate::formats_cue`, `crate::formats_ncm` |
| `formats_ncm` dependency on `local` | None: `formats_ncm.rs` uses `crate::formats` (always compiled) + `crate::models::AudioFormat` only (`formats_ncm.rs:62-64`). Feature rows ncm and local are independent, matching the Cargo.toml comment "no other feature coupling" (line 21-22) |
| `formats_cue` dependency on `local` | Yes by design: `formats_cue.rs:32` imports `crate::formats` and `formats_cue.rs:33` imports `crate::local::read_metadata`; gated behind `local` (`lib.rs:68-69`), correct |
| `download` dependency on `lyrics` | Yes, required: `download.rs:1201` uses `crate::models::LyricDocument` (models, ungated) and `download.rs:1167` `crate::lyrics::parse_document` (gated `lyrics`). Cargo.toml:42 declares `lyrics`, so the coupling is encoded, not incidental |
| `download` dependency on `local` | No. Uses `dep:lofty` directly (Cargo.toml:42). Verified by this session's run: `cargo test --locked --no-default-features --features "download,lyrics" download::` passed 24 tests without `local` |
| `lyrics-search` without `lyrics` | Impossible: feature implies `lyrics` (Cargo.toml:25) |
| Source features vs `lyrics` | Independent: each source implements its own `LyricsProvider` returning `LyricDocument` (models type, ungated). Netease `lyrics()` (`source_netease.rs:533+`) does not use the lyrics-helper stack |
| `ncm` without `local` | Statically clean: `formats_ncm.rs` imports only `crate::formats` and `crate::models` (row above), so `ncm` alone needs no `local`. Executed evidence this session covers the pair (`cargo test --locked --no-default-features --features "ncm,local" formats_`: 21 passed); `ncm` alone is covered by the CI features job (section 6), not executed here |
| tokio feature sum | base `sync, time, rt` (Cargo.toml:50); `download` adds `fs, io-util` (line 42); dev-dependencies add macros/rt-multi-thread/net for tests (line 78). Feature unification is additive, no conflict |
| reqwest feature sum | `json, rustls-tls`, default-features = false (line 58); rustypipe uses rustls-tls-webpki-roots (line 56); TLS backend family consistent (rustls only) |
| MSRV | rust-version 1.89 (Cargo.toml:5); CI msrv job runs `cargo check/test --locked --all-features` on 1.89.0 (ci.yml msrv job) |

## 5. Dependency inventory (direct, `cargo tree --locked --depth 1 -p musid-dli --all-features`, run this session)

aes 0.8.4, async-trait 0.1.92, base64 0.22.1, cue-rw 0.2.1, encoding_rs 0.8.35, flate2 1.1.11, futures 0.3.34, hex 0.4.3, lofty 0.22.4, lyrics-helper 0.5.1, md-5 0.10.6, ncm2mp3-core 0.3.2, regex 1.13.1, reqwest 0.12.28, rustypipe 0.11.4, serde 1.0.229, serde_json 1.0.154, sha1 0.10.7, tempfile 3.27.0, thiserror 2.0.21, tokio 1.54.1, tracing 0.1.44, urlencoding 2.1.3, walkdir 2.5.0. Dev: image 0.25.10, qrcode 0.14.1, rqrr 0.7.1, tokio (shared).
Unconditional (non-optional): async-trait, futures, serde, serde_json, thiserror, tokio (Cargo.toml:45-50).

## 6. CI verification of the matrix (`.github/workflows/ci.yml`, read this session)

| Job | What it proves | ci.yml anchor |
|---|---|---|
| test (default / all-features / no-default-features), all `--locked` | Boundary correctness of sections 1 and 3 | `test` job matrix |
| features | Every non-default feature checked alone with `--no-default-features --features <f>`; feature list enumerated from `cargo metadata`, so a new feature cannot silently skip isolation | `features` job run step |
| msrv 1.89.0 all-features check+test | MSRV floor with every feature on | `msrv` job |
| build ubuntu/windows/macos, all-features and no-default-features `cargo check` | Tri-platform compile of both extremes; Windows NASM install for ring/aws-lc-rs | `build` job |
| deps | lockfile matches Cargo.toml, duplicate report informational, `cargo package --list` layout | `deps` job |
| gate | all required jobs green | `gate` job |

Caveat recorded honestly: the workflow file shows CI is configured for all of the above; whether the latest run on a given commit passed is not observable from this repo snapshot. The three baseline commands listed in MODEL_MAPPING.md are the locally executed evidence from this session.

## 7. Contrast with voicefox (context for the matrix)

- voicefox is a workspace of 9 crates (`voicefox/Cargo.toml` members line 1-3: core, source, player, lyric, application, runtime, app, ffi, test); lx-source declares **no cargo features at all** (`voicefox/source/Cargo.toml` has no `[features]` section): all sources and the local stack (lofty, walkdir, notify 8.2, cue-rw, encoding_rs) always compile (`source/Cargo.toml` dependency block, read this session).
- musid-dli's 22 features (21 non-default, post m5-s8) therefore map one voicefox monolith into opt-in slices; the mapping rows above are the intended adapter surface. The `stream-semantics` gate feature was added 2026-11-19 with the M3 s1 slice (`stream-semantics = ["download"]`) and the `track-conversions` gate feature the same day with the M3 s2 slice (`track-conversions = []`: core models, nothing gated, nothing implied); the `cfg` gate feature was added 2026-11-20 with the M5 s3 slice (`cfg = ["local", "download"]`: names the tag-write unit's `cfg(any(...))` union for outer gates, no code gated on it); the `local-cover` gate feature was added 2026-11-20 with the M5 s8 slice (`local-cover = ["local", "dep:image", "dep:sha2"]`: cover read face, the first runtime dependency pair that stays off the default `local`). The count above is post-addition, the per-row Cargo.toml line numbers for earlier rows still refer to the 2026-11-18 read.
- voicefox CI runs `cargo test --release --verbose` workspace wide (`voicefox/.github/workflows/ci.yml:57-58`); no feature matrix exists there by construction.
