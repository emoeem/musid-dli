# End-to-end workflow (build, verify, login, full coverage)

This document is the single entry point for everything remaining in the project. Automatable work is complete; what needs a phone is condensed into step 3. Every command runs from the repo root.

## 0. Continuous integration (GitHub Actions)

`.github/workflows/ci.yml` runs the same gates remotely: fmt, clippy (all features, all targets, warnings denied), tests on default / all / no default features, an isolated check of every non-default feature (enumerated live from cargo metadata, not hardcoded), MSRV 1.89.0 (check plus test, all features), build checks on ubuntu/windows/macos runners (the Windows job installs NASM because both TLS backends in the tree, ring and aws-lc-rs, need it), docs with warnings denied, a dependency integrity job (lockfile sync, duplicate report, package layout) and a final gate job for branch protection. Push to main or open a PR to trigger it; no job touches real provider endpoints.

## 1. Build and gates (no network)

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --no-default-features
cargo test --no-default-features
cargo test --all-features
```

Expected: fmt clean, clippy clean, 19 + 172 tests green.

## 2. Anonymous provider verification (automated, one command)

```bash
./scripts/run_all_smokes.sh
```

Runs all nine provider smokes in sequence. Expected outcomes per provider:

| Provider | Passing modules | Known gates (documented, not bugs) |
|---|---|---|
| netease | all | user needs a stored session cookie |
| qqmusic | all except stream | stream refused anonymously (code 10006) |
| kugou | all except stream | stream requires login (server-side) |
| kuwo | all | lossless tier via web signing (live-verified) |
| migu | all | anonymously only PQ/128k served |
| bilibili | all except keyword search | keyword search v_voucher-gated; references/streams work |
| jamendo | search/stream | containers hang anonymously (server-side) |
| apple | all | every stream is a 30-second preview (DRM contract) |
| fivesing | search/stream/lyrics/link | playlist track lists are JS-rendered server-side |

Filter to one provider: `./scripts/run_all_smokes.sh netease kugou`

## 3. Login flows (phone required, one command per platform)

The auth modules now poll every 3 seconds for up to 180 seconds: scan the QR the moment the command starts and the session lands in the store automatically.

```bash
# QQ Music (QQ scan)
cargo run --features source-qqmusic --example qqmusic_smoke auth
# QQ Music (WeChat scan)
cargo run --features source-qqmusic --example qqmusic_smoke wechat
# Kugou
cargo run --features source-kugou --example kugou_smoke auth
# Bilibili
cargo run --features source-bilibili --example bilibili_smoke auth
```

On Success the smoke prints the logged-in identity. Immediately after, run the gated modules with the same process exit:

```bash
cargo run --features source-qqmusic --example qqmusic_smoke user stream
cargo run --features source-kugou --example kugou_smoke user stream
cargo run --features source-bilibili --example bilibili_smoke search stream favorites
```

Expected: the previously gated modules return real data (QQ stream code 10006 disappears, Kugou v5 URLs resolve, Bilibili keyword search stops returning v_voucher). Note: the smoke processes are independent, so session persistence (feature `session-file`) must be enabled for the login to carry across runs; otherwise keep the phone ready and chain the modules in one filtered run.

## 4. Known non-implementable items (documented, not fixable in code)

- QQ Music QRC 3DES: needs a verifiable key reference; the crypt:0 plain path covers the readable lyrics.
- Soda: CENC-encrypted MP4s require host-side decryption, which conflicts with the StreamInfo-only contract.
- Qianqian/JOOX: endpoints closed since the audit (probes 2026-11-17); re-probe before implementing.
- Bilibili lyrics: the platform exposes no lyric API.

## 5. Parity checklist after login

- [ ] qqmusic user: user_playlists returns the account's lists
- [ ] qqmusic stream: a VIP track resolves past code 10006
- [ ] kugou stream: v5 (and v3 fallback) URLs resolve with the session
- [ ] kugou user: user_playlists lists created + collected folders
- [ ] bilibili search: keyword query returns results past v_voucher
- [ ] bilibili favorites: folders list and their media resolve

## 6. Provider status after this workflow

9 of 12 VoiceFox providers implemented and live-verified (netease, qqmusic, kugou, kuwo, migu, bilibili, jamendo, apple, fivesing). 2 deferred with recorded reasons (soda CENC model, qianqian/joox closed endpoints). 1 planned (LX Music JS adapter: contract scaffold done, engine wiring host-provided).
