# CI requirements and current workflow inventory (integration campaign M0)

Audit date: 2026-11-18. Every job list below was read from the workflow file cited; every run-state line comes from `gh run list` executed in this session (read-only API). No workflow was modified.

## 1. musid-dli: .github/workflows/ci.yml

Single workflow, name `CI`. Triggers (.github/workflows/ci.yml:15-20): push to main, pull_request to main, workflow_dispatch. Concurrency cancels superseded runs per ref (:25-27). Top-level permissions: `contents: read` only (:22-23). Env: CARGO_TERM_COLOR always. Third-party actions pinned by major tag: actions/checkout@v7, Swatinem/rust-cache@v2 (:38, :47). Secrets required: NONE; the workflow uses no `secrets.*` at all. All correctness flags run with --locked so a stale Cargo.lock fails loudly (design note, :4-5).

Job inventory (9 jobs):

| Job | Runner | Key steps and flags | Gate role |
|---|---|---|---|
| fmt | ubuntu-latest, 10 min | `cargo fmt --all -- --check` (:33-39) | formatting |
| clippy | ubuntu-latest, 30 min | `cargo clippy --locked --all-targets --all-features -- -D warnings` (:41-48) | zero-warning lint over every feature and target |
| test (3-way matrix) | ubuntu-latest, 30 min | `cargo test --locked` / `--locked --all-features` / `--locked --no-default-features` (:50-67) | default, all and no-default feature sets |
| features | ubuntu-latest, 45 min | enumerates every non-default feature live from `cargo metadata --locked --no-deps`, then `cargo check --locked --no-default-features --features X` per feature (:69-85) | an isolated check per feature; a new feature cannot miss the gate. Covers the 2026-11-18 `ncm` and `download` features automatically |
| msrv | ubuntu-latest, 30 min | installs 1.89.0 via rustup, `rustup run 1.89.0 cargo check/test --locked --all-features` (:87-96) | MSRV 1.89 (matches Cargo.toml rust-version = "1.89", Cargo.toml:5); invoked via rustup run because `+toolchain` is silently ignored on this machine class (docs/IMPLEMENTATION_STATUS.md:99) |
| build (3-OS matrix) | ubuntu-latest, windows-latest, macos-latest, 45 min | `cargo check --locked --all-features` plus `--no-default-features`; Windows step installs NASM via choco because both TLS backends (ring, aws-lc-rs) need it (:98-115) | cross-platform compile coverage |
| docs | ubuntu-latest, 30 min | `cargo doc --locked --all-features --no-deps` with RUSTDOCFLAGS=-D warnings (:117-126) | doc build without warnings |
| deps | ubuntu-latest, 10 min | lockfile sync via `cargo metadata --locked`; informational `cargo tree --locked --duplicates`; publishability via `cargo package --locked --no-verify --list` (:128-139) | dependency integrity |
| gate | ubuntu-latest, 5 min | needs all 8 jobs above, if: always(); fails when any required job failed or was cancelled (:141-155) | single aggregate gate |

Design constraints to preserve (stated in the file header, :1-11): everything --locked; feature matrix enumerated, not hardcoded; no job touches real provider endpoints (network smokes stay opt-in local tools, scripts/run_all_smokes.sh); cache must never affect correctness flags; Cargo.toml `exclude = [".github"]` keeps workflow files out of the published crate (Cargo.toml:10).

Run state: `gh run list --repo emoeem/musid-dli --limit 10` returned EMPTY (exit 0) in this session: zero runs ever. Cause: the commit that adds the workflow, 919f1de, is unpushed (main is ahead 1 of origin/main at 9fa8d74). Requirement: push main to trigger the first full 9-job run; nothing needs to be added to make CI work, but the uncommitted working tree (22 modified files, 4 new src files, 124 fixtures) is invisible to that first run unless committed first.

## 2. voicefox workflows (all four read from .github/workflows/)

### 2.1 ci.yml (name `CI`)

Triggers: push to main, pull_request to main (ci.yml:3-7). No permissions block (workflow uses the default GITHUB_TOKEN; nothing elevated). Env pins LIBMPV_TAG v0.41.0-1 and its SHA256 (:9-14). Secrets: none.

| Job | Runner | Content | Must keep |
|---|---|---|---|
| lint | ubuntu-24.04 | apt libmpv-dev; `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings -A clippy::double_must_use` (:16-38) | the single documented clippy exception (double_must_use inside #[async_trait] macro expansion, comment :34-36) |
| build-linux | ubuntu-24.04 | apt libmpv-dev; `cargo build --release --verbose`; `cargo test --release --verbose`; upload artifact voicefox-linux-x86_64 (:40-66) | release tests run in CI here; if the campaign changes host code, this is the workspace regression net |
| build-windows | windows-2022 | msys2 MINGW64 toolchain; rustup target x86_64-pc-windows-gnu; downloads the pinned audio-only libmpv 7z and verifies SHA256 before extraction (:69-133); builds voicefox-app `--locked --release --target x86_64-pc-windows-gnu --features lx-player/build_libmpv` (:135-142); objdump import validation with an explicit DLL allowlist for both voicefox.exe and libmpv-2.dll (:144-217); `voicefox.exe --check-libmpv`; zip artifact (:219-243) | the SHA256 pin plus LIBMPV_TAG pair (:11-14, must be bumped together), the import allowlist, and the --check-libmpv runtime gate: they are the integrity story for the bundled DLL |
| build-macos | matrix: macos-15-intel (x86_64) and macos-15 (arm64) | brew install mpv with RUSTFLAGS -Lnative; `cargo build --locked --release --package voicefox-app --target <triple>`; file/otool linkage inspection; `--check-libmpv`; per-arch artifacts (:245-308) | both arch entries; the runner_arch assertion (:294) |

### 2.2 pages.yml (name `Deploy GitHub Pages`)

Triggers: push to main with paths [site/**, Cargo.toml, .github/workflows/pages.yml], plus workflow_dispatch (pages.yml:2-6). Permissions: `contents: read`, `pages: write`, `id-token: write` (:7-10). Environment: github-pages with the deploy URL (:17-19). Concurrency group pages (:11-13). Secrets: none beyond the built-in OIDC id-token.

Single job `deploy` (ubuntu-24.04): checkout@v7, configure-pages@v6, version badge injection that greps the workspace version from Cargo.toml and rewrites site/index.html with a hard failure when the page badges do not end up exactly v<version> (:27-34), upload-pages-artifact@v5 from ./site, deploy-pages@v5 (:35-41). Must keep: the paths filter (Cargo.toml in the list is what redeploys on version bumps, comment :25-26) and the badge-injection hard gate.

### 2.3 release.yml (name `Release`)

Trigger: tags v*.*.* only (release.yml:4-6). Top-level permissions `contents: read` (:15-16); the publish job elevates to `contents: write` (:294-296). Secrets: only the built-in github.token via GH_TOKEN (:303-306). Same LIBMPV_TAG/SHA256 pin as ci.yml (:10-13).

| Job | Runner matrix | Content | Must keep |
|---|---|---|---|
| build (5 targets) | ubuntu-24.04 (x86_64-linux-gnu), ubuntu-24.04-arm (aarch64-linux-gnu), windows-2022 (x86_64-pc-windows-gnu MinGW), macos-15-intel (x86_64-darwin), macos-15 (aarch64-darwin) | per-OS libmpv install or the pinned Windows 7z download with SHA256 check; `cargo build --locked --release --package voicefox-app --target <triple>` (Windows adds --features lx-player/build_libmpv, :148-156); non-Windows `--check-libmpv`, macOS otool inspection; Windows objdump import allowlist plus `--check-libmpv`; zip per target; artifact upload with retention-days 1 (:57-286) | fail-fast: false; the --locked flag on every build; the pinned libmpv pair; the import allowlist |
| publish | ubuntu-24.04 | downloads all artifacts, `gh release create` with --verify-tag --generate-notes (idempotent: skips when the release exists), `gh release upload --clobber` for each asset (:289-324) | contents: write scoped to this job only, not the whole workflow |

### 2.4 scoop.yml (name `Scoop manifest`)

Triggers: schedule cron `0 */6 * * *` (deliberate backstop), workflow_dispatch, workflow_run on Release completed, release published (scoop.yml:9-23). Permissions: `contents: write` (:24-25). Concurrency group scoop-manifest, cancel-in-progress false (:27-29). Secrets: only `secrets.GITHUB_TOKEN` (:49). Explicit design decision in the comments: no third-party action, one script plus one git commit, so contents: write never goes to a floating @main tag (:6-7).

Single job `update` (ubuntu-24.04, if the workflow_run conclusion is success :36): checkout pinned to `ref: main` because release/schedule triggers check out detached HEADs (:39-45); runs packaging/scoop/update-manifest.sh with GITHUB_TOKEN, which reads the latest Windows release asset, computes its hash and rewrites packaging/scoop/voicefox.json only when version/url/hash all change; commits with the github-actions[bot] identity and pushes main, exiting cleanly when the manifest is already current (:47-63). Must keep: the ref: main pin, the skip-if-clean guard, the bot identity.

## 3. Cross-repo requirements summary

### 3.1 Platform matrix

| Workflow | ubuntu | windows | macos | Notes |
|---|---|---|---|---|
| musid-dli ci.yml | yes (7 jobs) | yes (build check only, no tests) | yes (build check only, no tests) | test and clippy run only on ubuntu; MSRV on ubuntu |
| voicefox ci.yml | ubuntu-24.04 | windows-2022 (GNU target) | macos-15-intel + macos-15 | real release build plus tests on Linux; compile-plus-runtime-check elsewhere |
| voicefox release.yml | ubuntu-24.04 and ubuntu-24.04-arm | windows-2022 | macos-15-intel + macos-15 | 5 release targets, tag-driven |
| voicefox pages.yml | ubuntu-24.04 | none | none | site deployment |
| voicefox scoop.yml | ubuntu-24.04 | none | none | manifest maintenance |

Integration implication: voicefox requires libmpv on every platform (apt libmpv-dev, brew mpv, pinned Windows DLL) while musid-dli requires none; any host-side CI change that touches the adapter must keep the libmpv provisioning steps intact.

### 3.2 Permissions and secrets

| Workflow | Declared permissions | Custom secrets |
|---|---|---|
| musid-dli ci.yml | contents: read (top level) | none |
| voicefox ci.yml | none declared (default token) | none |
| voicefox pages.yml | contents: read, pages: write, id-token: write | none (OIDC) |
| voicefox release.yml | contents: read top level; contents: write on the publish job | none (github.token) |
| voicefox scoop.yml | contents: write | none (GITHUB_TOKEN) |

Requirement for the campaign: keep minimal scopes as-is. No new secret is needed by anything audited here. If a future musid-dli workflow publishes to crates.io or deploys docs, add the token as a scoped environment secret rather than widening `permissions`.

### 3.3 Current run state (gh run list, this session)

| Repo / workflow | Last run | Result | Trigger | Evidence |
|---|---|---|---|---|
| musid-dli (any) | NEVER | n/a | n/a | `gh run list --repo emoeem/musid-dli --limit 10`: empty output, exit 0; workflow commit 919f1de unpushed |
| voicefox ci.yml | 2026-10-30T04:22:34Z, run 41002542959, 8m43s, commit e88cf98 (0.8.1) | success | push | `gh run list --workflow ci.yml`; note: HEAD dd116b1 (2026-11-18 bot push) has NO CI run because GITHUB_TOKEN-triggered pushes are recursion-protected (voicefox scoop.yml:10-11 documents this GitHub behavior) |
| voicefox pages.yml | 2026-10-30T04:22:34Z, run 41002542988, 18s | success | push | `gh run list --workflow pages.yml` |
| voicefox release.yml | 2026-10-30T04:22:45Z, run 41002559075, 10m29s, tag v0.8.1 | success | push (tag) | `gh run list --workflow release.yml`; one historical failure at v0.7.0 first attempt 2026-10-17 |
| voicefox scoop.yml | 2026-11-18T14:52:32Z, run 44329098978, 9s | success | schedule | `gh run list --repo emoeem/voicefox --limit 10`: 10 most recent runs are all scoop schedule runs, latest success |

Statement for the record: nothing in either repository's Actions was triggered by this machine during the audit. musid-dli additionally requires a push (of main, including the uncommitted working tree) before any CI result exists at all. voicefox workflows are live on GitHub (scoop runs on its 6-hour schedule), but its latest human-driven push predates the audit and the current bot-only HEAD has intentionally not run CI.

## 4. What the integration campaign must not break

| Item | Where | Why |
|---|---|---|
| --locked on every test/clippy/build in musid-dli ci.yml | .github/workflows/ci.yml:48,59-63,78,83,95-96,114-115,126,135,139 | a stale Cargo.lock must fail loudly; Cargo.lock is modified and uncommitted right now |
| cargo metadata feature enumeration | ci.yml:78 | makes the 2026-11-18 ncm and download features gated automatically; hardcoding the list would regress on the next feature |
| contents: read top-level permission on musid-dli CI | ci.yml:22-23 | least privilege; no job needs more |
| No CI job touches real provider endpoints | ci.yml:9 design note, docs/IMPLEMENTATION_STATUS.md:59 | network smokes are opt-in local tools (scripts/run_all_smokes.sh); CI stays hermetic |
| voicefox libmpv pin pair (TAG + SHA256) | voicefox ci.yml:11-14 and release.yml:10-13 | bumping one without the other breaks checksum validation |
| Windows import allowlist + --check-libmpv | voicefox ci.yml:195-222, release.yml:237-261 | the only automated proof that the bundled DLL loads |
| voicefox clippy exception `-A clippy::double_must_use` | voicefox ci.yml:34-38 | the single documented macro-expansion lint that cannot be fixed at source level |
| pages.yml paths filter and badge hard gate | voicefox pages.yml:3-5,27-34 | version bumps redeploy the site with the correct badge or fail |
| scoop.yml ref: main pin and skip-if-clean guard | voicefox scoop.yml:39-45,53-57 | release and schedule triggers check out detached HEADs; the guard prevents empty bot commits like dd116b1 churn |
| 6-hour scoop schedule as recursion backstop | voicefox scoop.yml:10-12 | GITHUB_TOKEN-triggered releases do not re-trigger workflows; the timer is the safety net |

## 5. Commands run for this document

File reads: musid-dli .github/workflows/ci.yml (full, 155 lines); voicefox .github/workflows/ci.yml, pages.yml, release.yml, scoop.yml (full).

Run-state queries: `gh run list --repo emoeem/musid-dli --limit 10`; `gh run list --repo emoeem/voicefox --limit 10`; `gh run list --repo emoeem/voicefox --workflow ci.yml --limit 6`; same per-workflow form for pages.yml and release.yml. Two per-workflow queries failed once with a transient API EOF and were retried successfully after a pause; no result above comes from a failed query.

No workflow file, permission, secret or runner setting was modified in either repository.
