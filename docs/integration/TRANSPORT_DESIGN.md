# TRANSPORT_DESIGN.md: unified transport layer (M1 slice `transport-core`)

Design date: 2026-11-19; revision 2 the same day, incorporating the M1 review
(P1-P8): `Http::send_once` unclassified entry (P1), idempotency gate extended
to body-level retries (P2), resolve-timeout budget interaction declared (P3),
`Build` errors short-circuited by the engine (P4/P5), error-text drift
declared (P6), download-profile `timeout: None` trap removed (P7), migu HEAD
probe note (P8). Revision 3 the same day, incorporating the second review
(P-1..P-7): migu resource-info POST face declared (P-1), QR/WX failover-loop
send entry pinned (P-2), netease fallback HEAD probe moved to `send_once`
with its 3s per-request timeout (P-3), non-UTF-8 header values treated as
absent by the accessors (P-4), worst-case wall-time arithmetic corrected
(P-5), kugou v3 gateway POST declared (P-6), `code_of` missing-code default
pinned (P-7). Revision 4 (2026-11-19): migration-status annotations only, after
all ten M1 slices landed (core plus nine provider migrations); no design
change. Input: the M1 transport audit (12 reqwest `Client`
construction sites, 9 hand-written retry loops, all cited with `path:line` in
the audit notes) plus the M0 baselines (`FEATURE_DEPENDENCY_MATRIX.md`,
`MODEL_MAPPING.md`, `CI_REQUIREMENTS.md`). Everything below was written after
re-reading the cited code in this session. Scope of this slice: design doc +
`src/transport.rs` core + unit tests. **No provider is migrated here; the nine
provider slices consume this seam in later batches** (migration order in §7).
`Cargo.toml` is untouched (zero-diff constraint, see §3). Revision 3 also
fixes `TransportResponse` accessors in code (§5.5 row 9): lossy-stored
non-UTF-8 header values are treated as absent, matching today's
`to_str().ok()` drop semantics. Revision 5 (2026-11-19): the qianqian
provider slice (feature `source-qianqian`, born on this seam) was appended to
the §3 `cfg(any(...))` gate; §7 migration markers are unchanged (qianqian is
not an M1 migration, it never had a pre-transport face).

Status vocabulary: designed / implemented / has tests / CI gated (see §8 for
which commands were actually executed for this slice).

Migration status (2026-11-19, M1 closed): the core slice and all nine provider
slices are migrated and passed their per-slice test/clippy gates (per-feature
evidence in the M1 entry of docs/IMPLEMENTATION_STATUS.md); section 7 carries
the per-slice markers. The download engine keeps its own loops (§6) and
`download.rs` references no `crate::transport`.

## 1. Layer position and non-goals

```
host application
   |
resolve.rs  StreamCache (resolve.rs:101-107 short-circuit, 125-127 write)
   |          ProviderHealthMonitor circuit breaker (resolve.rs:109-116
   |          try_acquire open-state check, 122-156 record_success/record_failure)
   |        ── both stay untouched; they sit ABOVE the HTTP face ──
   |
provider (source_*.rs)            platform-specific logic stays here:
   |  · signatures:  netease eapi AES-128-ECB (source_netease.rs:277-295),
   |    qq zzc_sign SHA1 (source_qqmusic.rs:355-382), migu mg_sign MD5
   |    (source_migu.rs:194-213), bilibili WBI md5 (source_bilibili.rs:335-359),
   |    jamendo x-jam-call sha1+nonce (source_jamendo.rs:120-131),
   |    kuwo sign_web Secret header (source_kuwo.rs:721-727)
   |  · cookie whitelists: netease request() (source_netease.rs:181-203),
   |    bilibili cookie-name filter (source_bilibili.rs:142-160)
   |  · decoders: kugou KRC XOR+zlib (source_kugou.rs:275-290),
   |    kuwo yeelion XOR+GB18030 (source_kuwo.rs:305-359),
   |    migu strategy response decrypt
   |  · host-failover loops (not time retries, stay put): netease QR
   |    three-host failover (source_netease.rs:1036-1162), qq WX musicu
   |    fallback hosts (source_qqmusic.rs:82-85, used from 1067+),
   |    qq manual redirect walk MAX_REDIRECTS=10 (source_qqmusic.rs:1782-1817),
   |    bilibili -403/-412 key-rotation single retry (source_bilibili.rs:268-281)
   |
transport.rs (this slice)         injectable seam + ClientProfile +
   |                              unified retry engine + mock transport
   |
retry.rs                          classify_http (retry.rs:68-80) /
   (unchanged)                    classify_api (retry.rs:83-104) /
                                  StepOutcome (retry.rs:15-24) / ApiCodes
                                  (retry.rs:27-34) with the four per-provider
                                  constants (retry.rs:36-62)
```

Non-goals, recorded from the audit:

- `source_youtube_music.rs` is NOT migrated: rustypipe 0.11.4 ships its own
  HTTP + PO-token stack (`source_youtube_music.rs:38-51`, `RustyPipe::builder`);
  the module has no `reqwest::Client` of its own.
- `lx-js-bridge` has no network calls at all (grep over `lx_js*.rs` and
  `provider/*.rs` for `Client`: zero hits).
- `download.rs` engine loops are NOT merged into the request-level retry
  engine; evaluation and conclusion in §6.

## 2. Terminology correction carried into the design

The brief named `classify_response`; that function does not exist. The actual
classifiers in `src/retry.rs` are `classify_http` (`retry.rs:68-80`) and
`classify_api` (`retry.rs:83-104`), plus `StepOutcome` (`retry.rs:15-24`) and
`ApiCodes` (`retry.rs:27-34`). The unified layer calls them and does not
re-implement classification. This is a hard constraint, and after migration
the engine in `transport.rs` is the only place HTTP-level classification
happens (each provider slice stops carrying its own loop).

## 3. Feature gating (zero `Cargo.toml` change)

`src/lib.rs` gains exactly one declaration, following the existing
`pub(crate) mod retry;` pattern (`lib.rs:17`):

```rust
#[cfg(any(
    feature = "source-netease",
    feature = "source-qqmusic",
    feature = "source-kugou",
    feature = "source-kuwo",
    feature = "source-migu",
    feature = "source-bilibili",
    feature = "source-jamendo",
    feature = "source-apple",
    feature = "source-fivesing",
    feature = "source-qianqian",
    feature = "download",
))]
pub(crate) mod transport;
```

Why `cfg(any(...))` on the eleven reqwest-bearing features instead of a new
internal feature:

1. Zero `Cargo.toml` diff beyond the feature line itself (tasks.md §15.2
   minimal-modification). A new internal feature would need edits to more
   feature lines plus a new public feature face.
2. The eleven-way list self-validates under CI: the `features` job enumerates
   every non-default feature from `cargo metadata` and runs
   `cargo check --locked --no-default-features --features <f>` per feature
   (`ci.yml:78-83`). If a provider referenced `crate::transport` while the
   `any` list missed that provider's feature, the isolated check would fail
   to compile. The list is therefore wrong-on-omission, never
   wrong-silently.
3. `--no-default-features` compiles none of the eleven, so the module (and
   reqwest) stay out of the smallest build. The no-default boundary was
   re-verified in this session (§8, command 1); it is also CI-gated
   (`ci.yml:62-63`, `ci.yml:115`).
4. Precedent for module-level feature gates outside `lib.rs`: `retry.rs:36-62`
   gates the four `*_CODES` constants and `lyrics.rs` gates `lyrics-search`
   items (`FEATURE_DEPENDENCY_MATRIX.md` §1/§3).

Module visibility is `pub(crate)`: every consumer (`source_*.rs`,
`download.rs`) is in-crate; nothing leaks into the public API, matching
`retry` (`lib.rs:17`).

Dependency safety inside the module: it uses only `reqwest` (present in every
gated feature row, `Cargo.toml:28-36,42`), `serde_json` (unconditional,
`Cargo.toml:48`), `async-trait` (unconditional, `Cargo.toml:45`) and std. It
does NOT use `tracing`: the provider wrappers keep their existing
`tracing::debug!` lines at call sites (`source_netease.rs:120,222`,
`source_qqmusic.rs:197` and the other five loops) so no feature loses a dep it
does not already carry through its own row.

## 4. Public API of `src/transport.rs`

All items are `pub(crate)`. Signatures as implemented in this slice:

### 4.1 Transport seam

```rust
#[async_trait::async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Send one request; implementations must fully buffer the response body.
    async fn send(&self, request: &TransportRequest)
        -> Result<TransportResponse, TransportError>;
}

/// Cloneable handle the providers hold instead of a raw `reqwest::Client`.
#[derive(Clone)]
pub struct Http { /* Arc<dyn Transport> */ }

impl Http {
    pub fn from_profile(profile: &ClientProfile) -> Self;   // reqwest-backed
    pub fn from_mock(mock: MockTransport) -> Self;          // test seam
    pub fn from_shared_mock(mock: Arc<MockTransport>) -> Self; // test seam
    pub async fn send_raw(&self, request: &TransportRequest,
        policy: &RetryPolicy) -> Result<TransportResponse, TransportError>;
    pub async fn send_once(&self, request: &TransportRequest)
        -> Result<TransportResponse, TransportError>;
    pub async fn send_api_json(&self, request: &TransportRequest,
        policy: &RetryPolicy, codes: &ApiCodes,
        code_of: impl Fn(&serde_json::Value) -> (i64, String))
        -> Result<serde_json::Value, TransportError>;
}

/// The one reqwest-backed implementation, built from a ClientProfile.
pub struct ReqwestTransport { /* Client */ }
impl Transport for ReqwestTransport { /* build, execute, buffer */ }
```

- `send_once` is the unclassified single-shot entry: exactly one attempt, no
  `classify_http`, no retry; the response comes back whatever its status. It
  exists because `classify_http` makes every 3xx a `Fatal`
  (`retry.rs:76-78`), while two faces have 3xx (or per-hop 3xx) as their
  SUCCESS path: migu `fetch_listen_url` reads the 302 `Location` as the
  answer (`source_migu.rs:1500-1519`, on the streaming resolve path) and qq
  `follow_redirects` walks redirect-none hops harvesting `Set-Cookie` and
  `Location` per hop (`source_qqmusic.rs:1782-1817`; `login_client` call
  sites at 1640/1657/1708/1794). Through `send_raw` those faces would get
  `Err(Status("HTTP 302"))` with the response dropped. Because redirect-none
  profiles return the hop response directly, no CDN body is ever buffered
  here; `send_once` is also the right entry for any future caller that must
  see a non-2xx status itself. Providers still hold `Http`; no one bypasses
  the handle.
- The host-failover loops that keep POST RequestBuilders today also go
  through `send_once`, one attempt per hop: netease QR key/poll POSTs inside
  `for host in HOSTS` (`source_netease.rs:1039,1085`) and qq WX musicu POSTs
  inside the fallback-host chain (`source_qqmusic.rs:1070`). After migration
  the provider no longer holds a raw `reqwest::Client` (it holds `Http`),
  so these loops build `TransportRequest::post(...)` with their existing
  headers/body and call `send_once` per host; the loop's own host rotation is
  the retry, exactly as today, and `send_once` adds no time retry on top.
  This keeps the no-raw-client rule (§4.1) enforceable: no `reqwest::Client`
  escapes `ClientProfile`/mock injection.
- `send_raw` covers the raw callers today: qq `raw_get`
  (`source_qqmusic.rs:185-214`), kugou `raw_get` (`source_kugou.rs:141-170`,
  whose `get_json` then parses and classifies with budget 0; after migration
  kugou/kuwo parse-and-classify moves onto `send_api_json` with
  `api_retries: 0`, see §5.3), kuwo `raw_get` (`source_kuwo.rs:165-203`),
  bilibili `request_json` (`source_bilibili.rs:161-193`, parse-only after the
  loop), and the single-shot faces of migu (`fetch_bytes/fetch_text/fetch_json`,
  `source_migu.rs:143-184`), jamendo (`api_get`, `source_jamendo.rs:78-111`),
  apple (`raw_text`, `source_apple.rs:91-104`) and fivesing (`get_json`,
  `source_fivesing.rs:84-97`).
- `send_api_json` covers netease `get_json` (`source_netease.rs:108-159`) and
  `eapi` (`source_netease.rs:204-269`), qq `musicu_inner`
  (`source_qqmusic.rs:268-334`), kugou `get_json` (`source_kugou.rs:171-198`),
  kuwo `get_json_with` (`source_kuwo.rs:209-229`). The `code_of` closure
  extracts the provider's `(code, message)` field names (netease `code`/
  `message`, qq `code`/`message|msg`, kugou `error_code`/`error|message|msg`,
  kuwo `code`/`msg`): extraction stays provider-side, classification is
  centralized in `retry::classify_api`. Pinned default (review P-7): when
  the body lacks the code field, `code_of` MUST fall back to `codes.ok`,
  exactly like today's `unwrap_or(<CODES>.ok)` (e.g. `source_netease.rs:143`),
  so a missing-code body counts as success; writing `unwrap_or(0)` instead
  would flip netease's missing-code bodies into `Fatal("API code 0")`.

### 4.2 Request / response / error

```rust
pub enum HttpMethod { Get, Post, Head }

pub struct TransportRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,   // order preserved; duplicates kept
    pub body: Option<Vec<u8>>,            // buffered; eapi form body / musicu JSON
    pub timeout: Option<Duration>,        // per-request override (WX_POLL_TIMEOUT)
    pub idempotent: bool,                 // retry gate, see §5.2
}
// Constructors: get(url) / post(url) / head(url) default idempotent to
// true for Get/Head and false for Post; builders: .header(k, v),
// .body(bytes), .timeout(d), .idempotent(bool).

pub struct TransportResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,   // multi-value preserved (Set-Cookie)
    pub url: Option<String>,              // final URL after redirects
    pub body: Vec<u8>,
}
// Helpers: header(name) -> Option<&str>, all(name) -> Vec<&str>,
// text() -> Cow<str>, json::<T>() -> Result<T, TransportError>,
// retry_after() -> Option<Duration> (integer seconds only), is_success().

pub enum TransportError {
    Build(String),        // request never sent (invalid URL/header, mock script
                          // dry); NEVER retried by the engine (deterministic)
    Transport(String),    // reqwest failure after retries exhausted / gated off
    Status(String),       // classify_http Fatal text, e.g. "HTTP 404"
    Body(String),         // JSON decode of a 2xx body failed
    Api(String),          // classify_api Fatal text, e.g. "API code 881: ...";
                          // also the terminal when a body Retry verdict hits a
                          // non-idempotent request (see §5.2)
    SessionExpired,       // classify_api SessionExpired; caller marks the session
}
impl TransportError {
    pub fn into_provider_error(self, provider: &str, label: &str)
        -> crate::Error;  // Error::Provider { provider, message: "{label}: {self}" }
}
```

`into_provider_error` is a convenience, not a mandate: its default text is not
byte-identical with every provider today (eapi errors carry no label prefix,
`source_netease.rs:229,240`; session expiry reads "login required or session
expired" in netease/qq/kugou and "login required" in kuwo; fivesing prefixes
parse failures with "parse failed: ", `source_fivesing.rs:96`). Slices that
must preserve exact host-visible texts map the variants themselves; row 7 of
§5.5 declares the drift.

`TransportResponse` is a plain buffer: reqwest's `Response` cannot be
constructed outside a real connection, so the seam buffers
`status/headers/url/bytes` once (`ReqwestTransport::send`) and everything
downstream (retry engine, provider parsing, mocks) works on plain data.
Cost: mid-body connection resets surface as `TransportError::Transport`
(retryable) instead of today's terminal `.json()`/`.bytes()` error. This is a
declared behavior improvement, listed in §5.5. A `content_length()` helper
parses the `content-length` header because the buffered model has no
reqwest-style accessor (migu's HEAD probe reads it, §7.9).

### 4.3 ClientProfile

```rust
pub struct ClientProfile {
    pub timeout: Option<Duration>,           // providers: 15s (all 11 sites)
    pub connect_timeout: Option<Duration>,   // download: clamp(1..30) (download.rs:256)
    pub redirect_none: bool,                 // false = follow (default);
                                             // true = Policy::none
    pub user_agent: Option<String>,          // netease UA, download USER_AGENT
    pub proxy: Option<String>,               // download only (download.rs:259-263)
    pub pool_idle_timeout: Option<Duration>, // download 120s
    pub pool_max_idle_per_host: Option<usize>, // download 16
    pub tcp_keepalive: Option<Duration>,     // download 30s
}
// Preset: ClientProfile::provider_default() = { timeout: 15s, everything else None/false }
// NOT valid for the download engine: its profile must carry timeout: None
// (no client-level total timeout, download.rs:243-247; see §6).
```

Mapping of the twelve current construction sites (post-migration intent,
executed by the provider slices, not here):

| Site | Profile |
|---|---|
| netease (`source_netease.rs:53-57`) | `provider_default()` + `user_agent(UA)` |
| qq business client (`source_qqmusic.rs:109-112`) | `provider_default()` |
| qq login client (`source_qqmusic.rs:113-117`) | `provider_default()` + `redirect_none: true` |
| kugou (`source_kugou.rs:72-75`) / kuwo (`source_kuwo.rs:119-122`) / jamendo (`source_jamendo.rs:50-53`) / apple (`source_apple.rs:60-63`) / fivesing (`source_fivesing.rs:54-57`) / bilibili (`source_bilibili.rs:94-97`) | `provider_default()` |
| migu client (`source_migu.rs:106-109`) | `provider_default()` |
| migu listen client (`source_migu.rs:110-114`) | `provider_default()` + `redirect_none: true` |
| download engine (`download.rs:253-265`) | NOT migrated (§6); when adopted later, `connect_timeout`, pool fields, `tcp_keepalive`, `proxy`, `user_agent` cover every field it sets |

`redirect_none` maps to `reqwest::redirect::Policy::none()` exactly as the two
existing `login_client`/`listen_client` sites do. Header behavior (netease UA
+ per-request UA, cookie whitelists) is unchanged: per-request headers from
`TransportRequest.headers` are applied by `RequestBuilder::header` exactly
like today's `request()` helpers, so today's duplication semantics carry over
verbatim (netease currently sets UA both client- and request-level,
`source_netease.rs:54-55` and `source_netease.rs:183`).

Cookies remain OUT of the client (no `cookie_store`): all providers inject
`Cookie` headers from `SessionStore` per request (`source_netease.rs:185-201`,
`source_qqmusic.rs:171-181`), and qq/migu redirect-none clients harvest
`Set-Cookie` per hop. That model is preserved by `TransportResponse.headers`.

Per-request timeout override (`TransportRequest.timeout`) covers the qq WX
long-poll constant `WX_POLL_TIMEOUT = 35s` (`source_qqmusic.rs:79`): reqwest's
per-request timeout overrides the client-level one. Redirect policies are not
introspectable on a built `reqwest::Client`; the unit tests assert construction
succeeds and the migu/qq slices assert hop-harvest behavior against their own
live/smoke paths.

### 4.4 Mock seam

```rust
pub struct MockTransport { /* handler + recording log */ }
impl MockTransport {
    pub fn new(handler: impl Fn(&TransportRequest) ->
        Result<TransportResponse, TransportError> + Send + Sync + 'static) -> Self;
    pub fn script(results: impl IntoIterator<Item =
        Result<TransportResponse, TransportError>>) -> Self;  // pops per send
    pub fn single(response: TransportResponse) -> Self;
    pub fn recorded(&self) -> Vec<TransportRequest>;  // assertion surface
}
impl Transport for MockTransport { /* record + dispatch */ }
```

- Handler form is for data-driven tests (`url`/`headers`/`idempotent`
  assertions, per-URL routing); script form drives the retry matrix; a script
  that runs dry returns `Err(TransportError::Build("mock transport script
  exhausted after N responses"))` so tests fail loudly instead of hanging.
  Because the engine never retries `Build` (P4/P5 fix), a dry script also
  cannot inflate `recorded()` through burnt retries: the exhaustion test
  asserts 2 recorded sends on an idempotent GET (the 503 leg plus the
  exhausted retry), not 5.
- `recorded()` returns every request in send order with its resolved
  `idempotent` flag; the retry-matrix tests assert attempt counts with it.
- `Http::from_mock(mock)` is the injection point provider slices use
  (e.g. `NeteaseProvider::with_transport(Http::from_mock(...))` later); tests
  that need the recording log use `Http::from_shared_mock(Arc<MockTransport>)`
  so both sides see the same instance. The mock lives outside `#[cfg(test)]`
  so provider-slice tests can use it; it is `pub(crate)`, so it never reaches
  the public API.

### 4.5 RetryPolicy

```rust
pub struct RetryPolicy {
    pub transport_retries: usize,  // extra attempts for transport errors and
                                   // 429/5xx (classify_http budget)
    pub api_retries: usize,        // budget handed to classify_api; clamped to
                                   // transport_retries inside the engine
    pub base_delay: Duration,      // exponential base
    pub max_delay: Duration,       // exponential cap
    pub jitter: Duration,          // added on top, in [0, jitter]
    pub retry_after_cap: Duration, // server Retry-After clamp
}
impl RetryPolicy {
    pub const fn standard() -> Self;  // 4 / 4 / 500ms / 8s / 250ms / 10s
}
```

## 5. Retry strategy (all classification via `retry.rs`)

### 5.1 The engine

`Http::send_raw` and `Http::send_api_json` share one engine loop
(`transport.rs::drive`), structured as:

1. `transport.send(request)` errors: `Build` is short-circuited immediately
   (the request never went out, deterministic; a mock script running dry also
   surfaces as `Build`, so dry scripts do not burn the budget). Any other
   error: if `request.idempotent` and `attempt < transport_retries`, back off
   (`next_delay`) and resend; else return the error. This matches today's
   behavior of all seven loops, where any transport error is retryable
   regardless of kind (`source_netease.rs:119-123` is representative).
2. Response: `retry::classify_http(status, attempt, transport_retries)`
   (`retry.rs:68-80`) decides. `Retry`: back off and resend, honoring the
   response's `Retry-After` header if present; the §5.2 idempotency gate
   applies here as well, so a non-idempotent request collapses the `Retry`
   verdict into the terminal `HTTP {status}` message instead of being resent.
   `Fatal(msg)`: `Err(TransportError::Status(msg))`. Otherwise fall through.
3. Body phase: `send_raw` accepts every classified-Ok response;
   `send_api_json` parses JSON (`Err(Body)` on failure, terminal as today),
   calls `code_of(&value)` and hands the result to
   `retry::classify_api(code, message, attempt, api_budget, codes)`
   (`retry.rs:83-104`). `Ok`: return the value. `Retry`: gated by §5.2 like
   the other two resend sites; when allowed, back off and resend.
   `SessionExpired`: `Err(TransportError::SessionExpired)` (provider marks
   the session, same as `mark_session_expired` today). `Fatal`:
   `Err(TransportError::Api)`.

Attempt counter: one shared counter per call; the same index feeds
`classify_http` and `classify_api`, exactly like the shared loop indices in
the seven current loops (`source_netease.rs:110-114` chains one `BACKOFF_MS`
table into both classifiers).

### 5.2 Idempotency gate (declared behavior for the two POST loops)

Every current loop retries POST bodies on a fixed table: netease `eapi`
(`source_netease.rs:204-269`) and qq `musicu_inner`
(`source_qqmusic.rs:268-334`) both resend the encrypted/signed body. The
engine therefore retries whatever `TransportRequest.idempotent` says, with
method-based defaults (`Get`/`Head` → true, `Post` → false), and the gate
covers ALL THREE resend sites: transport errors, `classify_http` Retry
(429/5xx) and `classify_api` Retry (throttled body codes, e.g. netease 405).
Without the third site, a POST slice that forgot `.idempotent(true)` would
still silently resend its body on 405 while transport errors visibly stopped
retrying; with it, every refusal is terminal and loud
(`TransportError::Api("retryable body verdict on a non-idempotent request;
not resent")`). The two POST loops set `.idempotent(true)` at their call
sites, keeping their behavior declarable and greppable; everything else keeps
the safe default.

### 5.3 Budgets converge into the profile

The current budgets differ: 4-entry tables (`[600,1200,2400,4000]` in netease
`get_json`, qq `raw_get`, kugou, kuwo, bilibili) and 3-entry tables
(`[400,900,1800]` in netease `eapi`, qq `musicu_inner`). They converge to
`RetryPolicy::standard()` (`transport_retries: 4, api_retries: 4`); kugou and
kuwo pass `api_retries: 0` because their `classify_api` calls run with
`budget = 0` today (`source_kugou.rs:188-189`, `source_kuwo.rs:217-218`), so
their body-level codes never retry, only transport failures and 429/5xx do.
The engine clamps `api_retries` to `transport_retries` so the two counters can
never diverge into an unterminated loop.

### 5.4 Backoff: exponential + jitter + Retry-After

`next_delay(policy, attempt, retry_after)`:

- exponential: `base_delay * 2^min(attempt, 6)`, capped at `max_delay`;
- jitter: `hash(system nanos ^ monotonic counter) % (jitter_ms + 1)` added on
  top (mirrors the download engine's lockstep-avoidance idea,
  `download.rs:1011-1015`);
- `Retry-After`: parsed from the response header (integer seconds only; the
  HTTP-date form is out of scope, nothing in the audited set sends it),
  clamped to `retry_after_cap` (10s) so a hostile or misconfigured server
  cannot stall a resolve, then `max(exponent, clamped_retry_after)`.

Timing changes versus today, stated once and normative for all nine provider
slices: fixed tables become exponential backoff with jitter (the audit's M1
goal), no loop reads `Retry-After` today (all seven gain it), the two
3-entry loops gain a 5th attempt (eapi, musicu_inner), and the zero-retry
providers (migu, jamendo, apple, fivesing) gain full `standard()` retries.
Each of those slices must restate the upgrade in its own acceptance notes,
per the audit.

Interaction with the resolve-level timeout (declared, review P3):
`resolve.rs:118` wraps every `provider.resolve(...)` in
`tokio::time::timeout(per_provider_timeout, ...)`, where
`per_provider_timeout` is host-supplied with no default (`resolve.rs:33`).
Worst case under `standard()` (corrected arithmetic, review P-5): 5 sends
× 15s client timeout + 4 backoff sleeps between them (0.5 + 1 + 2 + 4 = 7.5s,
+ at most 4 × 250ms jitter ≈ 1s) ≈ 83.5s per HTTP call; a server-sent
`Retry-After` can raise individual sleeps to the 10s cap, bounding the worst
case at ≈ 5 × 15 + 4 × 10 + 1 ≈ 116s. The resolve chain makes several such
calls, so a host tuned for today's single-shot migu/jamendo/apple/fivesing
(≈15s worst case) will cut the retry ladder mid-flight. Cancellation itself
is clean (tokio drops the future; `tokio::time::sleep` cancels between
awaits, no zombie retries past the deadline), but the failure is then masked:
`resolve.rs:150-157` replaces the real provider error with "provider
resolution timed out" and feeds `record_failure` into the circuit breaker.
Normative consequences for slices 6-9 (jamendo, apple, fivesing, migu):
(1) the acceptance notes must state this interaction and either declare the
required host-side `per_provider_timeout` retune (≥ the slice's worst-case
budget) or pass a reduced policy (fewer `transport_retries`) with the number
declared; (2) `RetryPolicy::standard()` is the default, not a mandate, for
these four slices, and row 3 of §5.5 applies per FACE, not per provider
(P-1): migu's retrying faces are the GET family (`fetch_bytes/fetch_text/
fetch_json`) and, if the slice declares it, the resource-info POST; the
`fetch_listen_url` and `probe_served_size` faces are `send_once` single
shots and stay outside any retry ladder. The netease/qq/kugou/kuwo/bilibili
slices keep their existing retry semantics under the resolve timeout (5
sends vs today's 5 sends for the 4-entry loops; +1 send for eapi/musicu), so
their exposure changes only marginally.

### 5.5 Declared behavior deltas for the whole migration

| # | Today | After |
|---|---|---|
| 1 | Fixed backoff tables per loop | `standard()` exponential + jitter + Retry-After |
| 2 | 3-entry loops (eapi, musicu_inner): at most 4 sends | at most 5 sends (4 retries) |
| 3 | migu/jamendo/apple/fivesing: single shot, no retry | full `standard()` policy per GET face, subject to the §5.4 resolve-timeout reconciliation. Per-face for migu (review P-1): the resolve resource-info POST (`source_migu.rs:1327-1335`, form body `resourceId=...`) is a read-only query POST; the slice either keeps it single-shot via the method-default `idempotent: false` (row 3 then does NOT cover it) or marks `.idempotent(true)` to gain retries, which is a declared behavior change that must appear in the slice's acceptance notes; the `fetch_listen_url` and `probe_served_size` faces are `send_once` and never retry |
| 4 | Body-read failures terminal | mid-body reset retried as `TransportError::Transport` |
| 5 | POST retried unconditionally | retried iff `idempotent`, now at all three resend sites (transport error, 429/5xx, throttled body code) |
| 6 | kugou/kuwo body codes: never retried | unchanged via `api_retries: 0` |
| 7 | Provider error texts (eapi bare message, "login required or session expired" vs "login required" vs "login session expired", fivesing "parse failed: " prefix) | `into_provider_error` is the convenience default; slices that must keep today's byte-exact host-visible texts map `TransportError` variants themselves instead of calling it |
| 8 | Single-shot providers finish inside any host `per_provider_timeout` | resolve-level `tokio::time::timeout` (`resolve.rs:118`) can cancel the new retry ladder and mask the error as "provider resolution timed out" + `record_failure` (§5.4); each slice declares its reconciliation |
| 9 | Non-UTF-8 header values silently DROPPED by `HeaderValue::to_str().ok()` (migu Location parse `source_migu.rs:1506-1512`, qq `collect_cookies` `source_qqmusic.rs:1602-1604`, netease HEAD probe content-type `source_netease.rs:500-504`) | adapter stores them lossily with U+FFFD markers; `header()`/`all()`/`retry_after()`/`content_length()` treat mangled values as ABSENT (skip and continue to a later usable duplicate), so a wild non-ASCII 302 `Location` still reads as NotFound, not a mangled URL (review P-4; test `non_utf8_header_values_are_treated_as_absent`) |

Everything else is behavior-preserving: non-2xx other than 429/5xx stays
terminal with the same `HTTP {status}` text (`classify_http`, `retry.rs:76-78`);
login-expiry codes stay terminal with session marking at the call site; JSON
decode failures stay terminal.

## 6. download.rs evaluation (ask-mandated conclusion)

**Conclusion: not merged into the request-level engine. The engine may adopt
`ClientProfile`-built construction later; its loops stay engine-side.**

Evidence read this session:

- Client construction `download.rs:253-265`: `connect_timeout(clamp(1..30))`,
  `pool_idle_timeout(120s)`, `pool_max_idle_per_host(16)`,
  `tcp_keepalive(30s)`, `user_agent`, optional `Proxy::all`. It is the only
  proxy user among the twelve sites, and every one of those knobs has a
  `ClientProfile` field (§4.3), so a later slice can replace the builder with
  a profile-built `ReqwestTransport` and expose `Transport` for control
  requests (`fetch_bytes`, `download.rs:273`) with near-zero risk. **Trap
  removed (review P7): that future profile MUST use `timeout: None`.**
  `ClientProfile::provider_default()` carries `timeout: Some(15s)`, but the
  download client deliberately sets no client-level total timeout because it
  would cut off large slow downloads mid-body (`download.rs:243-247` comment);
  the engine instead applies a per-request `request_timeout`
  (`download.rs:280`) which maps to `TransportRequest.timeout` per call.
  The `provider_default` doc comment warns about this in code.
- Loop shape: `download_single` (`download.rs:475-506`) and per-part
  `download_part` (`download.rs:682-724`) retry with
  `DownloadError::is_retryable` (`download.rs:126-138`), which is a different
  policy family: integrity/cancel/range-strategy outcomes (e.g.
  `RangesUnsupported` means strategy switch, not resend) and 408 joins the
  retry set, which `classify_http` does not retry. Merging would either dilute
  `classify_http` or fork it per caller; both violate the one-classifier
  constraint.
- Streaming: the engine consumes `bytes_stream()` for multi-hundred-MB bodies;
  `TransportResponse` is a fully buffered `Vec<u8>` by design (§4.2). Buffering
  is right for the provider API face (all provider calls are small JSON/bytes)
  and wrong for the download body path.
- The `RangesUnsupported`-driven strategy switch (`download.rs:126-138`,
  `download.rs:344-354`) is control flow above any request retry; it has no
  analogue in `retry.rs` semantics.

Recommendation recorded for the M1 tracker: migrate only the
`DownloadEngine`'s client *construction* to a `ClientProfile` in a later,
separately-gated change; keep both loops and `sleep_before_retry`
(`download.rs:1011-1015`) as engine policy. The download slice of this
campaign changes nothing in `download.rs`.

## 7. Migration order for the nine provider slices

Ordered by risk and blast radius; each slice lands alone with its own
`cargo check --locked --no-default-features --features <f>` (the §3
self-validating gate) plus its unit tests before the next one starts.

Status (2026-11-19, M1 closed): all nine slices below are migrated; the
marker after each item records its landing and its per-feature gate from the
M1 record (docs/IMPLEMENTATION_STATUS.md).

1. **netease**: biggest loop surface (`get_json` + `eapi`); existing
   `retry.rs` tests already cover its codes (`retry.rs:128-155`). Proves
   `send_api_json` end-to-end including `.idempotent(true)` on `eapi`.
   Keeps: `eapi_encrypt` (277-295), cookie whitelist (181-203). The QR
   key/poll host-failover loops (1036-1162) keep their own rotation and send
   each hop via `Http::send_once` (§4.1; today one attempt per host, no time
   retry, preserved). The resolve fallback HEAD probe (489-514) goes through
   `send_once` too, carrying its existing `.timeout(Duration::from_secs(3))`
   as `TransportRequest.timeout` (review P-3: `send_raw` would have retried
   it, 4 times on the dead-fallback path, and silently stretched 3s to the
   15s profile timeout; `send_once` keeps today's exact single-shot, 3s
   behavior).
   **Migrated and gated (2026-11-19): Client construction plus the
   `get_json`/`eapi` backoff loops merged into the unified layer; eapi
   encryption and the cookie whitelist stay provider-side; test/clippy passed
   at feature source-netease (M1 record).**
2. **qqmusic**: two profiles (business + `redirect_none` login client),
   `musicu_inner` with per-request UA + `.idempotent(true)`, per-request
   timeout for `WX_POLL_TIMEOUT`. The manual redirect walk
   (`follow_redirects`, 1782-1817) and the other three `login_client` sites
   (1640/1657/1708) go through `Http::send_once` per hop: they harvest
   `Set-Cookie`/`Location` from responses whose status may be a redirect
   their own loop resolves, which `send_raw`'s `classify_http` would collapse
   into `Err(Status("HTTP 302"))`. The WX musicu fallback-host POST chain
   (`source_qqmusic.rs:1067-1075`) likewise keeps its own host rotation and
   sends each host via `send_once` (§4.1; today single attempt per host).
   Keeps: `zzc_sign` (355-382).
   **Migrated and gated (2026-11-19): dual Client including the
   redirect-none `login_client`, the `raw_get`/`musicu_inner` loops merged;
   `zzc_sign` and the manual redirect walk stay provider-side; test/clippy
   passed at feature source-qqmusic (M1 record).**
3. **kugou**: first `api_retries: 0` slice. Keeps KRC decrypt (275-290) and
   login gate (201-229). The v3 gateway POST (`source_kugou.rs:705-740`,
   `V3_URL` + `KG-*` headers + JSON body, single-shot today) goes through
   `send_raw` with the method-default `idempotent: false`, staying exactly
   as single-shot as today (review P-6, same trap class as migu's resource
   POST: do NOT add `.idempotent(true)` there without declaring the resend
   change in the slice's notes).
   **Migrated and gated (2026-11-19): Client plus the `raw_get` loop merged;
   KRC decryption and the login cookie gate stay provider-side; test/clippy
   passed at feature source-kugou (M1 record).**
4. **kuwo**: second `api_retries: 0` slice; exercises per-request Referer
   (`source_kuwo.rs:171-173` → `TransportRequest.header`). Keeps yeelion
   decode chain (305-359), `sign_web` (721-727), illegal-session handling
   (723-750).
   **Migrated and gated (2026-11-19): Client plus the `raw_get` loop merged;
   the yeelion lyric decode and web Secret signing stay provider-side;
   test/clippy passed at feature source-kuwo (M1 record).**
5. **bilibili**: raw + parse-only (`send_raw`, then provider-side JSON).
   WBI/key-rotation and buvid bootstrap stay provider-side (268-281, 243-266,
   335-359).
   **Migrated and gated (2026-11-19): Client plus the `request_json` loop
   merged; WBI signing, the -403/-412 key-rotation retry and the buvid
   bootstrap stay provider-side; test/clippy passed at feature
   source-bilibili (M1 record).**
6. **jamendo**: first zero-retry → unified-retry upgrade (behavior delta
   §5.5 #3 declared in the slice's notes). Keeps x-jam-call (120-131).
   **Migrated and gated (2026-11-19): Client plus `api_get` merged, unified
   retry introduced from zero per the M1 record (the §5.5 #3 delta);
   x-jam-call signing stays provider-side; test/clippy passed at feature
   source-jamendo (M1 record).**
7. **apple**: same upgrade. JWT crawl and candidate probing stay out of the
   engine (141-187: auth probing, not transport retry).
   **Migrated and gated (2026-11-19): Client plus `raw_text`/`amp_get`
   merged, unified retry introduced from zero per the M1 record; JWT crawling
   and candidate probing stay provider-side; test/clippy passed at feature
   source-apple (M1 record).**
8. **fivesing**: same upgrade, smallest surface (54-97).
   **Migrated and gated (2026-11-19): Client plus `get_json` merged, unified
   retry introduced from zero per the M1 record; test/clippy passed at
   feature source-fivesing (M1 record).**
9. **migu**: last. Two profiles (listen client `redirect_none`), 302
   `Location` harvest (`fetch_listen_url`, 1494-1529) goes through
   `Http::send_once` because the 302 IS the success answer (§4.1); the
   HEAD→Range probe (`probe_served_size`, 1523-1545) also stays single-shot
   `send_once` and reads sizes from headers: the buffered model has no
   reqwest-style `content_length()` accessor for a body-less HEAD response,
   so the slice uses the new `TransportResponse::content_length()` (header
   parse) for the HEAD leg and a manual `header("content-range")` split on
   `/` for the 206 leg (review P8; `range` semantics are why this probe must
   not be retried blindly). The resolve resource-info POST
   (`source_migu.rs:1327-1335`, `RESOURCE_INFO_URL?resourceType=2` + form
   body, the first and most critical call of the streaming resolve path) is
   a read-only query POST; per §5.5 row 3 the slice declares in its notes
   whether it stays single-shot (method-default `idempotent: false`, today's
   behavior) or gains retries via `.idempotent(true)` (declared change). The
   resolve-timeout reconciliation of §5.4 is mandatory in this slice's
   acceptance notes: `fetch_listen_url` and the resource POST sit on the
   streaming resolve path.
   **Migrated and gated (2026-11-19): dual Client including the
   redirect-none `listen_client` merged and every single-shot request moved
   onto the unified layer; mg_sign and strategy decryption stay
   provider-side; test/clippy passed at feature source-migu (M1 record).**

`download`: no slice in this campaign; §6 conclusion governs.

## 8. What this slice executed (evidence)

Commands run in this session against the working repo, all exit 0 unless
stated (revision 3 re-ran the battery after the P-1..P-7 fixes):

1. `cargo check --locked --no-default-features`: smallest build stays
   reqwest-free with `mod transport` gated in (§3).
2. `cargo check --locked --no-default-features --features <f>` for all ten
   gated features (`source-netease`, `source-qqmusic`, `source-kugou`,
   `source-kuwo`, `source-migu`, `source-bilibili`, `source-jamendo`,
   `source-apple`, `source-fivesing`, `download`): every isolated check
   compiles; any further feature is CI's job (`ci.yml:69-84`).
3. `cargo test --locked --no-default-features --features source-netease
   transport::` and the same with `--features download`: 28 unit tests each
   (mock injection, retry matrix, idempotency gate at all three resend
   sites, Build short-circuit, script-exhaustion no-budget-burn, send_once
   302 passthrough, non-UTF-8 header values as absent, API-classification
   surface, profile isolation, adapter introspection).
4. `cargo test --locked --no-default-features`: pre-existing no-default suite
   unchanged (19 lib + 1 integration, same as the M0 baseline in
   `MODEL_MAPPING.md` header).
5. `cargo clippy --locked --no-default-features --features source-netease
   --all-targets -- -D warnings` and the same with `--features download`:
   clippy-clean under the CI strictness (`ci.yml:48`) for the gates run here;
   the full-matrix clippy and the rest of the gate belong to the outer script.
6. `cargo fmt --all` (applied) and `cargo fmt --all --check` (clean).

Full-matrix gates (all-features clippy/test, MSRV, tri-platform) are the outer
script's responsibility and are NOT claimed here.
