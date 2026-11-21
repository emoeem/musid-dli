//! Unified transport layer for the reqwest-bearing features.
//!
//! Compiled only under the twelve reqwest-carrying features (see the `mod
//! transport` gate in `lib.rs`), so `--no-default-features` stays reqwest-free.
//! Three pieces live here:
//!
//! 1. An injectable [`Transport`] seam with a reqwest-backed default
//!    ([`ReqwestTransport`], built from a [`ClientProfile`]) and a recording
//!    [`MockTransport`] for provider tests.
//! 2. A buffered request/response/error model ([`TransportRequest`],
//!    [`TransportResponse`], [`TransportError`]) so the retry engine, provider
//!    parsing and mocks all work on plain data.
//! 3. The unified retry engine ([`Http::send_raw`], [`Http::send_api_json`])
//!    that delegates every classification to the shared `retry` module
//!    (`classify_http` / `classify_api`), adds exponential backoff with jitter
//!    plus `Retry-After` support, and gates retries on a per-request
//!    `idempotent` flag.
//!
//! Providers keep every platform-specific concern (signatures, cookie
//! whitelists, decoders, host failover); the resolve layer (StreamCache and
//! ProviderHealthMonitor) sits above this seam and is untouched.
#![allow(dead_code)]
//!
//! The allow above mirrors `retry.rs`: the module lands before its consumers
//! (the eleven provider migration slices), so some helpers are unused until
//! those slices switch their request faces over.
//!
//! ## Host transport injection (0.2.1)
//!
//! The seam is public: hosts that want their own proxy/timeout budget (or an
//! entirely custom transport) construct a provider through its
//! `with_transport` constructor instead of `new()`:
//!
//! ```ignore
//! // (feature-gated providers: compile this under source-netease)
//! use musid_dli::{ClientProfile, Http, source_netease::NeteaseProvider};
//!
//! let profile = ClientProfile {
//!     proxy: Some("http://127.0.0.1:7890".into()),
//!     timeout: Some(std::time::Duration::from_secs(20)),
//!     ..ClientProfile::provider_default()
//! };
//! let provider = NeteaseProvider::with_transport(Http::from_profile(&profile));
//! ```
//!
//! `Http` is a cloneable handle over any [`Transport`] implementation —
//! `Http::new(my_transport)` rides a fully custom stack (a shared client
//! pool, a gRPC sidecar, a test mock). QQ Music and Migu additionally take a
//! second handle through `with_transports` (their login/listen faces ride a
//! separate client).

use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde::de::DeserializeOwned;

use crate::retry::{ApiCodes, StepOutcome, classify_api, classify_http};

/// HTTP method subset used by the providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Head,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Head => "HEAD",
        }
    }
}

/// A transport-agnostic request description.
///
/// `idempotent` gates every retry: it defaults to `true` for `Get`/`Head` and
/// `false` for `Post`. The two POST loops that retry their bodies today (netease
/// eapi, QQ musicu) set `.idempotent(true)` explicitly after migration so the
/// behavior stays declarable.
#[derive(Debug, Clone)]
pub struct TransportRequest {
    pub method: HttpMethod,
    pub url: String,
    /// Request headers in build order; duplicates are preserved so callers can
    /// append multi-value headers.
    pub headers: Vec<(String, String)>,
    /// Fully buffered body (eapi form body, musicu JSON, ...).
    pub body: Option<Vec<u8>>,
    /// Per-request total-timeout override (QQ `WX_POLL_TIMEOUT = 35s`);
    /// reqwest applies it instead of the client-level timeout.
    pub timeout: Option<Duration>,
    pub idempotent: bool,
}

impl TransportRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self::new(HttpMethod::Get, url, true)
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self::new(HttpMethod::Post, url, false)
    }

    pub fn head(url: impl Into<String>) -> Self {
        Self::new(HttpMethod::Head, url, true)
    }

    fn new(method: HttpMethod, url: impl Into<String>, idempotent: bool) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: None,
            idempotent,
        }
    }

    /// Append a header; order and duplicates are preserved.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = idempotent;
        self
    }
}

/// Marker the adapter's `String::from_utf8_lossy` leaves in header values that
/// were not valid UTF-8 on the wire; the accessors treat such values as absent
/// (today's `to_str().ok()` drop semantics, declared in TRANSPORT_DESIGN.md
/// §5.5 row 9).
const LOSSY: char = '\u{FFFD}';

/// Fully buffered response. Headers keep duplicates (Set-Cookie harvesting for
/// QQ/MiGu redirect-none clients) and the final URL after redirects.
#[derive(Debug, Clone)]
pub struct TransportResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub url: Option<String>,
    pub body: Vec<u8>,
}

impl TransportResponse {
    /// First matching header (case-insensitive), if any.
    ///
    /// Values that were not valid UTF-8 on the wire were stored lossily (with
    /// U+FFFD replacements) by the adapter; the accessors treat them as
    /// ABSENT, matching today's `HeaderValue::to_str().ok()` drop semantics
    /// (migu Location parse `source_migu.rs:1506-1512`, qq `collect_cookies`
    /// `source_qqmusic.rs:1602-1604`). A raw (possibly mangled) view stays
    /// available through the `headers` field for debugging.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, value)| key.eq_ignore_ascii_case(name) && !value.contains(LOSSY))
            .map(|(_, value)| value.as_str())
    }

    /// Every matching header value, in response order; lossy-mangled values
    /// are skipped (see [`TransportResponse::header`]).
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, value)| key.eq_ignore_ascii_case(name) && !value.contains(LOSSY))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T, TransportError> {
        serde_json::from_slice(&self.body).map_err(|e| TransportError::Body(e.to_string()))
    }

    /// `Retry-After` in seconds; the integer form only (the HTTP-date form is
    /// not sent by any audited provider and is left unparsed). The caller
    /// clamps the value with [`RetryPolicy::retry_after_cap`].
    pub fn retry_after(&self) -> Option<Duration> {
        let raw = self.header("retry-after")?;
        raw.trim().parse::<u64>().ok().map(Duration::from_secs)
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `Content-Length` parsed from the header. The buffered model has no
    /// reqwest-style `content_length()` accessor (a HEAD response has an empty
    /// body), so migu `probe_served_size` reads this instead
    /// (`source_migu.rs:1523-1545`); its `content-range` total stays a manual
    /// `header("content-range")` parse.
    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")?.trim().parse::<u64>().ok()
    }
}

/// Transport failure taxonomy. `Status` and `Api` carry the exact
/// `classify_http` / `classify_api` `Fatal` texts so migrated providers keep
/// today's message strings for those two variants.
///
/// The engine never retries [`TransportError::Build`]: it means the request
/// never went out (invalid URL/header value, or a mock script ran dry), so a
/// resend is deterministic no-op work.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("{0}")]
    Build(String),
    #[error("{0}")]
    Transport(String),
    #[error("{0}")]
    Status(String),
    #[error("{0}")]
    Body(String),
    #[error("{0}")]
    Api(String),
    #[error("login session expired")]
    SessionExpired,
}

impl TransportError {
    /// Convenience mapping into the crate error with the provider id and
    /// call-site label (`Error::Provider { message: "{label}: {self}" }`).
    ///
    /// The default text is NOT byte-identical with every provider today: eapi
    /// errors carry no label prefix (`source_netease.rs:229,240`), session
    /// expiry reads "login required or session expired" in netease/qq/kugou
    /// and "login required" in kuwo, and fivesing prefixes parse failures with
    /// "parse failed: ". Slices that must preserve exact host-visible texts
    /// map the variants themselves (see TRANSPORT_DESIGN.md §5.5 row 7);
    /// this helper is the fallback, not a mandate.
    pub fn into_provider_error(self, provider: &str, label: &str) -> crate::Error {
        crate::Error::Provider {
            provider: provider.to_owned(),
            message: format!("{label}: {self}"),
        }
    }
}

/// Knobs for building the reqwest client. Every field maps one of the twelve
/// audited construction sites; the download engine's site (`download.rs:253-265`)
/// is the only user of `connect_timeout` / pool / keepalive / `proxy`.
#[derive(Debug, Clone)]
pub struct ClientProfile {
    /// Client-level total timeout (providers use 15s today).
    pub timeout: Option<Duration>,
    /// Connect-phase timeout (download engine).
    pub connect_timeout: Option<Duration>,
    /// `true` maps to `reqwest::redirect::Policy::none()` (qq login client,
    /// migu listen client). `false` keeps reqwest's follow-default.
    pub redirect_none: bool,
    pub user_agent: Option<String>,
    pub proxy: Option<String>,
    pub pool_idle_timeout: Option<Duration>,
    pub pool_max_idle_per_host: Option<usize>,
    pub tcp_keepalive: Option<Duration>,
}

impl ClientProfile {
    /// The provider preset: 15s total timeout, follow redirects, nothing else.
    ///
    /// NOT for the download engine: its client deliberately has no
    /// client-level total timeout because that would cut off large slow
    /// downloads mid-body (`download.rs:243-247`, per-request
    /// `request_timeout` instead at `download.rs:280`). A download profile
    /// must set `timeout: None` and keep the per-request timeout on the
    /// request (TRANSPORT_DESIGN.md §6).
    pub const fn provider_default() -> Self {
        Self {
            timeout: Some(Duration::from_secs(15)),
            connect_timeout: None,
            redirect_none: false,
            user_agent: None,
            proxy: None,
            pool_idle_timeout: None,
            pool_max_idle_per_host: None,
            tcp_keepalive: None,
        }
    }
}

/// Retry budget and backoff shape. The seven hand-written loops converge here:
/// 4-entry tables (`[600,1200,2400,4000]`) and 3-entry tables
/// (`[400,900,1800]`) both become exponential backoff with jitter, and the
/// two tables' different body-level budgets are expressed by `api_retries`.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Extra attempts for transport errors and 429/5xx responses; also the
    /// budget index passed to `retry::classify_http`.
    pub transport_retries: usize,
    /// Budget passed to `retry::classify_api`. Kugou/kuwo pass 0 (their body
    /// codes never retry today, `source_kugou.rs:188-189`,
    /// `source_kuwo.rs:217-218`); the engine clamps this to
    /// `transport_retries`.
    pub api_retries: usize,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// Added on top of the exponential delay, in `[0, jitter]`.
    pub jitter: Duration,
    /// Clamp for a server-sent `Retry-After` so a hostile value cannot stall
    /// a resolve.
    pub retry_after_cap: Duration,
}

impl RetryPolicy {
    /// Converged preset: 4 transport retries, 4 API retries, 500ms base,
    /// 8s cap, 250ms jitter, 10s `Retry-After` cap.
    pub const fn standard() -> Self {
        Self {
            transport_retries: 4,
            api_retries: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_millis(8_000),
            jitter: Duration::from_millis(250),
            retry_after_cap: Duration::from_millis(10_000),
        }
    }
}

/// Backoff for `attempt` (zero-based): `base * 2^min(attempt, 6)` capped at
/// `max_delay`, raised to the clamped `Retry-After` when the server sent one,
/// plus `[0, jitter]` on top. Pure so the matrix is unit-testable.
fn next_delay(policy: &RetryPolicy, attempt: usize, retry_after: Option<Duration>) -> Duration {
    let exponent = attempt.min(6) as u32;
    let mut delay = policy
        .base_delay
        .saturating_mul(1_u32 << exponent)
        .min(policy.max_delay);
    if let Some(after) = retry_after {
        let after = after.min(policy.retry_after_cap);
        if after > delay {
            delay = after;
        }
    }
    delay + Duration::from_millis(jitter_millis(policy.jitter))
}

/// Bounded `[0, bound_ms]` jitter from a hashed mix of system nanos and a
/// monotonic counter (same lockstep-avoidance idea as
/// `download.rs::retry_jitter_seed`).
fn jitter_millis(bound: Duration) -> u64 {
    let bound_ms = bound.as_millis() as u64;
    if bound_ms == 0 {
        return 0;
    }
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut hash = nanos ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    hash ^= hash >> 33;
    hash % (bound_ms + 1)
}

/// Per-attempt decision made on a classified-Ok response by the body phase.
enum Step<T> {
    Done(T),
    Retry,
    Fail(TransportError),
}

/// The transport seam. Implementations must fully buffer the response body
/// (the engine and provider parsing work on plain data afterwards).
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn send(&self, request: &TransportRequest) -> Result<TransportResponse, TransportError>;
}

/// Cloneable handle providers hold instead of a raw `reqwest::Client`.
/// `from_profile` builds the reqwest-backed transport, `from_mock` injects the
/// test seam.
#[derive(Clone)]
pub struct Http {
    inner: std::sync::Arc<dyn Transport>,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http").field("transport", &"boxed").finish()
    }
}

impl Http {
    pub fn from_profile(profile: &ClientProfile) -> Self {
        Self::new(ReqwestTransport::from_profile(profile))
    }

    /// Test seam. The mock is wrapped here; when the test also needs the
    /// recording log, wrap it itself in an `Arc` and use
    /// [`Http::from_shared_mock`] so both sides see the same instance.
    pub fn from_mock(mock: MockTransport) -> Self {
        Self::new(mock)
    }

    /// Test seam that shares the mock: the caller keeps its own `Arc` and can
    /// read `MockTransport::recorded()` after driving requests through `Http`.
    pub fn from_shared_mock(mock: std::sync::Arc<MockTransport>) -> Self {
        Self {
            inner: mock, // unsized coercion: Arc<MockTransport> -> Arc<dyn Transport>
        }
    }

    fn new(inner: impl Transport) -> Self {
        Self {
            inner: std::sync::Arc::new(inner),
        }
    }

    /// Send with transport-level retries only: connection errors, timeouts and
    /// 429/5xx are retried per `retry::classify_http`; every other non-2xx is
    /// terminal. Returns the final classified-Ok response.
    pub async fn send_raw(
        &self,
        request: &TransportRequest,
        policy: &RetryPolicy,
    ) -> Result<TransportResponse, TransportError> {
        self.drive(request, policy, |_, response| Step::Done(response))
            .await
    }

    /// Single unclassified send: exactly one attempt, no `classify_http`, no
    /// retry. The response is returned whatever its status, so a 302 is a
    /// normal `Ok` value whose `headers`/`url` the caller interprets.
    ///
    /// This is the entry point for faces whose SUCCESS path is a redirect or
    /// that walk redirects themselves:
    /// - migu `fetch_listen_url` reads the 302 `Location` as the answer
    ///   (`source_migu.rs:1500-1519`);
    /// - qq `follow_redirects` walks redirect-none hops one by one, harvesting
    ///   `Set-Cookie` and `Location` per hop (`source_qqmusic.rs:1782-1817`,
    ///   `login_client` sites at 1640/1657/1708/1794).
    ///
    /// `send_raw` would collapse any 3xx into `Err(Status("HTTP 302"))` and
    /// drop the response, which is exactly wrong here. Use it anywhere a
    /// non-2xx status carries meaning the caller must see.
    pub async fn send_once(
        &self,
        request: &TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.inner.send(request).await
    }

    /// Send, parse the body as JSON and classify the provider status code with
    /// `retry::classify_api`. `code_of` extracts the provider's
    /// `(code, message)` pair (field names differ per provider); classification
    /// itself stays in the shared `retry` module. `SessionExpired` surfaces as
    /// [`TransportError::SessionExpired`] so the provider can mark its session.
    pub async fn send_api_json(
        &self,
        request: &TransportRequest,
        policy: &RetryPolicy,
        codes: &ApiCodes,
        code_of: impl Fn(&serde_json::Value) -> (i64, String),
    ) -> Result<serde_json::Value, TransportError> {
        let api_budget = policy.api_retries.min(policy.transport_retries);
        self.drive(request, policy, |attempt, response| {
            let value = match response.json::<serde_json::Value>() {
                Ok(value) => value,
                Err(error) => return Step::Fail(error),
            };
            let (code, message) = code_of(&value);
            match classify_api(code, &message, attempt, api_budget, codes) {
                StepOutcome::Ok => Step::Done(value),
                StepOutcome::Retry => Step::Retry,
                StepOutcome::SessionExpired => Step::Fail(TransportError::SessionExpired),
                StepOutcome::Fatal(message) => Step::Fail(TransportError::Api(message)),
            }
        })
        .await
    }

    /// The one retry loop. All classification goes through `crate::retry`.
    async fn drive<T>(
        &self,
        request: &TransportRequest,
        policy: &RetryPolicy,
        mut on_ok: impl FnMut(usize, TransportResponse) -> Step<T>,
    ) -> Result<T, TransportError> {
        let transport_budget = policy.transport_retries;
        let mut attempt = 0usize;
        loop {
            let response = match self.inner.send(request).await {
                Ok(response) => response,
                // `Build` means the request never went out (invalid URL or
                // header value, or a mock script ran dry): deterministic, so
                // resending cannot help. Short-circuit instead of burning the
                // retry budget on it.
                Err(error @ TransportError::Build(_)) => return Err(error),
                Err(error) => {
                    if request.idempotent && attempt < transport_budget {
                        tokio::time::sleep(next_delay(policy, attempt, None)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(error);
                }
            };
            match classify_http(response.status, attempt, transport_budget) {
                StepOutcome::Retry => {
                    // The idempotency gate covers status retries too: a
                    // non-idempotent request must never be resent after a
                    // response was received, so the Retry verdict collapses
                    // into the same terminal message the budget-exhausted
                    // path would produce.
                    if !request.idempotent {
                        return Err(TransportError::Status(format!("HTTP {}", response.status)));
                    }
                    let after = response
                        .retry_after()
                        .map(|d| d.min(policy.retry_after_cap));
                    tokio::time::sleep(next_delay(policy, attempt, after)).await;
                    attempt += 1;
                    continue;
                }
                StepOutcome::Fatal(message) => return Err(TransportError::Status(message)),
                // classify_http never returns SessionExpired; the shared
                // StepOutcome forces the arm to exist.
                StepOutcome::SessionExpired | StepOutcome::Ok => {}
            }
            let after = response
                .retry_after()
                .map(|d| d.min(policy.retry_after_cap));
            match on_ok(attempt, response) {
                Step::Done(value) => return Ok(value),
                Step::Retry => {
                    // The idempotency gate covers body-level retries too: a
                    // throttled API code must not silently resend a POST body
                    // (netease 405 over a non-idempotent eapi request is the
                    // concrete case).
                    if !request.idempotent {
                        return Err(TransportError::Api(
                            "retryable body verdict on a non-idempotent request; not resent"
                                .to_owned(),
                        ));
                    }
                    tokio::time::sleep(next_delay(policy, attempt, after)).await;
                    attempt += 1;
                    continue;
                }
                Step::Fail(error) => return Err(error),
            }
        }
    }
}

/// The reqwest-backed [`Transport`], built from a [`ClientProfile`].
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn from_profile(profile: &ClientProfile) -> Self {
        Self {
            client: build_client(profile),
        }
    }

    /// Build one reqwest request; exposed for tests and kept deterministic
    /// (method, header order, per-request timeout override).
    pub(crate) fn build_request(
        &self,
        request: &TransportRequest,
    ) -> Result<reqwest::Request, TransportError> {
        let method = match request.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Head => reqwest::Method::HEAD,
        };
        let mut builder = self.client.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        builder
            .build()
            .map_err(|error| TransportError::Build(error.to_string()))
    }
}

fn build_client(profile: &ClientProfile) -> reqwest::Client {
    let mut builder = reqwest::Client::builder();
    if let Some(timeout) = profile.timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(connect_timeout) = profile.connect_timeout {
        builder = builder.connect_timeout(connect_timeout);
    }
    if profile.redirect_none {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    if let Some(user_agent) = &profile.user_agent {
        builder = builder.user_agent(user_agent);
    }
    if let Some(pool_idle_timeout) = profile.pool_idle_timeout {
        builder = builder.pool_idle_timeout(pool_idle_timeout);
    }
    if let Some(pool_max_idle_per_host) = profile.pool_max_idle_per_host {
        builder = builder.pool_max_idle_per_host(pool_max_idle_per_host);
    }
    if let Some(tcp_keepalive) = profile.tcp_keepalive {
        builder = builder.tcp_keepalive(tcp_keepalive);
    }
    if let Some(proxy) = profile
        .proxy
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        match reqwest::Proxy::all(proxy) {
            Ok(proxy) => builder = builder.proxy(proxy),
            // A malformed proxy URL used to be dropped silently: the host
            // believed it was proxied while every request went out direct.
            // Warn loudly and keep going direct (the request itself is still
            // well-formed), so the misconfiguration is visible in logs.
            Err(error) => {
                tracing::warn!(
                    "ignoring malformed proxy URL {:?}: {} (requests will go out direct)",
                    proxy,
                    error
                );
            }
        }
    }
    // With rustls-tls the build effectively cannot fail, but a failure used
    // to substitute a DEFAULT client silently - losing the configured
    // timeout/pool/proxy budget. Fall back to a minimal client that keeps
    // the timeout and warn.
    match builder.build() {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(
                "client builder failed ({}); falling back to a minimal client with the configured timeout",
                error
            );
            let mut fallback = reqwest::Client::builder();
            if let Some(timeout) = profile.timeout {
                fallback = fallback.timeout(timeout);
            }
            fallback.build().unwrap_or_default()
        }
    }
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn send(&self, request: &TransportRequest) -> Result<TransportResponse, TransportError> {
        let req = self.build_request(request)?;
        let response = self
            .client
            .execute(req)
            .await
            .map_err(|error| TransportError::Transport(error.to_string()))?;
        let status = response.status().as_u16();
        let url = Some(response.url().to_string());
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    // Lossy on purpose: values that were not valid UTF-8 on
                    // the wire keep a U+FFFD marker (LOSSY) that the accessors
                    // then treat as absent, matching today's
                    // `to_str().ok()` drop semantics (§5.5 row 9).
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|error| TransportError::Transport(error.to_string()))?
            .to_vec();
        Ok(TransportResponse {
            status,
            headers,
            url,
            body,
        })
    }
}

/// Recording mock transport for provider tests. Handler form supports
/// data-driven routing; script form drives the retry matrix; a dry script
/// errors loudly instead of hanging.
pub struct MockTransport {
    handler: MockHandler,
    recorded: Mutex<Vec<TransportRequest>>,
}

type MockHandler = std::sync::Arc<
    dyn Fn(&TransportRequest) -> Result<TransportResponse, TransportError> + Send + Sync,
>;

impl MockTransport {
    pub fn new(
        handler: impl Fn(&TransportRequest) -> Result<TransportResponse, TransportError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            handler: std::sync::Arc::new(handler),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// Canned results popped per send, in order.
    pub fn script(
        results: impl IntoIterator<Item = Result<TransportResponse, TransportError>>,
    ) -> Self {
        let results: Vec<_> = results.into_iter().collect();
        let total = results.len();
        let queue = Mutex::new(VecDeque::from(results));
        Self::new(move |_| {
            let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.pop_front().unwrap_or_else(|| {
                Err(TransportError::Build(format!(
                    "mock transport script exhausted after {total} responses"
                )))
            })
        })
    }

    /// Always answer with the same response.
    pub fn single(response: TransportResponse) -> Self {
        Self::new(move |_| Ok(response.clone()))
    }

    /// Snapshot of every request this mock has seen, in send order.
    pub fn recorded(&self) -> Vec<TransportRequest> {
        self.recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn send(&self, request: &TransportRequest) -> Result<TransportResponse, TransportError> {
        self.recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.clone());
        (self.handler)(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn resp(status: u16) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: Vec::new(),
        }
    }

    fn json_resp(status: u16, body: &str) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: body.as_bytes().to_vec(),
        }
    }

    fn transport_err(message: &str) -> TransportError {
        TransportError::Transport(message.to_owned())
    }

    /// Millisecond-scale policy so the retry matrix finishes instantly while
    /// still exercising every engine path.
    fn test_policy() -> RetryPolicy {
        RetryPolicy {
            transport_retries: 4,
            api_retries: 4,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            jitter: Duration::ZERO,
            retry_after_cap: Duration::from_millis(5),
        }
    }

    fn netease_like_codes() -> ApiCodes {
        ApiCodes {
            ok: 200,
            expired: &[301, 512],
            throttled: Some(405),
        }
    }

    fn netease_like_code_of(value: &serde_json::Value) -> (i64, String) {
        (
            value["code"].as_i64().unwrap_or(200),
            value["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_owned(),
        )
    }

    /// Build an `Http` handle sharing the mock so tests can assert on the
    /// recording log afterwards.
    fn shared(
        script: impl IntoIterator<Item = Result<TransportResponse, TransportError>>,
    ) -> (Http, Arc<MockTransport>) {
        let mock = Arc::new(MockTransport::script(script));
        (Http::from_shared_mock(mock.clone()), mock)
    }

    #[tokio::test]
    async fn mock_transport_injection_delivers_and_records() {
        let (http, mock) = shared([Ok(json_resp(200, r#"{"code":200,"data":["hit"]}"#))]);
        let request = TransportRequest::get("https://example.test/api")
            .header("user-agent", "ua")
            .header("cookie", "a=1");
        let value = http
            .send_api_json(
                &request,
                &test_policy(),
                &netease_like_codes(),
                netease_like_code_of,
            )
            .await
            .expect("single-shot must succeed");
        assert_eq!(value["data"][0], "hit");
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].url, "https://example.test/api");
        assert_eq!(recorded[0].method, HttpMethod::Get);
        assert!(recorded[0].idempotent);
        assert_eq!(
            recorded[0].headers,
            vec![
                ("user-agent".to_owned(), "ua".to_owned()),
                ("cookie".to_owned(), "a=1".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn retry_matrix_over_http_statuses() {
        // (status, recovers): retryable rows burn the whole budget (4 failing
        // sends on attempts 0..3) then recover on attempt 4; fatal rows stop
        // after the first send with the exact classify_http text.
        let matrix = [
            (429u16, true),
            (500, true),
            (502, true),
            (503, true),
            (404, false),
            (400, false),
            (408, false),
        ];
        for (status, recovers) in matrix {
            let mut script: Vec<Result<TransportResponse, TransportError>> = if recovers {
                (0..4).map(|_| Ok(resp(status))).collect()
            } else {
                vec![Ok(resp(status))]
            };
            if recovers {
                script.push(Ok(resp(200)));
            }
            let (http, mock) = shared(script);
            let result = http
                .send_raw(
                    &TransportRequest::get("https://example.test/x"),
                    &test_policy(),
                )
                .await;
            if recovers {
                assert_eq!(
                    result.expect("retryable status must recover").status,
                    200,
                    "status {status} must recover on the last retry"
                );
            } else {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    format!("HTTP {status}"),
                    "status {status} must stay terminal with the classify_http text"
                );
            }
            let expected_sends = if recovers { 5 } else { 1 };
            assert_eq!(
                mock.recorded().len(),
                expected_sends,
                "status {status} must produce exactly {expected_sends} sends"
            );
        }
    }

    #[tokio::test]
    async fn retry_matrix_records_exact_attempt_counts() {
        let mut script: Vec<Result<TransportResponse, TransportError>> =
            (0..4).map(|_| Ok(resp(503))).collect();
        script.push(Ok(resp(200)));
        let (http, mock) = shared(script);
        http.send_raw(
            &TransportRequest::get("https://example.test/x"),
            &test_policy(),
        )
        .await
        .expect("fifth attempt must succeed");
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 5);
        for request in &recorded {
            assert_eq!(request.method, HttpMethod::Get);
            assert!(request.idempotent);
        }
    }

    #[tokio::test]
    async fn transport_error_retries_then_succeeds() {
        let (http, mock) = shared([Err(transport_err("conn reset")), Ok(resp(200))]);
        let response = http
            .send_raw(
                &TransportRequest::get("https://example.test/x"),
                &test_policy(),
            )
            .await
            .expect("one transport error must be retried");
        assert_eq!(response.status, 200);
        assert_eq!(mock.recorded().len(), 2);
    }

    #[tokio::test]
    async fn non_idempotent_post_stops_after_first_failure() {
        let (http, mock) = shared([Ok(resp(503)), Ok(resp(200))]);
        let request = TransportRequest::post("https://example.test/post");
        assert!(!request.idempotent);
        let error = http
            .send_raw(&request, &test_policy())
            .await
            .expect_err("503 must not be retried on a non-idempotent POST");
        assert_eq!(error.to_string(), "HTTP 503");
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn post_marked_idempotent_retries_like_eapi_and_musicu() {
        let mut script: Vec<Result<TransportResponse, TransportError>> =
            (0..4).map(|_| Err(transport_err("conn reset"))).collect();
        script.push(Ok(json_resp(200, r#"{"code":200}"#)));
        let (http, mock) = shared(script);
        let request = TransportRequest::post("https://example.test/eapi")
            .header("content-type", "application/x-www-form-urlencoded")
            .body("params=ENCRYPTED")
            .idempotent(true);
        let value = http
            .send_api_json(
                &request,
                &test_policy(),
                &netease_like_codes(),
                netease_like_code_of,
            )
            .await
            .expect("idempotent POST must retry through transport errors");
        assert_eq!(value["code"], 200);
        assert_eq!(mock.recorded().len(), 5);
        assert!(mock.recorded()[0].idempotent);
    }

    #[tokio::test]
    async fn api_throttled_code_retries_then_succeeds() {
        let (http, mock) = shared([
            Ok(json_resp(200, r#"{"code":405,"message":"throttled"}"#)),
            Ok(json_resp(200, r#"{"code":200,"songs":[]}"#)),
        ]);
        let value = http
            .send_api_json(
                &TransportRequest::get("https://example.test/api"),
                &test_policy(),
                &netease_like_codes(),
                netease_like_code_of,
            )
            .await
            .expect("throttled API code must retry");
        assert_eq!(value["songs"].as_array().map(Vec::len), Some(0));
        assert_eq!(mock.recorded().len(), 2);
    }

    #[tokio::test]
    async fn api_session_expired_is_terminal_without_retry() {
        let (http, mock) = shared([Ok(json_resp(200, r#"{"code":301,"message":"need login"}"#))]);
        let error = http
            .send_api_json(
                &TransportRequest::get("https://example.test/api"),
                &test_policy(),
                &netease_like_codes(),
                netease_like_code_of,
            )
            .await
            .expect_err("session expiry must not retry");
        assert!(matches!(error, TransportError::SessionExpired));
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn api_fatal_code_is_terminal_with_exact_message() {
        let (http, _) = shared([Ok(json_resp(200, r#"{"code":881,"message":"参数异常"}"#))]);
        let error = http
            .send_api_json(
                &TransportRequest::get("https://example.test/api"),
                &test_policy(),
                &netease_like_codes(),
                netease_like_code_of,
            )
            .await
            .expect_err("fatal API code must not retry");
        assert_eq!(error.to_string(), "API code 881: 参数异常");
    }

    #[tokio::test]
    async fn api_retries_zero_keeps_kugou_kuwo_body_semantics() {
        let codes = ApiCodes {
            ok: 0,
            expired: &[],
            throttled: Some(405),
        };
        let policy = RetryPolicy {
            api_retries: 0,
            ..test_policy()
        };
        let (http, mock) = shared([
            Ok(json_resp(200, r#"{"code":405,"message":"throttled"}"#)),
            Ok(json_resp(200, r#"{"code":0}"#)),
        ]);
        let code_of = |value: &serde_json::Value| {
            (
                value["code"].as_i64().unwrap_or(0),
                value["message"].as_str().unwrap_or("").to_owned(),
            )
        };
        let error = http
            .send_api_json(
                &TransportRequest::get("https://example.test/api"),
                &policy,
                &codes,
                code_of,
            )
            .await
            .expect_err("api_retries: 0 must keep body codes terminal");
        assert_eq!(error.to_string(), "API code 405: throttled");
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn malformed_json_body_is_terminal() {
        let (http, mock) = shared([Ok(json_resp(200, "not json"))]);
        let error = http
            .send_api_json(
                &TransportRequest::get("https://example.test/api"),
                &test_policy(),
                &netease_like_codes(),
                netease_like_code_of,
            )
            .await
            .expect_err("body decode failure must not retry");
        assert!(matches!(error, TransportError::Body(_)));
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn retry_after_header_is_respected_and_capped() {
        let policy = RetryPolicy {
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_millis(2_000),
            jitter: Duration::ZERO,
            retry_after_cap: Duration::from_millis(100),
            ..test_policy()
        };
        // 30s Retry-After clamps to 100ms and dominates the 50ms exponent.
        assert_eq!(
            next_delay(&policy, 0, Some(Duration::from_secs(30))),
            Duration::from_millis(100)
        );
        // A small Retry-After loses against the exponent.
        assert_eq!(
            next_delay(&policy, 3, Some(Duration::from_millis(10))),
            Duration::from_millis(400)
        );
        assert_eq!(next_delay(&policy, 0, None), Duration::from_millis(50));
    }

    #[test]
    fn exponential_backoff_grows_and_caps() {
        let policy = RetryPolicy {
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_millis(2_000),
            jitter: Duration::ZERO,
            ..test_policy()
        };
        assert_eq!(next_delay(&policy, 0, None), Duration::from_millis(50));
        assert_eq!(next_delay(&policy, 1, None), Duration::from_millis(100));
        assert_eq!(next_delay(&policy, 2, None), Duration::from_millis(200));
        assert_eq!(next_delay(&policy, 3, None), Duration::from_millis(400));
        assert_eq!(next_delay(&policy, 4, None), Duration::from_millis(800));
        assert_eq!(next_delay(&policy, 5, None), Duration::from_millis(1_600));
        // attempt 6: 50 * 64 = 3200ms, capped at max_delay.
        assert_eq!(next_delay(&policy, 6, None), Duration::from_millis(2_000));
        assert_eq!(next_delay(&policy, 99, None), Duration::from_millis(2_000));
    }

    #[test]
    fn jitter_stays_bounded() {
        let policy = RetryPolicy {
            base_delay: Duration::from_millis(50),
            max_delay: Duration::from_millis(8_000),
            jitter: Duration::from_millis(10),
            ..test_policy()
        };
        for attempt in 0..12 {
            let delay = next_delay(&policy, attempt, None).as_millis();
            let base = (50_u64 * (1_u64 << attempt.min(6))).min(8_000) as u128;
            assert!(
                delay >= base && delay <= base + 10,
                "delay {delay} outside [{base}, {}]",
                base + 10
            );
        }
        // Zero jitter must yield exactly the base delay.
        let no_jitter = RetryPolicy {
            jitter: Duration::ZERO,
            ..test_policy()
        };
        assert_eq!(next_delay(&no_jitter, 0, None), Duration::from_millis(1));
    }

    #[tokio::test]
    async fn mock_script_exhaustion_fails_loudly_without_budget_burn() {
        // Idempotent GET: the 503 leg is retried once, the retry hits the dry
        // script, and the Build sentinel must short-circuit instead of burning
        // the remaining retry budget (old behavior inflated recorded() to 5).
        let (http, mock) = shared([Ok(resp(503))]);
        let request = TransportRequest::get("https://example.test/x");
        let error = http
            .send_raw(&request, &test_policy())
            .await
            .expect_err("dry script must error");
        assert!(matches!(error, TransportError::Build(_)), "got: {error}");
        assert_eq!(
            mock.recorded().len(),
            2,
            "503 send + exhausted retry, no further attempts"
        );
        // A later send still fails loudly and stays a single attempt.
        let second = http
            .send_raw(&request, &test_policy())
            .await
            .expect_err("still dry");
        assert!(matches!(second, TransportError::Build(_)));
        assert_eq!(mock.recorded().len(), 3);
    }

    #[tokio::test]
    async fn build_errors_are_never_retried() {
        // Deterministic Build failures (invalid URL/header, request never
        // sent) must not consume the retry budget even on idempotent GETs.
        let mock =
            MockTransport::new(|_| Err(TransportError::Build("invalid header value".to_owned())));
        let http = Http::from_mock(mock);
        let error = http
            .send_raw(
                &TransportRequest::get("https://example.test/x"),
                &test_policy(),
            )
            .await
            .expect_err("Build must surface immediately");
        assert!(matches!(error, TransportError::Build(_)));
    }

    #[tokio::test]
    async fn send_once_passes_3xx_through_untouched() {
        // The migu listen face succeeds on a 302 by reading Location;
        // send_raw would collapse it into Err(Status("HTTP 302")) and drop
        // the response. send_once must hand the redirect response back.
        let (http, mock) = shared([Ok(TransportResponse {
            status: 302,
            headers: vec![(
                "location".to_owned(),
                "https://cdn.example.test/listen/track.mp3".to_owned(),
            )],
            url: Some("https://listen.example.test/song.do".to_owned()),
            body: Vec::new(),
        })]);
        let request = TransportRequest::get("https://listen.example.test/song.do")
            .header("user-agent", "UA_IPHONE");
        let response = http
            .send_once(&request)
            .await
            .expect("302 is a normal send_once result");
        assert_eq!(response.status, 302);
        assert_eq!(
            response.header("location"),
            Some("https://cdn.example.test/listen/track.mp3")
        );
        assert_eq!(mock.recorded().len(), 1);
        // Same request through send_raw is terminal with the classify_http
        // text, documenting the contrast the design relies on.
        let (http_raw, _) = shared([Ok(resp(302))]);
        let error = http_raw
            .send_raw(&request, &test_policy())
            .await
            .expect_err("send_raw must not hand back a 302");
        assert_eq!(error.to_string(), "HTTP 302");
    }

    #[tokio::test]
    async fn body_retry_verdict_gated_on_non_idempotent_post() {
        // netease 405 over a POST that forgot .idempotent(true): the engine
        // must refuse the silent body resend instead of retrying it.
        let (http, mock) = shared([
            Ok(json_resp(200, r#"{"code":405,"message":"throttled"}"#)),
            Ok(json_resp(200, r#"{"code":200,"songs":[]}"#)),
        ]);
        let request = TransportRequest::post("https://example.test/eapi");
        assert!(!request.idempotent);
        let error = http
            .send_api_json(
                &request,
                &test_policy(),
                &netease_like_codes(),
                netease_like_code_of,
            )
            .await
            .expect_err("body retry must be gated on idempotency");
        match error {
            TransportError::Api(message) => {
                assert!(message.contains("non-idempotent"), "got: {message}");
            }
            other => panic!("expected TransportError::Api, got {other:?}"),
        }
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn profile_isolation_keeps_transports_independent() {
        let mock_a = Arc::new(MockTransport::single(resp(200)));
        let mock_b = Arc::new(MockTransport::single(resp(200)));
        let http_a = Http::from_shared_mock(mock_a.clone());
        let _http_b = Http::from_shared_mock(mock_b.clone()); // handle exists; its mock must stay empty
        let _ = http_a
            .send_raw(&TransportRequest::get("https://a.test/"), &test_policy())
            .await
            .expect("transport A must serve its own mock");
        assert_eq!(mock_a.recorded().len(), 1);
        assert_eq!(
            mock_b.recorded().len(),
            0,
            "transport B must not see A's requests"
        );
        // Cloning the handle shares the same transport, it does not fork it.
        let http_a2 = http_a.clone();
        let _ = http_a2
            .send_raw(&TransportRequest::get("https://a.test/2"), &test_policy())
            .await
            .expect("cloned handle must reach the same transport");
        assert_eq!(mock_a.recorded().len(), 2);
        assert_eq!(mock_b.recorded().len(), 0);
    }

    #[test]
    fn client_profile_defaults_and_overrides() {
        let default = ClientProfile::provider_default();
        assert_eq!(default.timeout, Some(Duration::from_secs(15)));
        assert!(!default.redirect_none);
        assert!(default.proxy.is_none());
        assert!(default.connect_timeout.is_none());
        let custom = ClientProfile {
            redirect_none: true,
            connect_timeout: Some(Duration::from_secs(8)),
            pool_max_idle_per_host: Some(16),
            tcp_keepalive: Some(Duration::from_secs(30)),
            proxy: Some("http://127.0.0.1:7890".into()),
            ..ClientProfile::provider_default()
        };
        assert!(custom.redirect_none);
        assert_eq!(custom.connect_timeout, Some(Duration::from_secs(8)));
        assert_eq!(custom.timeout, Some(Duration::from_secs(15)));
    }

    #[test]
    fn adapter_applies_headers_body_and_timeout_override() {
        let transport = ReqwestTransport::from_profile(&ClientProfile::provider_default());
        let request = TransportRequest::post("https://example.test/eapi")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", "MUSIC_U=secret")
            .header("set-cookie", "first")
            .header("set-cookie", "second")
            .body("params=ENCRYPTED")
            .timeout(Duration::from_secs(35));
        let req = transport.build_request(&request).expect("must build");
        assert_eq!(req.method(), reqwest::Method::POST);
        assert_eq!(req.url().as_str(), "https://example.test/eapi");
        assert_eq!(
            req.timeout(),
            Some(&Duration::from_secs(35)),
            "per-request timeout (WX_POLL_TIMEOUT) must override the profile"
        );
        assert_eq!(
            req.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(
            req.headers().get("cookie").and_then(|v| v.to_str().ok()),
            Some("MUSIC_U=secret")
        );
        let cookies: Vec<_> = req.headers().get_all("set-cookie").iter().collect();
        assert_eq!(cookies.len(), 2, "duplicate headers must be preserved");
        assert_eq!(
            req.body().and_then(reqwest::Body::as_bytes),
            Some(&b"params=ENCRYPTED"[..])
        );
    }

    #[test]
    fn adapter_builds_clients_for_both_redirect_profiles() {
        let follow = ReqwestTransport::from_profile(&ClientProfile::provider_default());
        let none = ReqwestTransport::from_profile(&ClientProfile {
            redirect_none: true,
            ..ClientProfile::provider_default()
        });
        // Redirect policies are not introspectable on a built client; assert
        // both profiles build and accept a request (hop semantics are asserted
        // by the qq/migu slices).
        for transport in [&follow, &none] {
            let req = transport
                .build_request(&TransportRequest::get("https://example.test/"))
                .expect("profile-built transport must accept requests");
            assert_eq!(req.url().as_str(), "https://example.test/");
        }
    }

    #[test]
    fn adapter_rejects_invalid_urls_without_panicking() {
        let transport = ReqwestTransport::from_profile(&ClientProfile::provider_default());
        let error = transport
            .build_request(&TransportRequest::get("::::not-a-url::::"))
            .expect_err("invalid URL must surface as Build");
        assert!(matches!(error, TransportError::Build(_)));
    }

    #[test]
    fn transport_error_maps_into_provider_error() {
        let error = TransportError::Status("HTTP 404".to_owned())
            .into_provider_error("netease", "song details");
        match error {
            crate::Error::Provider { provider, message } => {
                assert_eq!(provider, "netease");
                assert_eq!(message, "song details: HTTP 404");
            }
            other => panic!("expected Error::Provider, got {other:?}"),
        }
        let error = TransportError::SessionExpired.into_provider_error("qqmusic", "poll");
        match error {
            crate::Error::Provider { message, .. } => {
                assert_eq!(message, "poll: login session expired");
            }
            other => panic!("expected Error::Provider, got {other:?}"),
        }
    }

    #[test]
    fn retry_after_parsing_accepts_integer_seconds_only() {
        let response = TransportResponse {
            status: 429,
            headers: vec![("retry-after".to_owned(), "3".to_owned())],
            url: None,
            body: Vec::new(),
        };
        assert_eq!(response.retry_after(), Some(Duration::from_secs(3)));
        let date = TransportResponse {
            status: 429,
            headers: vec![(
                "retry-after".to_owned(),
                "Wed, 21 Oct 2026 07:28:00 GMT".to_owned(),
            )],
            url: None,
            body: Vec::new(),
        };
        assert_eq!(date.retry_after(), None);
        assert_eq!(resp(200).retry_after(), None);
    }

    #[test]
    fn content_length_parses_the_header_for_head_probes() {
        let response = TransportResponse {
            status: 200,
            headers: vec![("content-length".to_owned(), "1048576".to_owned())],
            url: None,
            body: Vec::new(),
        };
        assert_eq!(response.content_length(), Some(1_048_576));
        let missing = resp(200);
        assert_eq!(missing.content_length(), None);
        let junk = TransportResponse {
            status: 200,
            headers: vec![("content-length".to_owned(), "not-a-number".to_owned())],
            url: None,
            body: Vec::new(),
        };
        assert_eq!(junk.content_length(), None);
    }

    #[test]
    fn non_utf8_header_values_are_treated_as_absent() {
        // The adapter stores lossily; today's providers drop such values via
        // `HeaderValue::to_str().ok()` (migu Location, qq Set-Cookie). The
        // accessors must do the same, including skipping a mangled duplicate
        // in favor of a later usable one.
        let response = TransportResponse {
            status: 302,
            headers: vec![
                ("location".to_owned(), format!("{LOSSY}mangled-cdn-path")),
                (
                    "location".to_owned(),
                    "https://cdn.example.test/ok".to_owned(),
                ),
                ("set-cookie".to_owned(), format!("bad={LOSSY}; Path=/")),
                ("set-cookie".to_owned(), "good=1; Path=/".to_owned()),
            ],
            url: None,
            body: Vec::new(),
        };
        assert_eq!(
            response.header("location"),
            Some("https://cdn.example.test/ok")
        );
        assert_eq!(
            response.all("location"),
            vec!["https://cdn.example.test/ok"]
        );
        assert_eq!(response.all("set-cookie"), vec!["good=1; Path=/"]);
        // The raw (mangled) view stays available for debugging.
        assert_eq!(response.headers.len(), 4);
        assert_eq!(response.headers[0].1, format!("{LOSSY}mangled-cdn-path"));
        // All-mangled: accessor reads as absent, like today's NotFound path.
        let only_mangled = TransportResponse {
            status: 302,
            headers: vec![("location".to_owned(), format!("{LOSSY}x"))],
            url: None,
            body: Vec::new(),
        };
        assert_eq!(only_mangled.header("location"), None);
    }

    #[test]
    fn method_defaults_gate_idempotency() {
        assert!(TransportRequest::get("https://x").idempotent);
        assert!(TransportRequest::head("https://x").idempotent);
        assert!(!TransportRequest::post("https://x").idempotent);
    }
}
