# Human test checklist (login flows that need a phone)

Everything automatable is already covered by the live smoke tests in the examples; this checklist covers only the steps that require a physical phone with QQ/WeChat/Kugou/Bilibili apps installed. Run each smoke command from the repo root and follow the printed URL with the phone.

**Scanning notes (2026-11-17 fix):** the terminal QR is now pinned to black-on-white ANSI colors. Before this fix it rendered inverted on dark-themed terminals, phone scanners silently failed, and the poll stayed on Waiting until the 180s budget ran out. Each QR smoke also writes a PNG fallback next to the terminal render (`<provider>_qr.png`); scan that if the terminal blocks still won't read. Scan with the provider's own in-app scanner (B 站 App 侧边栏「扫一扫」); the WeChat/system camera only opens the URL as a webpage and cannot confirm a desktop login. Watch the poll progression: Waiting (等待扫码) must flip to Scanned (已扫码，请在手机上确认) within ~3 seconds of scanning; if it stays Waiting, the scan never registered, so rescan or use the PNG.

## 1. QQ Music QR login (both channels)

```bash
cargo run --features source-qqmusic --example qqmusic_smoke auth
```

Steps: the smoke renders the decoded QR content in the terminal (plus `qqmusic_qr.png`), scan with the QQ Music mobile app, approve on the phone; the smoke polls until Success/expiry, then run `user` and `stream`:

```bash
cargo run --features source-qqmusic --example qqmusic_smoke user stream
```

Expected: `user` lists playlists via `user_playlists`; `stream` resolves a VIP track URL (module code 10006 disappears once the session cookie exists).

## 2. QQ Music WeChat channel

Same flow, `create_wechat_login` + `poll_qr_login` with the `wx:` prefixed key. Verify `login_status` flips to LoggedIn and `uin`/`wxuin` cookies both arrive.

## 3. Kugou QR login and lossless streams

```bash
cargo run --features source-kugou --example kugou_smoke auth
```

Scan the H5 login page URL with the Kugou app. On Success, `stream` should resolve through the signed v5 endpoint (anonymous runs are refused server-side):

```bash
cargo run --features source-kugou --example kugou_smoke stream
```

## 4. Bilibili QR login and VIP DASH

```bash
cargo run --features source-bilibili --example bilibili_smoke auth
```

Scan with the Bilibili app's in-app scanner (侧边栏「扫一扫」); a `bilibili_qr.png` fallback is written next to the terminal render. After Success, keyword search stops returning the v_voucher risk-control response, 320k DASH audio resolves for VIP content and the favorites folders list (login-gated) returns real data:

```bash
cargo run --features source-bilibili --example bilibili_smoke search stream favorites
```

## 5. NetEase user playlists (authenticated)

Requires a session cookie already stored via `NeteaseProvider::set_cookies` (QR scan or exported cookie); the anonymous `user` module skips honestly:

```bash
cargo run --features source-netease --example netease_smoke user
```

## 6. Kuwo web-signed lossless tier (lossless-entitlement account required)

Not a QR flow (first categorized 2026-11-18): Kuwo login is not implemented in the library (`capabilities.authentication` is false: the capabilities block at src/source_kuwo.rs:1560 leaves it at the false default, asserted at :2684), so this check needs a Kuwo account session with lossless entitlement attached through the SessionStore for provider id `kuwo`; cookies join every request (src/source_kuwo.rs:137). The stream module requests lossless with the web-signed playUrl first (`build_secret` Secret header, src/source_kuwo.rs:621) and falls back to the mobile ladder; anonymous runs only get placeholders for the lossless tiers, so the web tier itself stays unverified until this check runs:

```bash
cargo run --locked --features source-kuwo --example kuwo_smoke stream
```

Expected with a lossless-entitlement session: the stream module reports a real Lossless/HiRes tier resolved through the web-signed playUrl instead of the anonymous High/Mp3 fallback. Without the entitlement the web tier is expected to keep failing over to the mobile ladder, which does not count as a pass.

## Pass criteria per step

- `poll_qr_login` reaches Success with non-empty user id/display name.
- The poll status actually flipped Waiting → Scanned after the scan (a permanently-Waiting poll means the scan never registered, not a slow server).
- Session store persists the cookies (Debug output redacts values).
- The gated module that was SKIPPED/10006/v_voucher while anonymous now returns real data.
- For the Kuwo lossless check (section 6): the web-signed tier actually serves lossless or HiRes; a resolve that lands on the mobile High/Mp3 ladder is the known anonymous behavior, not a pass.
- Re-running the same module twice does not double-count or duplicate sessions.
