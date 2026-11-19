# Baseline audit: musid-dli and voicefox (integration campaign M0)

Audit date: 2026-11-18. Auditor ran every command listed here in this session; nothing is quoted from memory or from another run. Scope: read-only inspection of both repositories plus the narrow test commands named in the audit brief. No business code was changed, no files in voicefox were touched, and git usage stayed read-only.

Legend for verification state, used in all tables:

| State | Meaning |
|---|---|
| Implemented | Code exists at the cited path |
| Has tests | A test that ran in this session or is recorded in docs/IMPLEMENTATION_STATUS.md with its command |
| Live verified | A recorded anonymous run against real provider endpoints (date cited); fixture tests do NOT count as live verification |
| CI verified | The check ran in a GitHub Actions run; for musid-dli this is currently "no" everywhere (see 2.4) |

## 1. Repository snapshots

### 1.1 musid-dli

| Item | Value | Source |
|---|---|---|
| Branch | main | `git branch -vv` |
| HEAD | 919f1defa212c9924281006f8da634188bd4cc0d | `git rev-parse HEAD` |
| Remote | origin https://github.com/emoeem/musid-dli.git (fetch and push) | `git remote -v` |
| Sync state | ahead 1 of origin/main; origin/main is at 9fa8d74 | `git status -b`, `git rev-parse origin/main` |
| Unpushed commit | 919f1de "ci: add GitHub Actions workflow (fmt, clippy, tests, feature matrix, MSRV, tri-platform builds, docs, deps gate)" | `git log origin/main..HEAD --oneline` |
| History depth | 3 commits: cebfca5 (initial release), 9fa8d74 (v0.1.0 publication docs), 919f1de (CI workflow) | `git log --oneline` |
| Toolchain | cargo 1.99.0 (5f94df478 2026-08-27), rustc 1.99.0 (b940084d7 2026-09-28) | `cargo --version`, `rustc --version` |

Working tree (22 modified + 134 untracked entries, `git status --porcelain=v1 -uall`, recounted 2026-11-18 by the acceptance pass; the first count of 128 predates the six files this audit wrote under docs/integration/):

| Untracked path | Notes |
|---|---|
| src/download.rs | 1948 lines, `pub mod download` behind feature `download` (src/lib.rs:74) |
| src/formats_cue.rs | 898 lines, `pub mod formats_cue` behind feature `local` (src/lib.rs:68) |
| src/formats_ncm.rs | 343 lines, `pub mod formats_ncm` behind feature `ncm` (src/lib.rs:71) |
| src/lx_js_provider.rs | 798 lines, `pub mod lx_js_provider` behind feature `lx-js-bridge` (src/lib.rs:65) |
| tests/fixtures/ | 124 files in 11 directories (counts below) |

| Modified files (22) | |
|---|---|
| Cargo.toml, Cargo.lock, README.md | Cargo.toml adds `ncm` and `download` features plus ncm2mp3-core 0.3.2 dependency (see 2.1) |
| docs/HUMAN_TEST_CHECKLIST.md, docs/IMPLEMENTATION_STATUS.md, docs/LX_JS_BRIDGE.md, docs/VOICEFOX_AUDIT.md, docs/WORKFLOW.md | status and workflow docs, latest entries dated 2026-11-18 |
| examples/kugou_smoke.rs, examples/kuwo_smoke.rs, examples/migu_smoke.rs | gained playlist/chart/artist/album/cover/link modules per the 2026-11-18 slices |
| src/formats.rs, src/lib.rs, src/source_apple.rs, src/source_bilibili.rs, src/source_fivesing.rs, src/source_jamendo.rs, src/source_kugou.rs, src/source_kuwo.rs, src/source_migu.rs, src/source_netease.rs, src/source_qqmusic.rs | fixture test modules, chart/album/link/cover slices |

Untracked fixtures, per directory (`ls tests/fixtures/<dir> | wc -l`):

| Dir | Files | Dir | Files |
|---|---|---|---|
| apple | 8 | kuwo | 22 |
| bilibili | 18 | migu | 12 |
| cue | 9 | ncm | 3 |
| fivesing | 14 | netease | 10 |
| jamendo | 5 | qqmusic | 15 |
| kugou | 8 | Total | 124 |

Each directory ships a README.md that states whether vectors are verbatim live captures, upstream inline goldens, formula-derived, or reconstructed shapes. That distinction matters: passing fixture tests never re-verifies live endpoints (docs/IMPLEMENTATION_STATUS.md line 100 and each fixture README).

### 1.2 voicefox

| Item | Value | Source |
|---|---|---|
| Branch | main, in sync with origin/main | `git status -b` |
| HEAD | dd116b1c744639fdaba3af489e6a59f52e3cba70 ("auto-update software manifest", the scoop bot commit) | `git rev-parse HEAD`, `git log --oneline -1` |
| Remote | origin https://github.com/emoeem/voicefox.git (fetch and push) | `git remote -v` |
| Working tree | clean, zero modified, zero untracked files | `git status --porcelain=v1 -b` |
| Layout | Cargo workspace, 9 members: core, source, player, lyric, application, runtime, app, ffi, test; version 0.8.1, edition 2024 | voicefox Cargo.toml:1-8 |
| Workflows | 4 files: ci.yml, pages.yml, release.yml, scoop.yml | `ls .github/workflows` |

voicefox was not modified in any way during this audit. It is treated as read-only per docs/tasks.md section 2.1.

## 2. musid-dli feature and test inventory

### 2.1 Feature list (read live from Cargo.toml, confirmed by cargo metadata)

Command run: `cargo metadata --locked --no-deps --format-version 1 | python3 -c "... print(pkg['features'])"`. Output: 18 features total, 17 non-default.

| Feature | Lines in | Declares | Added | Status |
|---|---|---|---|---|
| default | Cargo.toml:19 | local, lyrics, session-file | v0.1.0 | Implemented, tested, CI verified locally (see 2.3), not on GitHub |
| local | Cargo.toml:20 | lofty, walkdir, cue-rw, encoding_rs | v0.1.0 | Implemented (src/local.rs, src/formats_cue.rs), tested |
| ncm | Cargo.toml:23 | dep:ncm2mp3-core 0.3.2 (Cargo.toml:75, pinned to the audited release) | 2026-11-18 | Implemented (src/formats_ncm.rs), tested (see 2.3) |
| lyrics | Cargo.toml:24 | dep:lyrics-helper 0.5.1 | v0.1.0 | Implemented (src/lyrics.rs) |
| lyrics-search | Cargo.toml:25 | lyrics + lyrics-helper/search | v0.1.0 | Implemented |
| session-file | Cargo.toml:26 | dep:tempfile | v0.1.0 | Implemented (src/session_file.rs) |
| source-youtube-music | Cargo.toml:27 | dep:rustypipe 0.11.4 | v0.1.0 | Implemented compile-only; live search/playback and Botguard/PO-token NOT verified (docs/IMPLEMENTATION_STATUS.md:26) |
| source-netease | Cargo.toml:28 | reqwest, aes, md-5, hex, tracing, urlencoding | 2026-11-16 | Implemented, live verified 2026-11-16 (11/11 smoke modules) |
| source-qqmusic | Cargo.toml:29 | reqwest, sha1, base64, hex, tracing, urlencoding | 2026-11-16/17 | Implemented, live verified 2026-11-17 anonymous (stream refused anonymously with module code 10006, server-side gate) |
| source-kugou | Cargo.toml:30 | reqwest, md-5, base64, flate2, tracing, urlencoding | 2026-11-17 | Implemented, live verified 2026-11-17; legacy v3 stream fallback implemented but unverified (no fixture, no live claim) |
| source-kuwo | Cargo.toml:31 | reqwest, md-5, hex, base64, flate2, encoding_rs, tracing, urlencoding, regex | 2026-11-17 | Implemented, live verified 2026-11-17/18; lrcx word lyrics and web-signed lossless tier fixture-verified only; live link resolution unverified |
| source-migu | Cargo.toml:32 | reqwest, md-5, base64, flate2, tracing, urlencoding, regex | 2026-11-17 | Implemented, live verified 2026-11-17/18; link endpoints live unverified in the link slice |
| source-bilibili | Cargo.toml:33 | reqwest, md-5, tracing, urlencoding | 2026-11-17 | Implemented, live verified 2026-11-17; chart module live path unverified (smoke has no chart module) |
| source-jamendo | Cargo.toml:34 | reqwest, sha1, hex, tracing, urlencoding | 2026-11-17 | Implemented; live path intermittent (slice smoke FAILED exit 1; documentation-pass re-run passed: 20 tracks, Low/Mp3 stream) |
| source-apple | Cargo.toml:35 | reqwest, tracing, urlencoding | 2026-11-17 | Implemented, live verified 2026-11-17/18 (preview-only semantics explicit) |
| source-fivesing | Cargo.toml:36 | reqwest, tracing, urlencoding | 2026-11-17 | Implemented, live verified 2026-11-17/18; fixture set is mostly trimmed verbatim live captures |
| lx-js-bridge | Cargo.toml:37 | no deps, engine seam trait only | 2026-11-18 | Provider wrapper implemented (src/lx_js_provider.rs), fake-engine tested only; end-to-end script loading is host-provided |
| download | Cargo.toml:42 | tokio/fs, tokio/io-util, reqwest, lofty, md-5, tracing, lyrics | 2026-11-18 | Implemented (src/download.rs), tested (see 2.3); loopback HTTP only, no real-CDN claim |

Doc drift to fix: docs/IMPLEMENTATION_STATUS.md:56 (CI section, written 2026-11-17) says "15 features today". The live count is 17 non-default after the 2026-11-18 `ncm` and `download` additions. Coverage is not lost because ci.yml enumerates features from cargo metadata at runtime (.github/workflows/ci.yml:78), but the prose count is stale.

### 2.2 Test layout

| Location | Content | Source |
|---|---|---|
| Inline `#[cfg(test)]` modules | 325 test functions across 27 src files (32 src files total; corrected from the initially recorded 434 by the 2026-11-18 acceptance recount), largest: source_kuwo.rs 47, source_qqmusic.rs 36, source_bilibili.rs 34, source_migu.rs 28, source_netease.rs 26, download.rs 24 | `grep -c '#\[test\]\|#\[tokio::test\]' src/*.rs` |
| tests/qr_terminal_png.rs | 1 integration test: PNG QR roundtrip decoded with rqrr | tests/qr_terminal_png.rs (37 lines) |
| tests/fixtures/ | 124 offline fixture files, loaded via include_str!/include_bytes! from the src test modules | 1.1 table |
| examples/*.rs | 9 opt-in network smoke tools (one per provider), never run by cargo test; orchestrator scripts/run_all_smokes.sh | Cargo.toml:83-118 |
| Ignored tests | NONE. `grep -rn "#\[ignore" src tests examples` returned nothing (exit 1). The ignored-test inventory is empty | grep run 2026-11-18 |

Tests that are conditionally absent: every provider module only compiles under its feature, so a bare `cargo test` exercises only core modules. This is by design and matches the CI matrix.

### 2.3 Baseline test reproduction (commands run in this session, 2026-11-18)

| Command | Result | Matches recorded status? |
|---|---|---|
| `cargo test --locked --no-default-features` | 19 unit passed, 0 failed, 0 ignored; tests/qr_terminal_png.rs 1 passed; Doc-tests 0 | Yes for the counts, with one wording fix: docs/IMPLEMENTATION_STATUS.md:97 records this as "19 unit + 1 doctest"; the "1" is actually the qr_terminal_png integration test, doctests ran 0 |
| `cargo test --locked --features ncm --lib formats_ncm` | 7 passed, 0 failed (handler identity, golden inspect metadata/cover/format, golden extract byte-exact plaintext, empty key rejected as InvalidInput not panic, non-NCM rejected, single-stream no tracks, format mapping) | Consistent with the 7 unit tests recorded in docs/IMPLEMENTATION_STATUS.md:132 |
| `cargo test --locked --features download --lib download` | 24 passed, 0 failed: 15 module tests (range planning, bounded caps, content-range parse, candidate ordering/dedup, NetEase node rewrite, size/MD5 gates, retry matrix, staging path, sniffers) plus 9 download::http_tests over a loopback TcpListener (multipart byte-exactness, candidate fallback, lying-Range fallback, strict chunk mode, MD5 gate, cancellation, tag/lyric finish, fetch_bytes limit) | Consistent with the 24 total recorded in docs/IMPLEMENTATION_STATUS.md:135; note that line says "16 unit tests" but the observed split is 15 unit + 9 loopback = 24 |

All three runs compiled clean with --locked against the local Cargo.lock (modified, uncommitted). The broader per-provider runs recorded in docs/IMPLEMENTATION_STATUS.md:63-99 (for example `cargo test --locked --all-features`: 325 passed, 2026-11-18) were NOT re-run here by instruction: the outer script owns full-suite runs.

### 2.4 CI verification state

`gh run list --repo emoeem/musid-dli --limit 10` returned empty output with exit 0: the repository has zero GitHub Actions runs. Corroborated by git: the workflow-adding commit 919f1de is unpushed (1.1). Conclusion: every musid-dli result in this document is local-machine evidence; nothing is CI verified yet. The first push of main runs all nine ci.yml jobs (see CI_REQUIREMENTS.md).

### 2.5 Verification matrix (four-state classification)

| Area | Implemented | Has tests | Live verified | CI verified |
|---|---|---|---|---|
| Core (cache, resolve, retry classifier, search, matcher, health, session) | yes, src/cache.rs, src/resolve.rs, src/retry.rs, src/search.rs, src/matcher.rs, src/health.rs, src/session.rs | yes (unit tests counted in 2.2) | n/a (offline logic) | no |
| NetEase | yes, src/source_netease.rs | yes, 26 unit incl. 10 fixture tests | yes, 2026-11-16, 11/11 modules anonymous (docs/IMPLEMENTATION_STATUS.md:96) | no |
| QQ Music | yes, src/source_qqmusic.rs | yes, 36 unit incl. 13 fixture tests | yes, 2026-11-17 anonymous; stream + user playlists server-refused anonymously (docs/IMPLEMENTATION_STATUS.md:95) | no |
| Kugou | yes, src/source_kugou.rs | yes, 21 unit incl. 7 fixture tests | yes, 2026-11-17; v3 stream fallback unverified (docs/IMPLEMENTATION_STATUS.md:30) | no |
| Kuwo | yes, src/source_kuwo.rs | yes, 47 unit incl. 29 fixture tests | yes, 2026-11-17/18; lrcx word lyrics, web-signed lossless tier and live link resolution unverified (docs/IMPLEMENTATION_STATUS.md:31,127) | no |
| Migu | yes, src/source_migu.rs | yes, 28 unit incl. 16 fixture tests | yes, 2026-11-17/18; link endpoints live unverified (docs/IMPLEMENTATION_STATUS.md:129) | no |
| Bilibili | yes, src/source_bilibili.rs | yes, 34 unit incl. 17 fixture tests | yes, 2026-11-17 (reference, stream); chart module live unverified (docs/IMPLEMENTATION_STATUS.md:123) | no |
| Jamendo | yes, src/source_jamendo.rs | yes, 11 unit incl. 4 fixture tests | intermittent: slice smoke failed exit 1, documentation-pass re-run passed (docs/IMPLEMENTATION_STATUS.md:115) | no |
| Apple Music | yes, src/source_apple.rs | yes, 14 unit incl. 8 fixture tests | yes, 2026-11-17/18 (preview-only by contract) | no |
| 5sing | yes, src/source_fivesing.rs | yes, 19 unit incl. 12 fixture tests (mostly live captures) | yes, 2026-11-17/18 | no |
| YouTube Music | yes, src/source_youtube_music.rs | compile only, 1 unit test | NO, explicitly unverified (Botguard/PO-token) | no |
| LX JS bridge | yes, src/lx_js_provider.rs (JsBridgeProvider) | yes, fake-engine tests only (docs/IMPLEMENTATION_STATUS.md:101) | NO by design: no anonymous endpoint, user scripts untrusted; engine wiring host-provided | no |
| CUE adapter | yes, src/formats_cue.rs (ContainerFormatHandler id "cue") | yes, 14 fixture tests | n/a (offline by nature) | no |
| NCM handler | yes, src/formats_ncm.rs (ContainerFormatHandler id "ncm") | yes, 7 tests run this session | n/a (offline by nature) | no |
| Download engine core | yes, src/download.rs | yes, 24 tests run this session | loopback HTTP only; no real-CDN run claimed | no |
| QR login flows | implemented for QQ, QQ WeChat, Kugou, Bilibili, NetEase key polling | unit tests for polling/signature paths | create + first poll (Waiting) verified live; phone-scan confirmation human-required (docs/HUMAN_TEST_CHECKLIST.md sections 1-5) | no |
| VoiceFox integration (P5) | NOT started (docs/IMPLEMENTATION_STATUS.md:14) | none | none | none |

## 3. Risk table

| # | Risk | Evidence | Suggested handling |
|---|---|---|---|
| R1 | Zero GitHub Actions history for musid-dli: every "passed" today is local evidence; runner-only surfaces (Windows NASM, macOS, cache behavior) have never executed | gh run list empty; 919f1de unpushed | Push main first; treat the first CI run as the real M0 gate |
| R2 | 22 modified files, 4 new src files and 124 fixtures are uncommitted; the pushed CI would test commit 919f1de WITHOUT the 2026-11-18 slices (ncm, download, fixtures) | git status (1.1) | Commit the working tree before or together with the push, Cargo.lock included, or --locked fails the deps job |
| R3 | CI feature prose is stale: docs/IMPLEMENTATION_STATUS.md:56 says 15 features, live count is 17 | cargo metadata output (2.1) | Update the CI section prose when committing; the workflow itself needs no change (metadata enumeration) |
| R4 | Recorded-count drift in docs: ":97" calls the 1 non-unit test a doctest (it is tests/qr_terminal_png.rs); ":135" says 16 unit + 9 loopback (observed 15 + 9 = 24) | session runs in 2.3 | Correct the two lines in the same documentation pass |
| R5 | Live-verified status decays: provider endpoints change, and several paths are verified only against fixtures (Kuwo lrcx, Kuwo web-signed lossless, Bilibili charts, Kugou v3 fallback, Migu link endpoints, Jamendo intermittent edge nodes) | docs/IMPLEMENTATION_STATUS.md:30-34,115,123,125,127,129 | Keep the smoke re-verification slices scheduled; do not upgrade fixture-verified to live-verified |
| R6 | Human-required gates are open: four QR phone scans, NetEase authenticated user playlists, Kuwo lossless-entitlement session, QQ QRC 3DES (no verifiable key) | docs/HUMAN_TEST_CHECKLIST.md sections 1-6 | Leave categorized as human-required; block nothing that does not depend on them |
| R7 | Download engine has no byte-level resume, by upstream parity (the .part file restarts from zero); nothing claims resume | docs/IMPLEMENTATION_STATUS.md:135 honesty note | Keep the claim out of any host-side integration docs |
| R8 | voicefox CI did not run on HEAD dd116b1: pushes made with GITHUB_TOKEN are recursion-protected, so the bot manifest commit is untested on GitHub | voicefox scoop.yml:10-11 comment; gh run list ci.yml last run 2026-10-30 (CI_REQUIREMENTS.md 3.3) | Accept for manifest-only commits; ensure a human push precedes the integration merge |
| R9 | musid-dli has no direct dependency on voicefox (docs/IMPLEMENTATION_STATUS.md:18), but the untracked src/download.rs was ported from voicefox app/src/download/engine.rs; regression risk sits at the host adapter seam, which has no tests yet | docs/IMPLEMENTATION_STATUS.md:133-135 | Cover the seam in the P5 host-side slice, not in this repo |

## 4. Commands used (all read-only or test-only)

git: `status --porcelain=v1 -b`, `status --porcelain=v1 -uall`, `rev-parse HEAD`, `rev-parse origin/main`, `branch -vv`, `remote -v`, `log --oneline`, `log origin/main..HEAD --oneline`, `log --oneline -- .github/workflows/ci.yml` (musid-dli); same set minus the workflow log (voicefox).

cargo: `cargo test --locked --no-default-features`, `cargo test --locked --features ncm --lib formats_ncm`, `cargo test --locked --features download --lib download`, `cargo metadata --locked --no-deps --format-version 1` (feature enumeration only).

grep: `#[ignore` inventory over src, tests, examples; module declaration check in src/lib.rs; `#[test]`/`#[tokio::test]` counts.

gh (read-only API): `gh run list` for emoeem/musid-dli and per-workflow for emoeem/voicefox.

No git write operation, no voicefox file modification, no business code change, no full-suite test run was performed.

## 5. M0 acceptance gate (2026-11-18, appended by the independent acceptance pass)

Every command in the evidence column was re-run during this acceptance pass on the musid-dli working tree; results come from that run, not from the sections above. Values: PASS / FAIL / N/A / NOT RUN.

| Gate | State | Evidence |
|---|---|---|
| Code implementation | PASS | 32 src files at src/, 18 features (Cargo.toml:19-42), 124 fixtures; inventory in 2.1, 2.2 and 2.5; every cited file opened and line refs checked during this pass |
| Public API docs | PASS | `RUSTDOCFLAGS="-D warnings" cargo doc --locked --all-features --no-deps` exit 0, generated target/doc/musid_dli/index.html |
| fmt | PASS | `cargo fmt --all -- --check` exit 0 |
| Clippy | PASS | `cargo clippy --locked --all-targets --all-features -- -D warnings` exit 0 |
| Unit tests | PASS | `cargo test --locked --all-features`: 325 passed, 0 failed; `cargo test --locked --no-default-features`: 19 passed, 0 failed |
| Integration tests | PASS | tests/qr_terminal_png.rs: 1 passed in both runs above; targeted suites re-run: `formats_` 21 passed, `download::` 24 passed, `local::` 1 passed, `registration` 9 passed (265 filtered), lx `capabilities` 1 passed (54 filtered), `formats_ncm` 7 passed, all exit 0 |
| Feature matrix | PASS | `cargo metadata --locked`: 18 features (17 non-default); isolated `cargo check --locked --no-default-features --features ncm` and `--features download` both exit 0; `cargo tree --no-default-features` contains no reqwest; full per-feature enumeration stays the CI features job (ci.yml:69-85) |
| Cross-platform builds | NOT RUN | Windows and macOS targets need CI runners; first evidence will be the build job (ci.yml:98-115) after the push |
| Dependency security check | PASS | `cargo metadata --locked` exit 0 (lockfile in sync); `cargo tree --locked --duplicates` exit 0; `cargo package --locked --no-verify --list` exit 101 on the uncommitted working tree, exit 0 with --allow-dirty (201 entries, zero .github files shipped); no cargo audit or cargo deny is configured anywhere in this repo, so the gate is integrity-only |
| GitHub Actions | NOT RUN | Per the audit brief, neither repo was triggered by this machine; re-confirmed: `gh run list --repo emoeem/musid-dli --limit 10` returned empty output; voicefox's latest run is the scoop schedule run 44329098978 (success, 2026-11-18T14:52:32Z) |
| Workspace review | PASS | 22 modified + 134 untracked entries reviewed (1.1); three drift corrections applied in this pass (2.2 test count 434 to 325, 1.1 untracked count 128 to 134, netease registration test name in PROVIDER_PARITY_MATRIX.md); voicefox untouched, clean at dd116b1; no business code changed |
| Release next phase | PASS | All locally executable gates PASS; GitHub Actions and cross-platform builds NOT RUN by accepted decision, with R1 (first pushed CI run stays the standing CI gate) and R2 (commit the working tree before the push) carried into the next stage |

Gate verdict: M0 accepted; the next stage may start under the R1 and R2 conditions recorded above.
