//! Download engine core (optional `download` feature).
//!
//! Behavior port of the VoiceFox downloader engine (`voicefox
//! app/src/download/engine.rs`, the slice's declared source of truth), per
//! tasks.md item 7: multipart/single fetch with Range probing, strict bounded
//! chunking for Range-hostile CDNs, candidate CDN fallback ordering with the
//! NetEase node rewrite, exponential backoff, size/MD5 integrity gates, and
//! the post-download tag/lyric finish (tags.rs plus the `write_metadata`
//! implementation from `voicefox source/src/local/metadata.rs`).
//!
//! Scope boundary (2026-11-18, see docs/IMPLEMENTATION_STATUS.md): this is the
//! library-level engine core only. Download task orchestration, the task
//! records database, the task UI, WebDAV upload and the filename template
//! stay in the VoiceFox host application, which consumes this module through
//! the adapter like any other musid-dli capability. `DownloadOptions::from_config`
//! is therefore not ported: the user-facing `DownloadConfig` is host-side; the
//! host clamps its own config into `DownloadOptions` fields.
//!
//! Honesty note on resume: the upstream engine has no byte-level resume either.
//! The `.part` staging file is rewritten from zero on every retry (progress is
//! rolled back atomically), and the finished file appears via an atomic rename.
//! This port keeps exactly that behavior; nothing here claims resume support.
//!
//! Substitutions recorded (upstream parity gaps, all intentional):
//! * `shrink_cover` is not ported: it needs the `image` crate to decode and
//!   Lanczos-resize oversized covers. Oversized covers are embedded as-is;
//!   a container that refuses them degrades to the no-cover rewrite path.
//! * `cover_is_usable` sniffs magic bytes instead of fully decoding with the
//!   `image` crate. A corrupt-but-magic-matching cover fails inside
//!   `Picture::from_reader` at tag-write time and hits the same
//!   degrade-without-cover path, so the final behavior matches.
//! * `standard_lyric` works over musid-dli's own `LyricDocument` instead of
//!   VoiceFox `LyricData` (`lyric`/`raw_lrc`/`lxlyric`): the raw text is
//!   trusted only when it actually carries bracketed `mm:ss.xx` timestamps
//!   with content, otherwise the parsed lines (word timing collapsed to line
//!   text) are rendered to LRC. Private/JSON payloads are never written as
//!   `.lrc`.
//! * the audio magic sniffer replaces `naming::detect_extension` (naming.rs
//!   stays host-side); the duration-tolerance verdict of `validate_audio` is
//!   ported unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, ErrorCategory, ErrorDetail};
use crate::models::StreamInfo;
use md5::{Digest, Md5};
use reqwest::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, HeaderMap, HeaderName, HeaderValue, RANGE,
};
use tokio::io::AsyncWriteExt;

pub const USER_AGENT: &str = "musid-dli/0.1";

/// Download request. `dest` is the final file name; the staging file is
/// written next to it and atomically renamed on success.
#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub url: String,
    /// Extra request headers the source requires (Referer / User-Agent / cookies).
    pub headers: Vec<(String, String)>,
    pub dest: PathBuf,
    /// Source-declared size, used for the integrity check.
    pub expected_size: Option<u64>,
    /// The declared size is unreliable (some sources under-report by a few
    /// bytes): only reject files shorter than the declared value.
    pub size_is_advisory: bool,
    /// Source-declared MD5, verified after download. Empty string skips the check.
    pub md5: Option<String>,
    /// Backup CDN URLs injected by the source, tried in order after the
    /// primary address fails.
    pub candidate_urls: Vec<String>,
    /// Strict bounded-Range chunking: when the source declares a per-request
    /// cap (bytes, 0 = disabled), every request must stay a bounded Range and
    /// no probe, plain GET or single-connection fallback is attempted
    /// (googlevideo-style CDNs answer all of those with 403).
    pub max_chunk_size: u64,
}

/// StreamInfo to DownloadRequest adapter (MODEL_ALIGNMENT_DESIGN.md §2.5):
/// the single mapping the inventory's G2 asks for, so the
/// content_length/checksum/fallback/header translation cannot drift per
/// call site. `dest` stays caller-owned: a StreamInfo is a resolution
/// result with no filesystem target, so there is deliberately no
/// `impl From<StreamInfo> for DownloadRequest`.
///
/// Known lossy steps, pinned by tests and §2.5:
/// * per-endpoint `headers`/`content_length`/`expires_at_unix`/`quality` on
///   `fallback_endpoints` are not representable in a `DownloadRequest` (the
///   engine applies one metadata set to every candidate, `download` loop);
///   the endpoint URLs still reach the candidate list via
///   [`StreamInfo::ordered_fallback_urls`].
/// * a declared non-MD5 `checksum_algorithm` drops the checksum instead of
///   reinterpreting it as MD5 (`verify_md5_sync` verifies MD5 only).
pub fn download_request_from_stream(stream: &StreamInfo, dest: PathBuf) -> DownloadRequest {
    DownloadRequest {
        url: stream.url.clone(),
        headers: stream.headers.clone(),
        dest,
        expected_size: stream.content_length,
        size_is_advisory: stream.size_is_advisory,
        md5: stream.md5_checksum().map(str::to_owned),
        candidate_urls: stream.ordered_fallback_urls(),
        max_chunk_size: stream.max_chunk_size.unwrap_or(0),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOptions {
    /// Piece concurrency for one file.
    pub concurrency: usize,
    /// Whether multipart download is enabled.
    pub multipart: bool,
    /// Files smaller than this stay single-connection.
    pub multipart_min_size: u64,
    /// Max retries for retryable (network/5xx) failures.
    pub max_retries: u32,
    /// Whether the byte count is verified against the declared size.
    pub verify_size: bool,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            concurrency: 4,
            multipart: true,
            multipart_min_size: 5 * 1024 * 1024,
            max_retries: 3,
            verify_size: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("download cancelled")]
    Cancelled,
    #[error("source does not support Range chunking")]
    RangesUnsupported,
    #[error("network error: {0}")]
    Network(String),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("file write failed: {0}")]
    Io(String),
    #[error("HTTP {status}")]
    Http { status: u16 },
    #[error("{0}")]
    Other(String),
}

impl DownloadError {
    /// Network errors and 5xx are retryable; integrity and cancellation are not.
    pub fn is_retryable(&self) -> bool {
        match self {
            DownloadError::Network(_) | DownloadError::Io(_) => true,
            // "Ranges unsupported" means "switch strategy": the caller falls
            // back to the single-connection path.
            DownloadError::RangesUnsupported => true,
            DownloadError::Http { status } => *status >= 500 || *status == 408 || *status == 429,
            DownloadError::Cancelled | DownloadError::Integrity(_) | DownloadError::Other(_) => {
                false
            }
        }
    }
}

/// The statuses [`DownloadError::is_retryable`] already treats as transient
/// (`download.rs:132`): 5xx joins 408. Shared by the category bridge so the
/// two faces cannot drift apart.
fn is_unavailable_status(status: u16) -> bool {
    status >= 500 || status == 408
}

impl DownloadError {
    /// Map this engine error onto the M2 taxonomy ([`ErrorCategory`];
    /// AUTH_REGISTRY_DESIGN.md §1.1, §8 item 8). Pure function, no state.
    ///
    /// Pinned rows (ask-mandated): `Network` → `Network`, `Http{429}` →
    /// `RateLimited`, `Http{5xx/408}` → `Unavailable`, `Cancelled` →
    /// `Cancelled`, `Integrity` → `ProviderInternal`, `Io` → `Io`,
    /// `RangesUnsupported` → `UnsupportedOperation`. The two residual arms
    /// are a declared decision, not an oversight: `Http` statuses outside
    /// the pinned set (403/404/…) and the `Other` catch-all stay in
    /// `ErrorCategory::Provider`, the taxonomy's unclassified catch-all,
    /// rather than being force-fitted to a semantic category the ask did
    /// not pin. Hosts needing finer HTTP classification can match the
    /// public `DownloadError` directly.
    pub fn category(&self) -> ErrorCategory {
        match self {
            DownloadError::Network(_) => ErrorCategory::Network,
            DownloadError::Http { status } => {
                if *status == 429 {
                    ErrorCategory::RateLimited
                } else if is_unavailable_status(*status) {
                    ErrorCategory::Unavailable
                } else {
                    ErrorCategory::Provider
                }
            }
            DownloadError::Cancelled => ErrorCategory::Cancelled,
            DownloadError::Integrity(_) => ErrorCategory::ProviderInternal,
            DownloadError::Io(_) => ErrorCategory::Io,
            DownloadError::RangesUnsupported => ErrorCategory::UnsupportedOperation,
            DownloadError::Other(_) => ErrorCategory::Provider,
        }
    }

    /// Bridge into the crate's unified [`Error`] with the registered
    /// provider id (AUTH_REGISTRY_DESIGN.md §1.2 / §8 item 8). The engine
    /// type stays engine-side (no `From<DownloadError>` impl): hosts call
    /// this explicitly at the boundary, exactly like
    /// `TransportError::into_provider_error` on the M1 face.
    ///
    /// Text preservation (§1.2 "不得静默丢弃"): every engine payload reaches
    /// the M2 face — through the `detail` slot (`Network`, `Integrity`,
    /// `RangesUnsupported`, the classified `Http` statuses) or through the
    /// `Io`/`Provider` payloads (`Io`, the residual `Http` statuses and
    /// `Other`). `retry_after` stays `None`: `DownloadError::Http` carries
    /// no server-sent duration. `Cancelled` bridges to the payload-less
    /// `Error::Cancelled`, so `provider_id()` is `None` on that row only.
    /// The engine produces error strings only (never headers or cookies),
    /// matching the `ErrorDetail` constructor-contract exposure class.
    pub fn to_error(&self, provider: &str) -> Error {
        let provider_id = Some(provider.to_owned());
        match self {
            DownloadError::Network(text) => Error::Network {
                provider: provider_id,
                detail: Some(ErrorDetail::new(text.clone())),
            },
            DownloadError::Http { status: 429 } => Error::RateLimited {
                provider: provider_id,
                retry_after: None,
                detail: Some(ErrorDetail::new("HTTP 429")),
            },
            DownloadError::Http { status } if is_unavailable_status(*status) => {
                Error::Unavailable {
                    provider: provider_id,
                    detail: Some(ErrorDetail::new(format!("HTTP {status}"))),
                }
            }
            DownloadError::Http { status } => Error::Provider {
                provider: provider.to_owned(),
                message: format!("download: HTTP {status}"),
            },
            DownloadError::Cancelled => Error::Cancelled,
            DownloadError::Integrity(text) => Error::ProviderInternal {
                provider: provider_id,
                detail: Some(ErrorDetail::new(text.clone())),
            },
            DownloadError::Io(text) => Error::Io(std::io::Error::other(text.clone())),
            DownloadError::RangesUnsupported => Error::UnsupportedOperation {
                provider: provider_id,
                detail: Some(ErrorDetail::new("source does not support Range chunking")),
            },
            DownloadError::Other(text) => Error::Provider {
                provider: provider.to_owned(),
                message: format!("download: {text}"),
            },
        }
    }
}

/// Download progress. Callers poll a snapshot; the download task never blocks.
#[derive(Debug, Default)]
pub struct DownloadProgress {
    downloaded: AtomicU64,
    total: AtomicU64,
    cancel: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub downloaded: u64,
    pub total: u64,
    pub cancelled: bool,
}

impl ProgressSnapshot {
    /// Progress ratio; `None` while the total size is unknown.
    pub fn ratio(&self) -> Option<f64> {
        if self.total == 0 {
            return None;
        }
        Some((self.downloaded as f64 / self.total as f64).clamp(0.0, 1.0))
    }
}

impl DownloadProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn add(&self, bytes: u64) {
        if bytes > 0 {
            self.downloaded.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    fn sub(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        // Roll back bytes counted by a failed piece so progress stays honest.
        let mut current = self.downloaded.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(bytes);
            match self.downloaded.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    /// Zero the counted bytes before switching candidate addresses.
    fn reset(&self) {
        self.downloaded.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            downloaded: self.downloaded.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            cancelled: self.cancel.load(Ordering::Relaxed),
        }
    }

    /// Request cancellation; the download loop exits at the next checkpoint.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn check_cancelled(&self) -> Result<(), DownloadError> {
        if self.is_cancelled() {
            Err(DownloadError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SourceProbe {
    /// Total file size, `None` when unknown.
    total: Option<u64>,
    /// Whether the source supports Range chunking.
    ranges: bool,
}

pub struct DownloadEngine {
    client: reqwest::Client,
    /// Caps the "waiting for response headers" and per-chunk stall windows of
    /// control requests and streaming downloads. Deliberately not a
    /// client-level total timeout: that would cut off large slow downloads
    /// mid-body.
    request_timeout: Duration,
    options: DownloadOptions,
}

impl DownloadEngine {
    pub fn new(proxy_url: &str, timeout_secs: u64, options: DownloadOptions) -> Self {
        let request_timeout = Duration::from_secs(timeout_secs.clamp(1, 600));
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(timeout_secs.clamp(1, 30)))
            .pool_idle_timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(16)
            .tcp_keepalive(Duration::from_secs(30))
            .user_agent(USER_AGENT);
        if !proxy_url.trim().is_empty()
            && let Ok(proxy) = reqwest::Proxy::all(proxy_url.trim())
        {
            builder = builder.proxy(proxy);
        }
        Self {
            client: builder.build().unwrap_or_default(),
            request_timeout,
            options,
        }
    }

    /// Download a small file (covers and similar) into memory with a size cap.
    pub async fn fetch_bytes(
        &self,
        url: &str,
        headers: &[(String, String)],
        limit: u64,
    ) -> Result<Vec<u8>, DownloadError> {
        let mut response = apply_headers(self.client.get(url), headers)
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|error| DownloadError::Network(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(DownloadError::Http {
                status: status.as_u16(),
            });
        }
        if let Some(length) = response.content_length()
            && length > limit
        {
            return Err(DownloadError::Other(format!(
                "response size {length} exceeds the limit {limit}"
            )));
        }
        let mut buffer = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| DownloadError::Network(error.to_string()))?
        {
            buffer.extend_from_slice(&chunk);
            if buffer.len() as u64 > limit {
                return Err(DownloadError::Other(format!(
                    "response size exceeds the limit {limit}"
                )));
            }
        }
        Ok(buffer)
    }

    /// Download to `req.dest`. The staging file is written next to the
    /// destination and atomically renamed on success, so an interrupted run
    /// never leaves a half file posing as the result.
    pub async fn download(
        &self,
        req: &DownloadRequest,
        progress: &Arc<DownloadProgress>,
    ) -> Result<u64, DownloadError> {
        if req.url.trim().is_empty() {
            return Err(DownloadError::Other("missing download url".to_string()));
        }
        if let Some(parent) = req.dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| DownloadError::Io(error.to_string()))?;
        }

        let part_path = staging_path(&req.dest);
        let candidates = candidate_download_urls(&req.url, &req.candidate_urls);
        let mut last_error = DownloadError::Other("no usable download url".to_string());
        for (index, url) in candidates.iter().enumerate() {
            if index > 0 {
                tracing::debug!(
                    "download retrying with candidate url: {} (for {})",
                    url,
                    req.dest.display()
                );
                progress.reset();
            }

            // Strict-chunk sources: no probe, no downgrade, every request is a
            // bounded Range.
            let outcome = if req.max_chunk_size > 0 {
                match req.expected_size.filter(|&total| total > 0) {
                    Some(total) => {
                        progress.set_total(total);
                        self.download_chunked(req, url, total, &part_path, progress)
                            .await
                    }
                    None => Err(DownloadError::Other(
                        "strict chunked download needs the source-declared size".to_string(),
                    )),
                }
            } else {
                let probe = self.probe(url, &req.headers).await;
                let total = probe.total.or(req.expected_size);
                progress.set_total(total.unwrap_or(0));
                self.download_to(req, url, probe, total, &part_path, progress)
                    .await
            };

            match outcome {
                Ok(written) => {
                    if self.options.verify_size
                        && let Some(expected) = req.expected_size
                        && let Err(error) = verify_size(expected, written, req.size_is_advisory)
                    {
                        let _ = tokio::fs::remove_file(&part_path).await;
                        last_error = error;
                        continue;
                    }
                    if let Some(expected_md5) = &req.md5
                        && let Err(error) = verify_md5(&part_path, expected_md5).await
                    {
                        let _ = tokio::fs::remove_file(&part_path).await;
                        last_error = error;
                        continue;
                    }
                    tokio::fs::rename(&part_path, &req.dest)
                        .await
                        .map_err(|error| {
                            let _ = std::fs::remove_file(&part_path);
                            DownloadError::Io(error.to_string())
                        })?;
                    return Ok(written);
                }
                Err(error) => {
                    let _ = tokio::fs::remove_file(&part_path).await;
                    last_error = error;
                }
            }
        }
        Err(last_error)
    }

    async fn download_to(
        &self,
        req: &DownloadRequest,
        url: &str,
        probe: SourceProbe,
        total: Option<u64>,
        dest: &Path,
        progress: &Arc<DownloadProgress>,
    ) -> Result<u64, DownloadError> {
        let multipart_ready = self.options.multipart
            && probe.ranges
            && total.is_some_and(|total| total >= self.options.multipart_min_size);
        if multipart_ready {
            match self
                .download_multipart(req, url, total.unwrap_or_default(), dest, progress)
                .await
            {
                Ok(written) => return Ok(written),
                Err(error) if error.is_retryable() => {
                    tracing::debug!(
                        "multipart download failed for {}, falling back to single connection: {}",
                        req.dest.display(),
                        error
                    );
                    let _ = tokio::fs::remove_file(dest).await;
                }
                Err(error) => return Err(error),
            }
        }
        self.download_single(req, url, dest, progress).await
    }

    /// Probe the source: HEAD first, then a `Range: bytes=0-0` GET as fallback.
    async fn probe(&self, url: &str, headers: &[(String, String)]) -> SourceProbe {
        if let Ok(response) = apply_headers(self.client.head(url), headers)
            .timeout(self.request_timeout)
            .send()
            .await
            && response.status().is_success()
        {
            let headers = response.headers().clone();
            let total = content_length(&headers).or_else(|| response.content_length());
            if total.is_some() {
                return SourceProbe {
                    total,
                    ranges: supports_ranges(&headers),
                };
            }
        }

        // When HEAD yields no length, a GET with `Range: bytes=0-0` returns a
        // 1-byte body while Content-Range confirms both total length and
        // Range support.
        match apply_headers(self.client.get(url), headers)
            .header(RANGE, "bytes=0-0")
            .timeout(self.request_timeout)
            .send()
            .await
        {
            Ok(response) if response.status().as_u16() == 206 => {
                let total = response
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_content_range_total);
                SourceProbe {
                    total,
                    ranges: true,
                }
            }
            Ok(response) if response.status().is_success() => SourceProbe {
                total: response.content_length(),
                ranges: false,
            },
            _ => SourceProbe::default(),
        }
    }

    async fn download_single(
        &self,
        req: &DownloadRequest,
        url: &str,
        dest: &Path,
        progress: &Arc<DownloadProgress>,
    ) -> Result<u64, DownloadError> {
        let attempts = self.options.max_retries.max(1);
        let mut last_error = DownloadError::Other("download failed".to_string());
        for attempt in 0..attempts {
            if attempt > 0 {
                sleep_before_retry(attempt).await;
            }
            progress.check_cancelled()?;
            match self.single_attempt(req, url, dest, progress).await {
                Ok(written) => return Ok(written),
                Err(error) if error.is_retryable() && attempt + 1 < attempts => {
                    tracing::debug!(
                        "download attempt {} for {} failed: {}",
                        attempt + 1,
                        req.dest.display(),
                        error
                    );
                    last_error = error;
                    let _ = tokio::fs::remove_file(dest).await;
                    progress.sub(progress.snapshot().downloaded);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error)
    }

    async fn single_attempt(
        &self,
        req: &DownloadRequest,
        url: &str,
        dest: &Path,
        progress: &Arc<DownloadProgress>,
    ) -> Result<u64, DownloadError> {
        // Waiting for the response headers is bounded by request_timeout; the
        // body itself has no total timeout (large slow downloads are
        // unpredictable) and uses per-chunk stall detection instead.
        let mut response = tokio::time::timeout(
            self.request_timeout,
            apply_headers(self.client.get(url), &req.headers).send(),
        )
        .await
        .map_err(|_| {
            DownloadError::Network("timed out waiting for the download response".to_string())
        })?
        .map_err(|error| DownloadError::Network(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(DownloadError::Http {
                status: status.as_u16(),
            });
        }
        let declared_length =
            content_length(response.headers()).or_else(|| response.content_length());
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|error| DownloadError::Io(error.to_string()))?;
        let mut written: u64 = 0;
        loop {
            progress.check_cancelled()?;
            let chunk = match tokio::time::timeout(self.request_timeout, response.chunk()).await {
                Ok(result) => result.map_err(|error| DownloadError::Network(error.to_string()))?,
                Err(_) => {
                    return Err(DownloadError::Network(format!(
                        "download stalled: no new data for {} seconds",
                        self.request_timeout.as_secs()
                    )));
                }
            };
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk)
                .await
                .map_err(|error| DownloadError::Io(error.to_string()))?;
            written += chunk.len() as u64;
            progress.add(chunk.len() as u64);
        }
        file.flush()
            .await
            .map_err(|error| DownloadError::Io(error.to_string()))?;
        drop(file);

        if self.options.verify_size
            && let Some(declared) = declared_length
        {
            verify_size(declared, written, req.size_is_advisory)?;
        }
        if let Some(expected_md5) = &req.md5 {
            verify_md5(dest, expected_md5).await?;
        }
        Ok(written)
    }

    async fn download_multipart(
        &self,
        req: &DownloadRequest,
        url: &str,
        total: u64,
        dest: &Path,
        progress: &Arc<DownloadProgress>,
    ) -> Result<u64, DownloadError> {
        let parts = plan_ranges(total, self.options.concurrency);
        if parts.len() <= 1 {
            return self.download_single(req, url, dest, progress).await;
        }
        self.download_ranges(req, url, parts, total, dest, progress)
            .await
    }

    /// Strict bounded-Range chunking: mandatory when the source declares
    /// `max_chunk_size`. Unlike ordinary multipart this is cap-first: it
    /// would rather issue more pieces than send an over-cap Range, and it
    /// never falls back to a plain GET (which such CDNs always 403).
    async fn download_chunked(
        &self,
        req: &DownloadRequest,
        url: &str,
        total: u64,
        dest: &Path,
        progress: &Arc<DownloadProgress>,
    ) -> Result<u64, DownloadError> {
        let parts = plan_bounded_ranges(total, self.options.concurrency, req.max_chunk_size);
        if parts.is_empty() {
            return Err(DownloadError::Other(
                "strict chunked download needs a valid chunk cap".to_string(),
            ));
        }
        self.download_ranges(req, url, parts, total, dest, progress)
            .await
    }

    /// Download the given ranges concurrently into one file, then verify the
    /// total length and MD5.
    async fn download_ranges(
        &self,
        req: &DownloadRequest,
        url: &str,
        parts: Vec<(u64, u64)>,
        total: u64,
        dest: &Path,
        progress: &Arc<DownloadProgress>,
    ) -> Result<u64, DownloadError> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dest)
            .map_err(|error| DownloadError::Io(error.to_string()))?;
        file.set_len(total)
            .map_err(|error| DownloadError::Io(error.to_string()))?;
        let file = Arc::new(file);

        let attempts = self.options.max_retries.max(1);
        let mut tasks = tokio::task::JoinSet::new();
        for (start, end) in parts {
            tasks.spawn(download_part(
                self.client.clone(),
                url.to_string(),
                req.headers.clone(),
                start,
                end,
                Arc::clone(&file),
                Arc::clone(progress),
                attempts,
                self.request_timeout,
            ));
        }

        let mut first_error: Option<DownloadError> = None;
        while let Some(result) = tasks.join_next().await {
            let error = match result {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => error,
                Err(join_error) => DownloadError::Other(join_error.to_string()),
            };
            if first_error.is_none() {
                first_error = Some(error);
                // Do not wait for the remaining pieces once one has failed.
                tasks.abort_all();
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        drop(file);

        let written = progress.snapshot().downloaded;
        if self.options.verify_size && written != total {
            return Err(DownloadError::Integrity(format!(
                "merged piece size mismatch: got {written} bytes, expected {total} bytes"
            )));
        }
        if let Some(expected_md5) = &req.md5 {
            verify_md5(dest, expected_md5).await?;
        }
        Ok(written)
    }
}

/// One piece: independent request and retry budget, written at its byte offset
/// into the shared file.
#[allow(clippy::too_many_arguments)]
async fn download_part(
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    start: u64,
    end: u64,
    file: Arc<std::fs::File>,
    progress: Arc<DownloadProgress>,
    attempts: u32,
    request_timeout: Duration,
) -> Result<(), DownloadError> {
    let expected = end - start + 1;
    let mut last_error = DownloadError::Other("piece download failed".to_string());
    for attempt in 0..attempts {
        if attempt > 0 {
            sleep_before_retry(attempt).await;
        }
        progress.check_cancelled()?;
        let request =
            apply_headers(client.get(&url), &headers).header(RANGE, format!("bytes={start}-{end}"));
        match part_attempt(
            &progress,
            file.as_ref(),
            request,
            start,
            expected,
            request_timeout,
        )
        .await
        {
            Ok(()) => return Ok(()),
            // The source claimed Range support but answered with the whole
            // body: fall back to the single connection immediately, no retry.
            Err(error @ DownloadError::RangesUnsupported) => return Err(error),
            Err(error) if error.is_retryable() && attempt + 1 < attempts => {
                tracing::debug!("part {start}-{end} attempt {} failed: {error}", attempt + 1);
                last_error = error;
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error)
}

async fn part_attempt(
    progress: &Arc<DownloadProgress>,
    file: &std::fs::File,
    request: reqwest::RequestBuilder,
    start: u64,
    expected: u64,
    request_timeout: Duration,
) -> Result<(), DownloadError> {
    let mut response = tokio::time::timeout(request_timeout, request.send())
        .await
        .map_err(|_| {
            DownloadError::Network("timed out waiting for the piece response".to_string())
        })?
        .map_err(|error| DownloadError::Network(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(DownloadError::Http {
            status: status.as_u16(),
        });
    }
    // A Range request answered with 200 means the source ignored the Range and
    // the piece result cannot be trusted.
    if status.as_u16() == 200 && start > 0 {
        return Err(DownloadError::RangesUnsupported);
    }
    let mut offset = start;
    let mut part_written: u64 = 0;
    let result = loop {
        if let Err(error) = progress.check_cancelled() {
            break Err(error);
        }
        let chunk = match tokio::time::timeout(request_timeout, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => break Ok(()),
            Ok(Err(error)) => break Err(DownloadError::Network(error.to_string())),
            Err(_) => {
                break Err(DownloadError::Network(format!(
                    "piece download stalled: no new data for {} seconds",
                    request_timeout.as_secs()
                )));
            }
        };
        if let Err(error) = write_at(file, offset, &chunk) {
            break Err(DownloadError::Io(error));
        }
        offset += chunk.len() as u64;
        part_written += chunk.len() as u64;
        progress.add(chunk.len() as u64);
    };
    if let Err(error) = result {
        // Roll back this piece's counted bytes before the retry rewrites it.
        progress.sub(part_written);
        return Err(error);
    }
    if part_written != expected {
        progress.sub(part_written);
        return Err(DownloadError::Integrity(format!(
            "piece {start} byte count mismatch: got {part_written}, expected {expected}"
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn write_at(file: &std::fs::File, offset: u64, data: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::FileExt;
    file.write_at(data, offset)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
fn write_at(file: &std::fs::File, offset: u64, data: &[u8]) -> Result<(), String> {
    use std::os::windows::fs::FileExt;
    file.seek_write(data, offset)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(not(any(unix, windows)))]
fn write_at(_file: &std::fs::File, _offset: u64, _data: &[u8]) -> Result<(), String> {
    Err("piece writing is not supported on this platform".to_string())
}

fn apply_headers(
    mut request: reqwest::RequestBuilder,
    headers: &[(String, String)],
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            request = request.header(name, value);
        }
    }
    request
}

/// Integrity check: byte-exact by default; when the declared size is only
/// advisory, shorter-than-declared is the only rejection.
fn verify_size(expected: u64, actual: u64, advisory: bool) -> Result<(), DownloadError> {
    if expected == 0 {
        return Ok(());
    }
    let ok = if advisory {
        actual >= expected
    } else {
        actual == expected
    };
    if ok {
        return Ok(());
    }
    if advisory {
        Err(DownloadError::Integrity(format!(
            "file too short: got {actual} bytes, expected at least {expected} bytes"
        )))
    } else {
        Err(DownloadError::Integrity(format!(
            "file size mismatch: got {actual} bytes, expected {expected} bytes"
        )))
    }
}

/// Hash the file with MD5 and compare against the expected value; an empty
/// expectation skips the check.
fn verify_md5_sync(path: &Path, expected: &str) -> Result<(), DownloadError> {
    if expected.is_empty() {
        return Ok(());
    }
    let mut file =
        std::fs::File::open(path).map_err(|error| DownloadError::Io(error.to_string()))?;
    let mut hasher = Md5::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        use std::io::Read;
        let n = file
            .read(&mut buf)
            .map_err(|error| DownloadError::Io(error.to_string()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual == expected.to_ascii_lowercase() {
        Ok(())
    } else {
        Err(DownloadError::Integrity(format!(
            "MD5 mismatch: got {actual}, expected {expected}"
        )))
    }
}

async fn verify_md5(path: &Path, expected: &str) -> Result<(), DownloadError> {
    // The hash runs on the blocking pool-sized file inline; downloads are
    // already async boundaries and the files are bounded by the source size.
    verify_md5_sync(path, expected)
}

/// Split the total length into contiguous ranges for the worker count; the
/// last piece absorbs the remainder.
///
/// Piece granularity is floored at 1 MiB: small files send one request even
/// under high concurrency instead of a dozen Range requests.
fn plan_ranges(total: u64, concurrency: usize) -> Vec<(u64, u64)> {
    if total == 0 {
        return Vec::new();
    }
    const MIN_PART_SIZE: u64 = 1024 * 1024;
    let max_useful = (total / MIN_PART_SIZE).max(1);
    let workers = (concurrency.clamp(1, 16) as u64).min(max_useful);
    let chunk = total.div_ceil(workers);
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + chunk - 1).min(total - 1);
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

/// Strict bounded chunking: no piece exceeds `max_chunk`; the piece count may
/// exceed the worker count.
///
/// googlevideo 403s any over-cap Range, so the cap wins: start from an even
/// split and shrink to the cap when the split would exceed it.
fn plan_bounded_ranges(total: u64, concurrency: usize, max_chunk: u64) -> Vec<(u64, u64)> {
    if total == 0 || max_chunk == 0 {
        return Vec::new();
    }
    let workers = concurrency.clamp(1, 16) as u64;
    let mut part_size = total / workers;
    if part_size == 0 || part_size > max_chunk {
        part_size = max_chunk;
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + part_size - 1).min(total - 1);
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

fn supports_ranges(headers: &HeaderMap) -> bool {
    headers
        .get(ACCEPT_RANGES)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("bytes"))
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// Parse the total length out of `Content-Range: bytes 0-0/12345`.
fn parse_content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse::<u64>().ok()
}

/// Candidate download urls for one resource, in attempt order: primary, then
/// the source-injected backup CDNs, each followed by its NetEase node rewrite.
/// The NetEase CDN m8/m801/m804/m704 nodes frequently 403; the m7/m701 family
/// is the last-resort rewrite.
pub fn candidate_download_urls(primary: &str, extra: &[String]) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    let mut push = |value: &str| {
        let value = value.trim();
        if value.is_empty() || candidates.iter().any(|existing| existing == value) {
            return;
        }
        candidates.push(value.to_string());
        let rewritten = rewrite_netease_host(value);
        if rewritten != value && !candidates.contains(&rewritten) {
            candidates.push(rewritten);
        }
    };
    push(primary);
    for url in extra {
        push(url);
    }
    candidates
}

/// NetEase node replacement table (mirrors the upstream rewrite).
pub fn rewrite_netease_host(url: &str) -> String {
    const REPLACEMENTS: [(&str, &str); 4] = [
        ("m801.", "m701."),
        ("m804.", "m701."),
        ("m704.", "m701."),
        ("m8.", "m7."),
    ];
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, url),
    };
    let (host, tail) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    let rewritten = REPLACEMENTS
        .iter()
        .find_map(|(from, to)| host.starts_with(from).then(|| host.replacen(from, to, 1)));
    match (rewritten, scheme) {
        (Some(host), Some(scheme)) => format!("{scheme}://{host}{tail}"),
        (Some(host), None) => format!("{host}{tail}"),
        (None, _) => url.to_string(),
    }
}

fn staging_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());
    dest.with_file_name(format!(".{name}.part"))
}

/// Exponential backoff with jitter so concurrent pieces do not retry in lockstep.
async fn sleep_before_retry(attempt: u32) {
    let backoff = 400u64.saturating_mul(1 << attempt.min(5)).min(8_000);
    let jitter = (retry_jitter_seed() % 250) as u64;
    tokio::time::sleep(Duration::from_millis(backoff + jitter)).await;
}

fn retry_jitter_seed() -> u128 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() as u128)
        .unwrap_or(0);
    nanos.wrapping_mul(31).wrapping_add(seq as u128)
}

// ───────────────────────── Post-download finish ─────────────────────────

/// Metadata written after a successful download (tags.rs port).
#[derive(Debug, Clone, Default)]
pub struct DownloadMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover: Option<Vec<u8>>,
    pub lyric: Option<String>,
}

impl DownloadMetadata {
    pub fn new(title: &str, artist: &str, album: &str) -> Self {
        Self {
            title: title.trim().to_string(),
            artist: artist.trim().to_string(),
            album: album.trim().to_string(),
            ..Self::default()
        }
    }
}

/// Write title/artist/album/lyrics and try to embed the cover.
///
/// When the container rejects the cover format (a WebP cover into FLAC, for
/// example) the write is retried without the cover so the tags still land.
pub fn embed_tags(path: &Path, metadata: &DownloadMetadata) -> Result<(), String> {
    let lyrics = metadata
        .lyric
        .as_ref()
        .map(|lyric| lyric.trim())
        .filter(|lyric| !lyric.is_empty())
        .map(|lyric| lyric.to_string());

    let usable_cover = metadata
        .cover
        .as_ref()
        .filter(|cover| cover_is_usable(cover))
        .cloned();

    let mut edit = MetadataEdit {
        title: Some(metadata.title.clone()).filter(|value| !value.is_empty()),
        artist: Some(metadata.artist.clone()).filter(|value| !value.is_empty()),
        album: Some(metadata.album.clone()).filter(|value| !value.is_empty()),
        lyrics,
        cover: usable_cover,
    };

    match write_metadata(path, &edit) {
        Ok(()) => Ok(()),
        Err(error) if edit.cover.is_some() => {
            tracing::warn!(
                "embedding cover failed for {}: {error}, retrying without cover",
                path.display()
            );
            edit.cover = None;
            write_metadata(path, &edit)
        }
        Err(error) => Err(error),
    }
}

// The tag-write implementation moved to the shared `tag_write` unit
// (`cfg(any(feature = "local", feature = "download"))`, see
// docs/integration/LOCAL_ENHANCEMENT_DESIGN.md §4). The write is now staged
// through a same-directory temp file and promoted with an atomic rename, so
// a failed write can no longer leave a half-written source file. The public
// paths below are unchanged for hosts and for this module's tests.
pub use crate::tag_write::{MetadataEdit, write_metadata};

/// Write the lyric (optionally with a translation) as a `.lrc` file next to
/// the audio file.
pub fn write_lyric_file(
    audio_path: &Path,
    lyric: &str,
    translation: Option<&str>,
) -> Result<PathBuf, String> {
    let merged = match translation.map(str::trim).filter(|text| !text.is_empty()) {
        Some(translation) => merge_lyric(lyric, translation),
        None => lyric.trim().to_string(),
    };
    if merged.is_empty() {
        return Err("empty lyric".to_string());
    }
    let lyric_path = audio_path.with_extension("lrc");
    std::fs::write(&lyric_path, merged).map_err(|error| error.to_string())?;
    Ok(lyric_path)
}

/// Merge the translation into the lyric by timestamp, producing a bilingual LRC.
pub fn merge_lyric(lyric: &str, translation: &str) -> String {
    let main = crate::lyrics::parse_document(lyric);
    let translated = crate::lyrics::parse_document(translation);
    if let (Ok(main), Ok(translated)) = (main, translated)
        && !main.lines.is_empty()
    {
        let mut merged = String::new();
        for line in &main.lines {
            merged.push_str(&format_lrc_line(line.start_ms, &line.text));
            if let Some(candidate) = translated
                .lines
                .iter()
                .find(|candidate| candidate.start_ms == line.start_ms)
                .filter(|candidate| !candidate.text.trim().is_empty())
            {
                merged.push_str(&format_lrc_line(candidate.start_ms, &candidate.text));
            }
        }
        if !merged.trim().is_empty() {
            return merged;
        }
    }
    // Unparseable timelines degrade to plain concatenation.
    format!("{}\n{}\n", lyric.trim(), translation.trim())
}

/// Pick the standard LRC text suited for the `.lrc` file and tag embedding
/// from a musid-dli `LyricDocument`.
///
/// The raw text is trusted only when it actually carries bracketed `mm:ss.xx`
/// timestamps with content; otherwise the parsed lines (word timing collapsed
/// to line text) are rendered. `None` means no usable LRC exists: rather than
/// writing a source's private JSON or binary format into a `.lrc` file, the
/// lyric is skipped entirely.
pub fn standard_lyric(document: &crate::models::LyricDocument) -> Option<String> {
    if looks_like_timed_lrc(&document.original) {
        return Some(document.original.trim().to_string());
    }
    let mut rendered = String::new();
    for line in &document.lines {
        let text = if line.text.trim().is_empty() {
            line.words
                .iter()
                .map(|word| word.text.as_str())
                .collect::<String>()
        } else {
            line.text.clone()
        };
        let text = text.trim();
        if !text.is_empty() {
            rendered.push_str(&format_lrc_line(line.start_ms, text));
        }
    }
    (!rendered.trim().is_empty()).then_some(rendered)
}

/// Does the text carry at least one bracketed `mm:ss.xx` timestamp plus content?
fn looks_like_timed_lrc(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    for index in 0..bytes.len().saturating_sub(6) {
        if bytes[index] == b'['
            && bytes[index + 1].is_ascii_digit()
            && bytes[index + 2].is_ascii_digit()
            && bytes[index + 3] == b':'
            && bytes[index + 4].is_ascii_digit()
        {
            return true;
        }
    }
    false
}

fn format_lrc_line(timestamp_ms: u64, text: &str) -> String {
    let minutes = timestamp_ms / 60_000;
    let seconds = (timestamp_ms % 60_000) as f64 / 1000.0;
    format!("[{minutes:02}:{seconds:05.2}]{}\n", text.trim())
}

/// Is the cover a decodable image by magic bytes? (Substitutes the upstream
/// full image decode; see the module docs.)
fn cover_is_usable(cover: &[u8]) -> bool {
    if cover.len() < 12 {
        return false;
    }
    cover.starts_with(b"\xFF\xD8\xFF")
        || cover.starts_with(b"\x89PNG\r\n\x1a\n")
        || cover.starts_with(b"GIF8")
        || (cover.starts_with(b"RIFF") && &cover[8..12] == b"WEBP")
        || cover.starts_with(b"BM")
        || (&cover[4..12] == b"ftypavif" || &cover[4..12] == b"ftypheic")
}

/// Verify the downloaded file really is complete audio.
///
/// The magic sniffer rejects non-audio payloads (error pages posing as .mp3);
/// when lofty cannot parse the container (rare codecs, damaged tails) the file
/// is kept with a warning instead of deleted. The duration-tolerance verdict
/// matches the upstream one: the shorter side gets 3 seconds or 5 percent,
/// whichever is smaller, the longer side gets an extra 1.5 seconds, because
/// sources report rounded seconds and decoders append padding.
pub fn validate_audio(path: &Path, expected: Duration) -> Result<(), String> {
    use lofty::file::AudioFile;
    use std::io::Read;
    let mut head = [0u8; 16];
    let read = std::fs::File::open(path)
        .and_then(|mut file| file.read(&mut head))
        .map_err(|error| error.to_string())?;
    if sniff_audio_extension(&head[..read]).is_none() {
        return Err("downloaded content is not an audio file".to_string());
    }

    let tagged = match lofty::read_from_path(path) {
        Ok(tagged) => tagged,
        Err(error) => {
            tracing::warn!("audio validation skipped for {}: {error}", path.display());
            return Ok(());
        }
    };
    let actual = tagged.properties().duration();
    if expected.is_zero() || actual.is_zero() {
        return Ok(());
    }

    let tolerance = Duration::from_secs(3).min(expected / 20);
    let longer = Duration::from_millis(1500).max(tolerance);
    if actual + tolerance < expected || actual > expected + longer {
        return Err(format!(
            "audio duration {:.1}s is too far from the source duration {:.1}s",
            actual.as_secs_f64(),
            expected.as_secs_f64()
        ));
    }
    Ok(())
}

/// Audio container magic sniff (replaces the host-side naming.rs detector).
fn sniff_audio_extension(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"ID3") {
        return Some("mp3");
    }
    if head.starts_with(b"fLaC") {
        return Some("flac");
    }
    if head.starts_with(b"OggS") {
        return Some("ogg");
    }
    if head.starts_with(b"MAC ") {
        return Some("ape");
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        return Some("m4a");
    }
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WAVE" {
        return Some("wav");
    }
    if head.len() >= 12 && head.starts_with(b"FORM") && &head[8..12] == b"AIFF" {
        return Some("aiff");
    }
    if head.starts_with(b"ADIF") {
        return Some("aac");
    }
    // Bare MPEG audio frame sync (no ID3 header).
    if head.len() >= 2 && head[0] == 0xFF && (head[1] & 0xE0) == 0xE0 {
        return Some("mp3");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_cover_the_whole_file_without_gaps() {
        let total = 10 * 1024 * 1024 + 7;
        let ranges = plan_ranges(total, 4);

        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges.first().unwrap().0, 0);
        assert_eq!(ranges.last().unwrap().1, total - 1);
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].1 + 1, pair[1].0, "pieces must not leave gaps");
        }
        let covered: u64 = ranges.iter().map(|(start, end)| end - start + 1).sum();
        assert_eq!(covered, total);
    }

    #[test]
    fn tiny_files_stay_single_part() {
        assert_eq!(plan_ranges(1024, 8), vec![(0, 1023)]);
    }

    #[test]
    fn bounded_ranges_never_exceed_the_configured_cap() {
        let total = 5 * 1024 * 1024 + 3;
        let cap = 1024 * 1024;
        let ranges = plan_bounded_ranges(total, 4, cap);

        assert!(ranges.iter().all(|(start, end)| end - start < cap));
        assert_eq!(ranges.first().unwrap().0, 0);
        assert_eq!(ranges.last().unwrap().1, total - 1);
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].1 + 1, pair[1].0, "pieces must not leave gaps");
        }
        let covered: u64 = ranges.iter().map(|(start, end)| end - start + 1).sum();
        assert_eq!(covered, total);
    }

    #[test]
    fn bounded_ranges_use_the_cap_when_workers_cannot_cover_the_file() {
        let ranges = plan_bounded_ranges(1000, 4, 100);
        assert_eq!(ranges.len(), 10);
        assert_eq!(ranges.first().unwrap(), &(0, 99));
        assert_eq!(ranges.last().unwrap(), &(900, 999));
    }

    #[test]
    fn bounded_ranges_are_empty_without_a_cap_or_size() {
        assert!(plan_bounded_ranges(0, 4, 1024).is_empty());
        assert!(plan_bounded_ranges(1024, 4, 0).is_empty());
    }

    #[test]
    fn size_verification_rejects_truncated_downloads() {
        assert!(verify_size(2048, 2048, false).is_ok());
        let error = verify_size(2048, 1024, false).unwrap_err();
        assert!(matches!(error, DownloadError::Integrity(_)));
    }

    #[test]
    fn advisory_size_allows_longer_but_not_shorter_files() {
        assert!(verify_size(2048, 2050, true).is_ok());
        assert!(verify_size(2048, 4096, true).is_ok());
        assert!(verify_size(2048, 1000, true).is_err());
    }

    #[test]
    fn content_range_parser_reads_total_length() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes */12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
    }

    #[test]
    fn staging_file_stays_next_to_the_destination() {
        let dest = PathBuf::from("/music/voicefox/test - song.mp3");
        assert_eq!(
            staging_path(&dest),
            PathBuf::from("/music/voicefox/.test - song.mp3.part")
        );
    }

    #[test]
    fn netease_nodes_are_rewritten_to_the_m7_family() {
        assert_eq!(
            rewrite_netease_host("http://m8.music.126.net/a/b.mp3?id=1"),
            "http://m7.music.126.net/a/b.mp3?id=1"
        );
        assert_eq!(
            rewrite_netease_host("https://m801.music.126.net/x.flac"),
            "https://m701.music.126.net/x.flac"
        );
        assert_eq!(
            rewrite_netease_host("https://m804.music.126.net/x.flac"),
            "https://m701.music.126.net/x.flac"
        );
        assert_eq!(
            rewrite_netease_host("https://cdn.example.com/m8.song.flac"),
            "https://cdn.example.com/m8.song.flac"
        );
    }

    #[test]
    fn candidates_put_the_original_url_first() {
        assert_eq!(
            candidate_download_urls("http://m8.music.126.net/a.mp3", &[]),
            vec![
                "http://m8.music.126.net/a.mp3".to_string(),
                "http://m7.music.126.net/a.mp3".to_string()
            ]
        );
        assert_eq!(
            candidate_download_urls("https://cdn.example.com/a.flac", &[]),
            vec!["https://cdn.example.com/a.flac".to_string()]
        );
        assert!(candidate_download_urls("   ", &[]).is_empty());
    }

    #[test]
    fn platform_candidates_are_kept_in_order_and_deduplicated() {
        let extra = vec![
            "http://cdn2.example.com/a.flac".to_string(),
            // An exact duplicate of the primary and an empty string are dropped.
            "https://cdn1.example.com/a.flac".to_string(),
            String::new(),
            "http://cdn2.example.com/a.flac".to_string(),
        ];
        let candidates = candidate_download_urls("https://cdn1.example.com/a.flac", &extra);
        assert_eq!(
            candidates,
            vec![
                "https://cdn1.example.com/a.flac".to_string(),
                "http://cdn2.example.com/a.flac".to_string(),
            ]
        );
    }

    /// T5 (§7): the §2.5 field table row by row; nothing mapped twice.
    #[test]
    fn download_request_from_stream_maps_every_field() {
        let stream = StreamInfo {
            url: "https://cdn.example.com/a.flac".to_string(),
            quality: crate::models::StreamQuality::Lossless,
            format: crate::models::AudioFormat::Flac,
            duration: Some(Duration::from_secs(200)),
            expires_at_unix: Some(1_800_000_000),
            content_length: Some(9_000),
            checksum: Some("  abc123  ".to_string()),
            checksum_algorithm: None,
            headers: vec![("User-Agent".to_string(), "ua".to_string())],
            fallback_urls: vec!["https://cdn2.example.com/a.flac".to_string()],
            fallback_endpoints: vec![],
            index_range: Some(crate::models::ByteRange { start: 0, end: 9 }),
            init_range: None,
            max_chunk_size: Some(1 << 20),
            size_is_advisory: true,
        };
        let request =
            download_request_from_stream(&stream, PathBuf::from("/music/voicefox/a.flac"));
        assert_eq!(request.url, "https://cdn.example.com/a.flac");
        assert_eq!(
            request.headers,
            vec![("User-Agent".to_string(), "ua".to_string())]
        );
        assert_eq!(request.dest, PathBuf::from("/music/voicefox/a.flac"));
        assert_eq!(request.expected_size, Some(9_000));
        assert!(request.size_is_advisory);
        // Checksum trimmed into the MD5 gate (absent algorithm = MD5).
        assert_eq!(request.md5.as_deref(), Some("abc123"));
        assert_eq!(
            request.candidate_urls,
            vec!["https://cdn2.example.com/a.flac"]
        );
        assert_eq!(request.max_chunk_size, 1 << 20);
        // Option 0 equivalence: an absent cap must read as "strict chunking
        // off" (engine reads `req.max_chunk_size > 0`).
        let uncapped = download_request_from_stream(
            &StreamInfo {
                url: "https://cdn.example.com/b.mp3".to_string(),
                quality: crate::models::StreamQuality::Low,
                format: crate::models::AudioFormat::Mp3,
                duration: None,
                expires_at_unix: None,
                content_length: None,
                checksum: None,
                checksum_algorithm: None,
                headers: vec![],
                fallback_urls: vec![],
                fallback_endpoints: vec![],
                index_range: None,
                init_range: None,
                max_chunk_size: None,
                size_is_advisory: false,
            },
            PathBuf::from("/music/voicefox/b.mp3"),
        );
        assert_eq!(uncapped.max_chunk_size, 0);
        assert_eq!(uncapped.expected_size, None);
        assert_eq!(uncapped.md5, None);
        assert!(!uncapped.size_is_advisory);
    }

    /// T5 (§7): a declared Sha1/Sha256 checksum is dropped, never fed to the
    /// MD5 gate (MODEL_ALIGNMENT_DESIGN.md §2.3).
    #[test]
    fn non_md5_checksum_is_dropped_not_reinterpreted() {
        let stream = StreamInfo {
            url: "https://cdn.example.com/a.flac".to_string(),
            quality: crate::models::StreamQuality::Lossless,
            format: crate::models::AudioFormat::Flac,
            duration: None,
            expires_at_unix: None,
            content_length: Some(1),
            checksum: Some("deadbeef".to_string()),
            checksum_algorithm: Some(crate::models::ChecksumAlgorithm::Sha256),
            headers: vec![],
            fallback_urls: vec![],
            fallback_endpoints: vec![],
            index_range: None,
            init_range: None,
            max_chunk_size: None,
            size_is_advisory: false,
        };
        let request =
            download_request_from_stream(&stream, PathBuf::from("/music/voicefox/a.flac"));
        assert_eq!(request.md5, None);
        // The explicit Md5 spelling still verifies.
        let md5_stream = StreamInfo {
            checksum_algorithm: Some(crate::models::ChecksumAlgorithm::Md5),
            ..stream
        };
        let request =
            download_request_from_stream(&md5_stream, PathBuf::from("/music/voicefox/a.flac"));
        assert_eq!(request.md5.as_deref(), Some("deadbeef"));
    }

    /// T5 (§7): structured endpoints reach the engine candidate list after
    /// the legacy strings, in slot order, still passing through the NetEase
    /// node rewrite and dedup (§2.4 ordering contract).
    #[test]
    fn structured_endpoints_reach_the_candidate_list() {
        let stream = StreamInfo {
            url: "http://m8.music.126.net/a.mp3".to_string(),
            quality: crate::models::StreamQuality::High,
            format: crate::models::AudioFormat::Mp3,
            duration: None,
            expires_at_unix: None,
            content_length: None,
            checksum: None,
            checksum_algorithm: None,
            headers: vec![],
            fallback_urls: vec!["http://cdn.example.com/legacy.mp3".to_string()],
            fallback_endpoints: vec![
                crate::models::FallbackEndpoint {
                    url: "http://cdn.example.com/legacy.mp3".to_string(),
                    headers: vec![("Referer".to_string(), "https://r".to_string())],
                    expires_at_unix: Some(1),
                    content_length: Some(2),
                    quality: Some(crate::models::StreamQuality::Low),
                },
                crate::models::FallbackEndpoint {
                    url: "http://m804.music.126.net/b.mp3".to_string(),
                    headers: vec![],
                    expires_at_unix: None,
                    content_length: None,
                    quality: None,
                },
            ],
            index_range: None,
            init_range: None,
            max_chunk_size: None,
            size_is_advisory: false,
        };
        let request = download_request_from_stream(&stream, PathBuf::from("/music/voicefox/a.mp3"));
        assert_eq!(
            request.candidate_urls,
            vec![
                "http://cdn.example.com/legacy.mp3".to_string(),
                "http://m804.music.126.net/b.mp3".to_string(),
            ]
        );
        // Engine-side view: primary first, rewrite applied to the NetEase
        // hosts, duplicate endpoint URL collapsed.
        assert_eq!(
            candidate_download_urls(&request.url, &request.candidate_urls),
            vec![
                "http://m8.music.126.net/a.mp3".to_string(),
                "http://m7.music.126.net/a.mp3".to_string(),
                "http://cdn.example.com/legacy.mp3".to_string(),
                // Each candidate keeps its original address before the node
                // rewrite (pinned upstream order, candidates_put_the_original_url_first).
                "http://m804.music.126.net/b.mp3".to_string(),
                "http://m701.music.126.net/b.mp3".to_string(),
            ]
        );
    }

    #[test]
    fn download_error_retry_matrix_matches_upstream() {
        assert!(DownloadError::Network("x".to_string()).is_retryable());
        assert!(DownloadError::Io("x".to_string()).is_retryable());
        assert!(DownloadError::RangesUnsupported.is_retryable());
        assert!(DownloadError::Http { status: 502 }.is_retryable());
        assert!(DownloadError::Http { status: 429 }.is_retryable());
        assert!(!DownloadError::Http { status: 403 }.is_retryable());
        assert!(!DownloadError::Integrity("x".to_string()).is_retryable());
        assert!(!DownloadError::Cancelled.is_retryable());
    }

    #[test]
    fn magic_sniffer_recognises_the_common_audio_containers() {
        assert_eq!(sniff_audio_extension(b"ID3\x04\x00"), Some("mp3"));
        assert_eq!(sniff_audio_extension(b"fLaC\x00\x00\x00"), Some("flac"));
        assert_eq!(sniff_audio_extension(b"OggS\x00\x02"), Some("ogg"));
        assert_eq!(sniff_audio_extension(b"\xFF\xFB\x90\x00"), Some("mp3"));
        let mp4 = [0u8, 0, 0, 0, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm'];
        assert_eq!(sniff_audio_extension(&mp4), Some("m4a"));
        let wav = [b'R', b'I', b'F', b'F', 0, 0, 0, 0, b'W', b'A', b'V', b'E'];
        assert_eq!(sniff_audio_extension(&wav), Some("wav"));
        assert_eq!(sniff_audio_extension(b"<html><body>404"), None);
        assert_eq!(sniff_audio_extension(b""), None);
    }

    #[test]
    fn cover_sniffer_accepts_the_embeddable_formats_only() {
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpeg.extend_from_slice(&[0u8; 12]);
        assert!(cover_is_usable(&jpeg));
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0u8; 8]);
        assert!(cover_is_usable(&png));
        assert!(!cover_is_usable(b"not an image at all"));
        assert!(!cover_is_usable(b""));
        assert!(!cover_is_usable(b"RIFFshort"));
    }

    // ───────────────── M2 error bridge (AUTH_REGISTRY_DESIGN.md §8 item 8) ─────────────────

    /// Every engine variant carries an M2 category; the `Error` bridge and
    /// the pure mapping agree; the M2-face retryability follows
    /// `Error::is_retryable` semantics for the mapped category; and every
    /// M2-retryable row is also engine-retryable (no face contradicts the
    /// engine's own verdict).
    #[test]
    fn download_error_category_bridge_covers_all_seven_variants() {
        let pid = "netease";
        let cases: Vec<(DownloadError, ErrorCategory)> = vec![
            (
                DownloadError::Network("connection reset".to_string()),
                ErrorCategory::Network,
            ),
            (
                DownloadError::Http { status: 429 },
                ErrorCategory::RateLimited,
            ),
            (
                DownloadError::Http { status: 502 },
                ErrorCategory::Unavailable,
            ),
            (
                DownloadError::Http { status: 408 },
                ErrorCategory::Unavailable,
            ),
            (DownloadError::Cancelled, ErrorCategory::Cancelled),
            (
                DownloadError::Integrity("MD5 mismatch".to_string()),
                ErrorCategory::ProviderInternal,
            ),
            (
                DownloadError::Io("disk full".to_string()),
                ErrorCategory::Io,
            ),
            (
                DownloadError::RangesUnsupported,
                ErrorCategory::UnsupportedOperation,
            ),
            (
                DownloadError::Other("missing download url".to_string()),
                ErrorCategory::Provider,
            ),
        ];
        for (error, category) in &cases {
            assert_eq!(error.category(), *category, "{error}");
            let bridged = error.to_error(pid);
            assert_eq!(bridged.category(), *category, "{error}");
            let m2_retryable = matches!(
                category,
                ErrorCategory::Network | ErrorCategory::RateLimited | ErrorCategory::Unavailable
            );
            assert_eq!(bridged.is_retryable(), m2_retryable, "{error}");
            if m2_retryable {
                assert!(
                    error.is_retryable(),
                    "M2 face contradicts the engine: {error}"
                );
            }
            if matches!(error, DownloadError::Cancelled | DownloadError::Io(_)) {
                // `Cancelled` bridges to the payload-less `Error::Cancelled`,
                // and the pinned `Io` row bridges to the legacy `Error::Io`
                // variant, which carries no provider slot; both keep the
                // underlying text without provider attribution.
                assert_eq!(bridged.provider_id(), None, "{error}");
            } else {
                assert_eq!(bridged.provider_id(), Some(pid), "{error}");
            }
        }
        // Declared residual: Http statuses outside the pinned set stay in
        // the unclassified catch-all (and non-retryable, like the engine).
        for status in [400u16, 403, 404, 410, 451, 499] {
            let error = DownloadError::Http { status };
            assert_eq!(error.category(), ErrorCategory::Provider, "{error}");
            assert!(!error.to_error(pid).is_retryable(), "{error}");
            assert!(!error.is_retryable(), "{error}");
        }
    }

    /// The engine retry policy and the M2 face diverge exactly where the
    /// design declares it (TRANSPORT_DESIGN.md §6, AUTH_REGISTRY_DESIGN.md
    /// §1.1): `Io` is engine-retryable (transient disk errors) but `Io` is
    /// not an M2-retryable category, and `RangesUnsupported` is
    /// engine-"retryable" as a strategy switch, not a resend, while
    /// `UnsupportedOperation` is never blind-resendable. `DownloadError::
    /// is_retryable` itself stays byte-identical with the upstream engine.
    #[test]
    fn engine_retry_policy_and_m2_face_diverge_only_where_declared() {
        assert!(DownloadError::Io("x".to_string()).is_retryable());
        assert!(
            !DownloadError::Io("x".to_string())
                .to_error("netease")
                .is_retryable()
        );
        assert!(DownloadError::RangesUnsupported.is_retryable());
        assert!(
            !DownloadError::RangesUnsupported
                .to_error("netease")
                .is_retryable()
        );
        // The other direction has no divergence: every M2-retryable mapped
        // error is also engine-retryable.
        let transient = [
            DownloadError::Network("x".to_string()),
            DownloadError::Http { status: 429 },
            DownloadError::Http { status: 408 },
            DownloadError::Http { status: 503 },
        ];
        for error in &transient {
            assert!(error.is_retryable(), "{error}");
            assert!(error.to_error("netease").is_retryable(), "{error}");
        }
    }

    /// §1.2 text preservation: no engine payload is silently dropped on the
    /// bridge — it reaches the `detail` slot, the `Io` payload or the
    /// `Provider` message, and the M2 Display renders it.
    #[test]
    fn bridge_preserves_the_underlying_text_in_the_sanitized_slots() {
        let pid = "netease";
        let network = DownloadError::Network("connection closed before response".to_string());
        let bridged = network.to_error(pid);
        assert_eq!(bridged.detail(), Some("connection closed before response"));
        assert_eq!(
            bridged.to_string(),
            "network error: connection closed before response"
        );

        let rate = DownloadError::Http { status: 429 }.to_error(pid);
        assert!(matches!(
            rate,
            Error::RateLimited {
                retry_after: None,
                ..
            }
        ));
        assert_eq!(rate.detail(), Some("HTTP 429"));
        assert_eq!(rate.to_string(), "rate limited: HTTP 429");

        let unavailable = DownloadError::Http { status: 503 }.to_error(pid);
        assert_eq!(unavailable.detail(), Some("HTTP 503"));
        assert_eq!(unavailable.to_string(), "provider unavailable: HTTP 503");

        let integrity =
            DownloadError::Integrity("MD5 mismatch: got aa, expected bb".to_string()).to_error(pid);
        assert_eq!(
            integrity.detail(),
            Some("MD5 mismatch: got aa, expected bb")
        );
        assert_eq!(
            integrity.to_string(),
            "provider internal error: MD5 mismatch: got aa, expected bb"
        );

        let ranges = DownloadError::RangesUnsupported.to_error(pid);
        assert_eq!(
            ranges.detail(),
            Some("source does not support Range chunking")
        );
        assert_eq!(
            ranges.to_string(),
            "unsupported operation: source does not support Range chunking"
        );

        let io = DownloadError::Io("No space left on device".to_string()).to_error(pid);
        assert!(matches!(io, Error::Io(_)));
        assert!(io.to_string().contains("No space left on device"), "{io}");

        assert_eq!(
            DownloadError::Cancelled.to_error(pid).to_string(),
            "cancelled"
        );

        let refused = DownloadError::Http { status: 403 }.to_error(pid);
        assert_eq!(
            refused.to_string(),
            "provider netease failed: download: HTTP 403"
        );
        let other = DownloadError::Other("missing download url".to_string()).to_error(pid);
        assert_eq!(
            other.to_string(),
            "provider netease failed: download: missing download url"
        );
    }
}

/// Local loopback HTTP server used only by the tests: exercises the Range
/// behaviors the engine must handle (ported from the upstream test_support
/// server, trimmed to the paths the engine core exercises).
#[cfg(test)]
mod http_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RangeMode {
        /// Normal Range support: HEAD advertises bytes, ranged GETs get 206.
        Supported,
        /// No Range support at all: every GET answers 200 with the full body.
        Unsupported,
        /// HEAD claims `Accept-Ranges: bytes` but Range requests are ignored.
        Lying,
        /// googlevideo-style: HEAD answers 405, plain GET answers 403, an
        /// over-cap Range answers 403, only bounded Ranges are served.
        BoundedOnly(usize),
    }

    struct TestServer {
        audio_url: String,
        denied_url: String,
    }

    async fn spawn_test_server(audio: Vec<u8>, mode: RangeMode) -> Option<TestServer> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
        let addr = listener.local_addr().ok()?;
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let audio = audio.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 4096];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let head_only = request.starts_with("HEAD");
                    let first_line = request.lines().next().unwrap_or("").to_string();
                    let path = first_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    let range = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|line| line.split_once(':'))
                        .map(|(_, value)| value.trim().to_string());

                    if path.starts_with("/denied") {
                        let _ = stream
                            .write_all(
                                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                        let _ = stream.shutdown().await;
                        return;
                    }

                    let total = audio.len();
                    let head: String = match mode {
                        RangeMode::Supported | RangeMode::Lying => format!(
                            "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                        ),
                        RangeMode::Unsupported => format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                        ),
                        RangeMode::BoundedOnly(_) => String::from(
                            "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        ),
                    };
                    let response: Vec<u8> = if head_only {
                        head.into_bytes()
                    } else {
                        let parsed_range = range.as_deref().and_then(parse_bytes_range);
                        match (mode, parsed_range) {
                            (RangeMode::Supported, Some((start, end))) => {
                                let end = (end as usize).min(total.saturating_sub(1));
                                let start = (start as usize).min(total);
                                let body = &audio[start..=end];
                                format!(
                                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    total - 1,
                                    body.len()
                                )
                                .into_bytes()
                                .into_iter()
                                .chain(body.iter().copied())
                                .collect::<Vec<u8>>()
                            }
                            (RangeMode::BoundedOnly(cap), Some((start, end))) => {
                                if end.saturating_sub(start).saturating_add(1) as usize > cap {
                                    String::from(
                                        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                    )
                                    .into_bytes()
                                } else {
                                    let end = (end as usize).min(total.saturating_sub(1));
                                    let start = (start as usize).min(total);
                                    let body = &audio[start..=end];
                                    format!(
                                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                        total - 1,
                                        body.len()
                                    )
                                    .into_bytes()
                                    .into_iter()
                                    .chain(body.iter().copied())
                                    .collect::<Vec<u8>>()
                                }
                            }
                            (RangeMode::BoundedOnly(_), None) => String::from(
                                "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .into_bytes(),
                            // Unsupported and Lying both ignore the Range and
                            // answer with the full body under 200.
                            _ => {
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                                )
                                .into_bytes()
                                .into_iter()
                                .chain(audio.iter().copied())
                                .collect::<Vec<u8>>()
                            }
                        }
                    };
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Some(TestServer {
            audio_url: format!("http://{addr}/audio"),
            denied_url: format!("http://{addr}/denied"),
        })
    }

    /// Parse a `bytes=a-b` (or open-ended `bytes=a-`) Range header value.
    fn parse_bytes_range(value: &str) -> Option<(u64, u64)> {
        let spec = value.strip_prefix("bytes=")?;
        let (start, end) = spec.split_once('-')?;
        let start: u64 = start.trim().parse().ok()?;
        let end: u64 = if end.trim().is_empty() {
            u64::MAX
        } else {
            end.trim().parse().ok()?
        };
        Some((start, end))
    }

    fn engine() -> DownloadEngine {
        DownloadEngine::new(
            "",
            5,
            DownloadOptions {
                concurrency: 4,
                multipart: true,
                multipart_min_size: 1024,
                max_retries: 2,
                verify_size: true,
            },
        )
    }

    fn request(url: &str, dest: &Path) -> DownloadRequest {
        DownloadRequest {
            url: url.to_string(),
            headers: Vec::new(),
            dest: dest.to_path_buf(),
            expected_size: None,
            size_is_advisory: false,
            md5: None,
            candidate_urls: Vec::new(),
            max_chunk_size: 0,
        }
    }

    fn sample_audio(len: usize) -> Vec<u8> {
        // ID3 head so the result also passes the audio magic sniffer.
        let mut data = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
        data.resize(len, 0u8);
        for (index, byte) in data.iter_mut().enumerate().skip(10) {
            *byte = (index * 11 + 3) as u8;
        }
        data
    }

    async fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "musid-dli-dl-{tag}-{}-{}",
            std::process::id(),
            retry_jitter_seed() as u64
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        dir
    }

    #[tokio::test]
    async fn multipart_download_matches_the_source_bytes() {
        let Some(server) =
            spawn_test_server(sample_audio(3 * 1024 * 1024 + 7), RangeMode::Supported).await
        else {
            return; // sandbox without loopback listening: skip
        };
        let dir = temp_dir("multipart").await;
        let dest = dir.join("song.mp3");
        let progress = DownloadProgress::new();

        let written = engine()
            .download(&request(&server.audio_url, &dest), &progress)
            .await
            .unwrap();

        assert_eq!(written as usize, 3 * 1024 * 1024 + 7);
        let stored = std::fs::read(&dest).unwrap();
        assert_eq!(stored, sample_audio(3 * 1024 * 1024 + 7));
        assert!(
            !dir.join(".song.mp3.part").exists(),
            "staging file must be renamed away"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn candidate_fallback_serves_from_the_backup_url() {
        let Some(server) =
            spawn_test_server(sample_audio(2 * 1024 * 1024), RangeMode::Supported).await
        else {
            return;
        };
        let dir = temp_dir("candidates").await;
        let dest = dir.join("song.mp3");
        let progress = DownloadProgress::new();
        let mut req = request(&server.denied_url, &dest);
        req.candidate_urls = vec![server.audio_url.clone()];

        let written = engine().download(&req, &progress).await.unwrap();

        assert_eq!(written, 2 * 1024 * 1024);
        assert_eq!(std::fs::read(&dest).unwrap().len(), 2 * 1024 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lying_range_server_falls_back_to_a_single_connection() {
        let Some(server) =
            spawn_test_server(sample_audio(2 * 1024 * 1024 + 3), RangeMode::Lying).await
        else {
            return;
        };
        let dir = temp_dir("lying").await;
        let dest = dir.join("song.mp3");
        let progress = DownloadProgress::new();

        let written = engine()
            .download(&request(&server.audio_url, &dest), &progress)
            .await
            .unwrap();

        assert_eq!(written, 2 * 1024 * 1024 + 3);
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            sample_audio(2 * 1024 * 1024 + 3)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unsupported_ranges_download_in_one_piece() {
        let Some(server) =
            spawn_test_server(sample_audio(600 * 1024), RangeMode::Unsupported).await
        else {
            return;
        };
        let dir = temp_dir("unsupported").await;
        let dest = dir.join("song.mp3");
        let progress = DownloadProgress::new();

        let written = engine()
            .download(&request(&server.audio_url, &dest), &progress)
            .await
            .unwrap();

        assert_eq!(written, 600 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bounded_only_server_needs_the_strict_chunk_mode() {
        let Some(server) =
            spawn_test_server(sample_audio(10 * 1024), RangeMode::BoundedOnly(1024)).await
        else {
            return;
        };
        let dir = temp_dir("bounded").await;
        let progress = DownloadProgress::new();

        // Without the strict mode the plain GET is 403 and the download fails.
        let plain = engine()
            .download(
                &request(&server.audio_url, &dir.join("plain.mp3")),
                &progress,
            )
            .await;
        assert!(
            plain.is_err(),
            "plain GET must not succeed against a bounded-only CDN"
        );

        // With max_chunk_size every request stays a bounded Range and succeeds.
        let mut req = request(&server.audio_url, &dir.join("chunked.mp3"));
        req.expected_size = Some(10 * 1024);
        req.max_chunk_size = 1024;
        let written = engine().download(&req, &progress).await.unwrap();
        assert_eq!(written, 10 * 1024);
        assert_eq!(
            std::fs::read(dir.join("chunked.mp3")).unwrap(),
            sample_audio(10 * 1024)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn md5_verification_gates_the_finished_file() {
        use md5::Digest as _;
        let audio = sample_audio(1024 * 1024);
        let good = format!("{:x}", md5::Md5::new_with_prefix(&audio).finalize());
        let Some(server) = spawn_test_server(audio.clone(), RangeMode::Supported).await else {
            return;
        };
        let dir = temp_dir("md5").await;
        let progress = DownloadProgress::new();

        let mut bad = request(&server.audio_url, &dir.join("bad.mp3"));
        bad.md5 = Some("00000000000000000000000000000000".to_string());
        let error = engine().download(&bad, &progress).await.unwrap_err();
        assert!(matches!(error, DownloadError::Integrity(_)), "{error}");
        assert!(
            !dir.join("bad.mp3").exists(),
            "a failed integrity check must not leave the file"
        );

        let mut good_req = request(&server.audio_url, &dir.join("good.mp3"));
        good_req.md5 = Some(good);
        engine().download(&good_req, &progress).await.unwrap();
        assert!(dir.join("good.mp3").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fetch_bytes_respects_the_size_limit() {
        let Some(server) = spawn_test_server(sample_audio(4096), RangeMode::Supported).await else {
            return;
        };
        let engine = engine();

        let bytes = engine
            .fetch_bytes(&server.audio_url, &[], 8 * 1024)
            .await
            .unwrap();
        assert_eq!(bytes.len(), 4096);

        let error = engine
            .fetch_bytes(&server.audio_url, &[], 1024)
            .await
            .unwrap_err();
        assert!(matches!(error, DownloadError::Other(_)), "{error}");
    }

    #[tokio::test]
    async fn cancellation_stops_the_download() {
        let Some(server) =
            spawn_test_server(sample_audio(2 * 1024 * 1024), RangeMode::Supported).await
        else {
            return;
        };
        let dir = temp_dir("cancel").await;
        let dest = dir.join("song.mp3");
        let progress = DownloadProgress::new();
        progress.cancel();

        let error = engine()
            .download(&request(&server.audio_url, &dest), &progress)
            .await
            .unwrap_err();
        assert!(matches!(error, DownloadError::Cancelled), "{error}");
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tags_and_lyrics_land_after_the_download() {
        use crate::models::{LyricDocument, LyricFormat};
        let Some(server) = spawn_test_server(sample_audio(1024 * 1024), RangeMode::Supported).await
        else {
            return;
        };
        let dir = temp_dir("tags").await;
        let dest = dir.join("song.mp3");
        let progress = DownloadProgress::new();
        engine()
            .download(&request(&server.audio_url, &dest), &progress)
            .await
            .unwrap();

        // A synthesized PCM WAV stub is the smallest file lofty accepts; here
        // the container is mp3-shaped bytes, which lofty refuses, so only the
        // lyric-file path is exercised against this download.
        let lyric = LyricDocument {
            format: LyricFormat::Lrc,
            original: "[00:01.00]first line\n[00:05.50]second line\n".to_string(),
            translation: Some("[00:01.00]first\n[00:05.50]second\n".to_string()),
            romanization: None,
            lines: Vec::new(),
        };
        let text = standard_lyric(&lyric).unwrap();
        assert!(text.contains("first line"));

        let path =
            write_lyric_file(&dest, &text, Some("[00:01.00]first\n[00:05.50]second\n")).unwrap();
        assert_eq!(path.file_name().unwrap(), "song.lrc");
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(stored.contains("first line"));
        assert!(stored.contains("first\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
