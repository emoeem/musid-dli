//! Unified error taxonomy (M2 core, AUTH_REGISTRY_DESIGN.md §1).
//!
//! The seven pre-M2 variants keep their exact shapes and Display texts. The
//! M2 variants classify first and preserve second: each carries structured,
//! credential-free payloads (provider id, platform code, duration, quality)
//! plus an optional controlled [`ErrorDetail`] slot for the pre-redacted
//! underlying error text (tasks.md §5.5 "保留底层错误信息"; the sanitization
//! contract lives on the type).
use crate::models::StreamQuality;
use thiserror::Error;

/// Controlled carrier for the underlying error text (tasks.md §5.5:
/// "错误应保留底层错误信息，但不应将 Cookie、Token 或签名参数暴露出来").
///
/// Construction contract (AUTH_REGISTRY_DESIGN.md §1.2): this slot holds
/// pre-redacted, short classification texts, e.g. the transport
/// classifier's `Fatal` texts ("HTTP 429", "API code 881: 参数异常") or a
/// call-site label ("stream resolve"). Cookies, tokens, signature
/// parameters, session ids and full request URLs must not enter this slot;
/// the realistic migration texts are pinned by the sanitization tests in
/// this module. Display/Debug print the content on purpose: this is the
/// preserved underlying information, not a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorDetail(String);
impl ErrorDetail {
    /// Wrap a pre-redacted underlying error text. See the type docs for the
    /// sanitization contract.
    pub fn new(redacted: impl Into<String>) -> Self {
        Self(redacted.into())
    }
    /// The redacted underlying text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for ErrorDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ErrorDetail {}

/// Library-wide error type. Provider-specific details remain available in the
/// source field without leaking a concrete HTTP client into the public API.
///
/// The M2 variants (`InvalidResponse` and below) carry structured payloads
/// plus an optional redacted [`ErrorDetail`]; free-form unclassified text
/// remains the job of the legacy `Provider` catch-all.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    // ---- legacy variants (unchanged since v0.1.0; Display is byte-stable) ----
    #[error("provider {provider} failed: {message}")]
    Provider { provider: String, message: String },
    #[error("provider {0} does not support this capability")]
    Unsupported(String),
    #[error("resource not found")]
    NotFound,
    #[error("request timed out")]
    Timeout,
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    // ---- M2 variants (AUTH_REGISTRY_DESIGN.md §1.1) ----
    /// Upstream responded in a way that violates its contract (missing
    /// field, unmapped status code, unparseable payload).
    #[error("invalid response{}{}", code_suffix(.code), detail_suffix(.detail))]
    InvalidResponse {
        provider: Option<String>,
        code: Option<i64>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// Transport-level failure (connection, DNS, TLS).
    #[error("network error{}", detail_suffix(.detail))]
    Network {
        provider: Option<String>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// Rate limited by the platform or an HTTP 429. `retry_after` mirrors a
    /// Retry-After header / platform hint when one was given.
    #[error(
        "rate limited{}{}",
        retry_suffix(.retry_after),
        detail_suffix(.detail)
    )]
    RateLimited {
        provider: Option<String>,
        retry_after: Option<std::time::Duration>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// Provider temporarily unavailable (5xx, maintenance, gateway down).
    #[error("provider unavailable{}", detail_suffix(.detail))]
    Unavailable {
        provider: Option<String>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// The resource existed but has since expired or been taken down.
    #[error("resource expired{}", detail_suffix(.detail))]
    ExpiredResource {
        provider: Option<String>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// Login is required before this call can succeed.
    #[error("login required{}", detail_suffix(.detail))]
    AuthenticationRequired {
        provider: Option<String>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// Credentials were presented and rejected (wrong password, QR 403,
    /// rejected signature).
    #[error(
        "authentication failed{}{}",
        code_suffix(.code),
        detail_suffix(.detail)
    )]
    AuthenticationFailed {
        provider: Option<String>,
        code: Option<i64>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// The stored login session has expired (NetEase/QQ/Kugou
    /// `mark_session_expired` landing spot). Not blindly retryable: recover
    /// through `AuthenticationProvider::refresh_session` or a fresh login.
    #[error("login required or session expired{}", detail_suffix(.detail))]
    SessionExpired {
        provider: Option<String>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// The platform refused the operation (risk control after retries,
    /// insufficient account permissions).
    #[error(
        "permission denied{}{}",
        code_suffix(.code),
        detail_suffix(.detail)
    )]
    PermissionDenied {
        provider: Option<String>,
        code: Option<i64>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// The content requires a paid VIP membership on the platform.
    #[error("vip membership required{}", detail_suffix(.detail))]
    VipRequired {
        provider: Option<String>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// The content is not available in the caller's region.
    #[error("region restricted{}", detail_suffix(.detail))]
    RegionRestricted {
        provider: Option<String>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// The content is blocked by a copyright decision on the platform.
    #[error("copyright restricted{}", detail_suffix(.detail))]
    CopyrightRestricted {
        provider: Option<String>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// The requested quality tier is not offered for this resource.
    #[error(
        "unsupported quality ({quality:?}){}",
        detail_suffix(.detail)
    )]
    UnsupportedQuality {
        provider: Option<String>,
        quality: StreamQuality,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// The current build does not implement this operation at all.
    #[error("unsupported operation{}", detail_suffix(.detail))]
    UnsupportedOperation {
        provider: Option<String>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// The provider itself failed internally (sandbox crash, integrity
    /// check), as opposed to the request being wrong.
    #[error("provider internal error{}", detail_suffix(.detail))]
    ProviderInternal {
        provider: Option<String>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// Protocol-level failure talking to the upstream (M1 transport / LX
    /// bridge contract layer).
    #[error("protocol error{}", detail_suffix(.detail))]
    Protocol {
        provider: Option<String>,
        #[source]
        detail: Option<ErrorDetail>,
    },
    /// The caller cancelled the operation.
    #[error("cancelled")]
    Cancelled,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn code_suffix(code: &Option<i64>) -> String {
    code.map(|c| format!(" (API code {c})")).unwrap_or_default()
}

fn retry_suffix(retry_after: &Option<std::time::Duration>) -> String {
    retry_after
        .map(|d| format!(", retry after {}s", d.as_secs()))
        .unwrap_or_default()
}

fn detail_suffix(detail: &Option<ErrorDetail>) -> String {
    detail
        .as_ref()
        .map(|d| format!(": {d}"))
        .unwrap_or_default()
}

/// Stable error category (AUTH_REGISTRY_DESIGN.md §1.1). One row per
/// `Error` variant; `Error::category()` is the total mapping.
///
/// `Ord` is a structural ordering over the variant declaration order with no
/// semantic meaning (M6: it exists so categories can key a `BTreeMap`, e.g.
/// the per-category negative-cache TTL table, cache.rs `NegativeTtlPolicy`);
/// never compare categories for meaning, use `==`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ErrorCategory {
    AuthenticationRequired,
    AuthenticationFailed,
    SessionExpired,
    PermissionDenied,
    VipRequired,
    RegionRestricted,
    CopyrightRestricted,
    NotFound,
    ExpiredResource,
    Network,
    Timeout,
    RateLimited,
    Unavailable,
    InvalidResponse,
    Protocol,
    InvalidInput,
    Unsupported,
    UnsupportedQuality,
    UnsupportedOperation,
    Cancelled,
    ProviderInternal,
    /// Unclassified catch-all (legacy `Error::Provider`).
    Provider,
    Serialization,
    Io,
}

impl Error {
    /// Category of this error (AUTH_REGISTRY_DESIGN.md §1.1 matrix).
    pub fn category(&self) -> ErrorCategory {
        match self {
            // legacy
            Error::Provider { .. } => ErrorCategory::Provider,
            Error::Unsupported(_) => ErrorCategory::Unsupported,
            Error::NotFound => ErrorCategory::NotFound,
            Error::Timeout => ErrorCategory::Timeout,
            Error::InvalidInput(_) => ErrorCategory::InvalidInput,
            Error::Serialization(_) => ErrorCategory::Serialization,
            Error::Io(_) => ErrorCategory::Io,
            // M2
            Error::InvalidResponse { .. } => ErrorCategory::InvalidResponse,
            Error::Network { .. } => ErrorCategory::Network,
            Error::RateLimited { .. } => ErrorCategory::RateLimited,
            Error::Unavailable { .. } => ErrorCategory::Unavailable,
            Error::ExpiredResource { .. } => ErrorCategory::ExpiredResource,
            Error::AuthenticationRequired { .. } => ErrorCategory::AuthenticationRequired,
            Error::AuthenticationFailed { .. } => ErrorCategory::AuthenticationFailed,
            Error::SessionExpired { .. } => ErrorCategory::SessionExpired,
            Error::PermissionDenied { .. } => ErrorCategory::PermissionDenied,
            Error::VipRequired { .. } => ErrorCategory::VipRequired,
            Error::RegionRestricted { .. } => ErrorCategory::RegionRestricted,
            Error::CopyrightRestricted { .. } => ErrorCategory::CopyrightRestricted,
            Error::UnsupportedQuality { .. } => ErrorCategory::UnsupportedQuality,
            Error::UnsupportedOperation { .. } => ErrorCategory::UnsupportedOperation,
            Error::ProviderInternal { .. } => ErrorCategory::ProviderInternal,
            Error::Protocol { .. } => ErrorCategory::Protocol,
            Error::Cancelled => ErrorCategory::Cancelled,
        }
    }

    /// Registered provider id, when the error carries one.
    pub fn provider_id(&self) -> Option<&str> {
        match self {
            Error::Provider { provider, .. } => Some(provider.as_str()),
            Error::InvalidResponse { provider, .. }
            | Error::Network { provider, .. }
            | Error::RateLimited { provider, .. }
            | Error::Unavailable { provider, .. }
            | Error::ExpiredResource { provider, .. }
            | Error::AuthenticationRequired { provider, .. }
            | Error::AuthenticationFailed { provider, .. }
            | Error::SessionExpired { provider, .. }
            | Error::PermissionDenied { provider, .. }
            | Error::VipRequired { provider, .. }
            | Error::RegionRestricted { provider, .. }
            | Error::CopyrightRestricted { provider, .. }
            | Error::UnsupportedQuality { provider, .. }
            | Error::UnsupportedOperation { provider, .. }
            | Error::ProviderInternal { provider, .. }
            | Error::Protocol { provider, .. } => provider.as_deref(),
            Error::Unsupported(_)
            | Error::NotFound
            | Error::Timeout
            | Error::InvalidInput(_)
            | Error::Serialization(_)
            | Error::Io(_)
            | Error::Cancelled => None,
        }
    }

    /// The preserved (pre-redacted) underlying error text, when the
    /// constructor provided one (tasks.md §5.5). Also reachable through the
    /// `std::error::Error::source()` chain as [`ErrorDetail`].
    pub fn detail(&self) -> Option<&str> {
        match self {
            Error::InvalidResponse { detail, .. }
            | Error::Network { detail, .. }
            | Error::RateLimited { detail, .. }
            | Error::Unavailable { detail, .. }
            | Error::ExpiredResource { detail, .. }
            | Error::AuthenticationRequired { detail, .. }
            | Error::AuthenticationFailed { detail, .. }
            | Error::SessionExpired { detail, .. }
            | Error::PermissionDenied { detail, .. }
            | Error::VipRequired { detail, .. }
            | Error::RegionRestricted { detail, .. }
            | Error::CopyrightRestricted { detail, .. }
            | Error::UnsupportedQuality { detail, .. }
            | Error::UnsupportedOperation { detail, .. }
            | Error::ProviderInternal { detail, .. }
            | Error::Protocol { detail, .. } => detail.as_ref().map(|d| d.as_str()),
            Error::Provider { .. }
            | Error::Unsupported(_)
            | Error::NotFound
            | Error::Timeout
            | Error::InvalidInput(_)
            | Error::Serialization(_)
            | Error::Io(_)
            | Error::Cancelled => None,
        }
    }

    /// Whether a blind resend is a sensible recovery. Only the transport-ish
    /// categories are retryable; `SessionExpired` recovers through
    /// `refresh_session`/re-login instead (`retry.rs` `StepOutcome` parity).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.category(),
            ErrorCategory::Network
                | ErrorCategory::Timeout
                | ErrorCategory::RateLimited
                | ErrorCategory::Unavailable
        )
    }

    /// Default user-facing hint for the category. Hosts localize/override.
    pub fn user_hint(&self) -> &'static str {
        match self.category() {
            ErrorCategory::AuthenticationRequired => "log in to this provider and retry",
            ErrorCategory::AuthenticationFailed => {
                "check your account credentials for this provider and log in again"
            }
            ErrorCategory::SessionExpired => {
                "your session has expired; log in again or refresh the session"
            }
            ErrorCategory::PermissionDenied => {
                "this content requires additional permissions on the platform"
            }
            ErrorCategory::VipRequired => "this content requires a VIP membership on the platform",
            ErrorCategory::RegionRestricted => "this content is not available in your region",
            ErrorCategory::CopyrightRestricted => {
                "this content is restricted by copyright on the platform"
            }
            ErrorCategory::NotFound => "the requested resource does not exist",
            ErrorCategory::ExpiredResource => "the resource has expired or been removed",
            ErrorCategory::Network => "check your network connection and retry",
            ErrorCategory::Timeout => "the request timed out; retry later",
            ErrorCategory::RateLimited => "too many requests; wait a moment and retry",
            ErrorCategory::Unavailable => "the provider is temporarily unavailable; retry later",
            ErrorCategory::InvalidResponse => {
                "the provider returned an unexpected response; retry later or report this"
            }
            ErrorCategory::Protocol => "a protocol error occurred while talking to the provider",
            ErrorCategory::InvalidInput => "check the request parameters",
            ErrorCategory::Unsupported => "this capability is not supported by the provider",
            ErrorCategory::UnsupportedQuality => {
                "the requested quality is not available; try a lower quality"
            }
            ErrorCategory::UnsupportedOperation => "this operation is not supported",
            ErrorCategory::Cancelled => "the operation was cancelled",
            ErrorCategory::ProviderInternal => {
                "the provider failed internally; retry later or report this"
            }
            ErrorCategory::Provider => "unclassified provider failure; see the error message",
            ErrorCategory::Serialization => "local data could not be read",
            ErrorCategory::Io => "a local I/O error occurred",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;
    use std::time::Duration;

    /// One row per `Error` variant (all 24): expected category, retryability,
    /// provider id and a Display fragment. Adding a variant without a row
    /// here does not compile-fail (the enum is `#[non_exhaustive]`), but
    /// `category()`/`user_hint()` exhaustiveness does force the mapping; this
    /// table pins the *values* (AUTH_REGISTRY_DESIGN.md §1.1).
    fn matrix() -> Vec<(
        Error,
        ErrorCategory,
        bool,
        Option<&'static str>,
        &'static str,
    )> {
        let pid = || Some("netease".to_owned());
        vec![
            // legacy seven
            (
                Error::Provider {
                    provider: "netease".into(),
                    message: "boom".into(),
                },
                ErrorCategory::Provider,
                false,
                Some("netease"),
                "provider netease failed: boom",
            ),
            (
                Error::Unsupported("netease".into()),
                ErrorCategory::Unsupported,
                false,
                None,
                "provider netease does not support this capability",
            ),
            (
                Error::NotFound,
                ErrorCategory::NotFound,
                false,
                None,
                "resource not found",
            ),
            (
                Error::Timeout,
                ErrorCategory::Timeout,
                true,
                None,
                "request timed out",
            ),
            (
                Error::InvalidInput("bad id".into()),
                ErrorCategory::InvalidInput,
                false,
                None,
                "invalid input: bad id",
            ),
            (
                Error::Serialization(serde_json::from_str::<usize>("{").unwrap_err()),
                ErrorCategory::Serialization,
                false,
                None,
                "serialization error:",
            ),
            (
                Error::Io(std::io::Error::other("disk")),
                ErrorCategory::Io,
                false,
                None,
                "I/O error:",
            ),
            // M2 seventeen
            (
                Error::InvalidResponse {
                    provider: pid(),
                    code: Some(881),
                    detail: Some(ErrorDetail::new("search: API code 881: 参数异常")),
                },
                ErrorCategory::InvalidResponse,
                false,
                Some("netease"),
                "invalid response (API code 881): search: API code 881: 参数异常",
            ),
            (
                Error::Network {
                    provider: pid(),
                    detail: None,
                },
                ErrorCategory::Network,
                true,
                Some("netease"),
                "network error",
            ),
            (
                Error::RateLimited {
                    provider: pid(),
                    retry_after: Some(Duration::from_secs(30)),
                    detail: None,
                },
                ErrorCategory::RateLimited,
                true,
                Some("netease"),
                "rate limited, retry after 30s",
            ),
            (
                Error::RateLimited {
                    provider: pid(),
                    retry_after: None,
                    detail: Some(ErrorDetail::new("stream resolve: HTTP 429")),
                },
                ErrorCategory::RateLimited,
                true,
                Some("netease"),
                "rate limited: stream resolve: HTTP 429",
            ),
            (
                Error::Unavailable {
                    provider: pid(),
                    detail: None,
                },
                ErrorCategory::Unavailable,
                true,
                Some("netease"),
                "provider unavailable",
            ),
            (
                Error::ExpiredResource {
                    provider: pid(),
                    detail: None,
                },
                ErrorCategory::ExpiredResource,
                false,
                Some("netease"),
                "resource expired",
            ),
            (
                Error::AuthenticationRequired {
                    provider: pid(),
                    detail: None,
                },
                ErrorCategory::AuthenticationRequired,
                false,
                Some("netease"),
                "login required",
            ),
            (
                Error::AuthenticationFailed {
                    provider: pid(),
                    code: Some(-403),
                    detail: None,
                },
                ErrorCategory::AuthenticationFailed,
                false,
                Some("netease"),
                "authentication failed (API code -403)",
            ),
            (
                Error::SessionExpired {
                    provider: pid(),
                    detail: None,
                },
                ErrorCategory::SessionExpired,
                false,
                Some("netease"),
                "login required or session expired",
            ),
            (
                Error::PermissionDenied {
                    provider: pid(),
                    code: Some(-412),
                    detail: None,
                },
                ErrorCategory::PermissionDenied,
                false,
                Some("netease"),
                "permission denied (API code -412)",
            ),
            (
                Error::VipRequired {
                    provider: pid(),
                    detail: None,
                },
                ErrorCategory::VipRequired,
                false,
                Some("netease"),
                "vip membership required",
            ),
            (
                Error::RegionRestricted {
                    provider: pid(),
                    detail: None,
                },
                ErrorCategory::RegionRestricted,
                false,
                Some("netease"),
                "region restricted",
            ),
            (
                Error::CopyrightRestricted {
                    provider: pid(),
                    detail: None,
                },
                ErrorCategory::CopyrightRestricted,
                false,
                Some("netease"),
                "copyright restricted",
            ),
            (
                Error::UnsupportedQuality {
                    provider: pid(),
                    quality: StreamQuality::HiRes,
                    detail: None,
                },
                ErrorCategory::UnsupportedQuality,
                false,
                Some("netease"),
                "unsupported quality (HiRes)",
            ),
            (
                Error::UnsupportedOperation {
                    provider: pid(),
                    detail: None,
                },
                ErrorCategory::UnsupportedOperation,
                false,
                Some("netease"),
                "unsupported operation",
            ),
            (
                Error::ProviderInternal {
                    provider: pid(),
                    detail: None,
                },
                ErrorCategory::ProviderInternal,
                false,
                Some("netease"),
                "provider internal error",
            ),
            (
                Error::Protocol {
                    provider: pid(),
                    detail: None,
                },
                ErrorCategory::Protocol,
                false,
                Some("netease"),
                "protocol error",
            ),
            (
                Error::Cancelled,
                ErrorCategory::Cancelled,
                false,
                None,
                "cancelled",
            ),
        ]
    }

    #[test]
    fn error_mapping_matrix_is_total_and_classified() {
        for (error, category, retryable, provider, display) in matrix() {
            assert_eq!(error.category(), category, "{error:?}");
            assert_eq!(error.is_retryable(), retryable, "{error:?}");
            assert_eq!(error.provider_id(), provider, "{error:?}");
            assert!(!error.user_hint().trim().is_empty(), "{error:?}");
            assert!(
                error.to_string().starts_with(display) || error.to_string().contains(display),
                "Display {:?} missing fragment {display:?}",
                error.to_string()
            );
        }
        // Categories are distinct labels for the matrix rows above.
        let categories: std::collections::HashSet<_> =
            matrix().into_iter().map(|(_, c, ..)| c).collect();
        assert!(categories.len() >= 20);
    }

    #[test]
    fn new_variant_debug_never_contains_credential_material() {
        let markers = ["cookie", "token", "secret", "sign", "password"];
        for (error, ..) in matrix() {
            let debug = format!("{error:?}").to_lowercase();
            let display = error.to_string().to_lowercase();
            for marker in markers {
                assert!(!debug.contains(marker), "Debug leaks {marker:?}: {debug}");
                assert!(
                    !display.contains(marker),
                    "Display leaks {marker:?}: {display}"
                );
            }
            assert!(!error.user_hint().to_lowercase().contains("token"));
        }
    }

    #[test]
    fn detail_slot_preserves_underlying_text_per_tasks_md_5_5() {
        // Realistic texts the §8 provider slices must preserve when leaving
        // the perr("{label}: {transport}") catch-all (source_netease.rs:147,
        // source_kugou.rs:185, source_qqmusic.rs:251/308).
        let cases: Vec<Error> = vec![
            Error::Network {
                provider: Some("netease".into()),
                detail: Some(ErrorDetail::new("song details: HTTP 502")),
            },
            Error::RateLimited {
                provider: Some("netease".into()),
                retry_after: None,
                detail: Some(ErrorDetail::new("stream resolve: HTTP 429")),
            },
            Error::InvalidResponse {
                provider: Some("netease".into()),
                code: Some(881),
                detail: Some(ErrorDetail::new("search: API code 881: 参数异常")),
            },
            Error::Protocol {
                provider: Some("qqmusic".into()),
                detail: Some(ErrorDetail::new("login client: HTTP 302 without Location")),
            },
        ];
        for error in &cases {
            // Preserved in the source() chain...
            let source = error.source().expect("detail must be wired as #[source]");
            let detail = source
                .downcast_ref::<ErrorDetail>()
                .expect("source chain carries the ErrorDetail");
            assert_eq!(detail.as_str(), error.detail().unwrap());
            // ...in the Display text...
            let display = error.to_string();
            assert!(
                display.ends_with(&format!(": {}", detail.as_str())),
                "Display must append the detail: {display}"
            );
            // ...and free of credential material.
            let lowered = display.to_lowercase();
            for marker in ["cookie", "token", "secret", "sign", "password"] {
                assert!(
                    !lowered.contains(marker),
                    "detail leaked {marker:?}: {display}"
                );
            }
        }
        // Details are optional: absent detail yields no accessor hit.
        assert_eq!(
            Error::Network {
                provider: None,
                detail: None
            }
            .detail(),
            None
        );
    }
}
