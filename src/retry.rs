//! Shared request retry/backoff policy for provider request loops.
//!
//! Compiled once for all providers; individual feature combinations may not
//! use every helper, so dead-code linting is relaxed module-wide.
#![allow(dead_code)]
//!
//! Both the NetEase and QQ Music providers drive HTTP request loops with the
//! same shape: retry transient transport/status failures with backoff, mark
//! sessions expired on login-expiry API codes, and fail fast on everything
//! else. The policy lives here as pure functions so it can be unit tested
//! without a network.

/// Outcome of one request/response cycle inside a retry loop.
#[derive(Debug, PartialEq, Eq)]
pub enum StepOutcome {
    /// Success; stop retrying and use the response.
    Ok,
    /// Transient failure; sleep the current backoff delay and try again.
    Retry,
    /// API signalled login expiry; mark the session expired and fail.
    SessionExpired,
    /// Permanent failure with a ready-to-report message.
    Fatal(String),
}

/// Provider-specific API status-code semantics.
pub struct ApiCodes {
    /// Code that means "request succeeded" (NetEase uses 200, QQ uses 0).
    pub ok: i64,
    /// Codes that mean the login session is expired or missing.
    pub expired: &'static [i64],
    /// Code that means throttled and worth retrying after backoff, if any.
    pub throttled: Option<i64>,
}

#[cfg(feature = "source-netease")]
pub const NETEASE_CODES: ApiCodes = ApiCodes {
    ok: 200,
    expired: &[301, 512],
    throttled: Some(405),
};

#[cfg(feature = "source-qqmusic")]
pub const QQ_CODES: ApiCodes = ApiCodes {
    ok: 0,
    expired: &[1000, 301],
    throttled: None,
};

/// Kugou body-code semantics, pinned by the §8 slice 3 code-table inventory
/// (AUTH_REGISTRY_DESIGN.md §9.2): neither the voicefox `lx-source::kg`
/// upstream nor the reconstructed `tests/fixtures/kugou` set shows any
/// `error_code` value with login-expiry semantics — the only code-bearing
/// faces are the QR poll's `data.status` field (`-1`/`5`/`6` mean the QR
/// *code* expired, not the session) and search's bare `error_code != 0`
/// check, and no fixture pins a body `error_code` as session-expiry
/// (tasks.md §15.4: fixture-less codes are not claimed live-verified).
/// Session expiry therefore rides the provider-written `login_expired`
/// marker (`KugouProvider::mark_session_expired`), and the empty
/// `expired` table keeps every non-ok body code terminal/Fatal.
#[cfg(feature = "source-kugou")]
pub const KUGOU_CODES: ApiCodes = ApiCodes {
    ok: 0,
    expired: &[],
    throttled: None,
};

#[cfg(feature = "source-kuwo")]
pub const KUWO_CODES: ApiCodes = ApiCodes {
    ok: 200,
    expired: &[],
    throttled: None,
};

/// HTTP-level classification, applied before the body is parsed.
///
/// `attempt` is the zero-based loop index and `budget` the number of backoff
/// entries, so the final iteration (`attempt == budget`) never retries.
pub fn classify_http(status: u16, attempt: usize, budget: usize) -> StepOutcome {
    if status == 429 || (500..600).contains(&status) {
        return if attempt < budget {
            StepOutcome::Retry
        } else {
            StepOutcome::Fatal(format!("HTTP {status}"))
        };
    }
    if !(200..300).contains(&status) {
        return StepOutcome::Fatal(format!("HTTP {status}"));
    }
    StepOutcome::Ok
}

/// API-level classification, applied after the JSON body has been parsed.
pub fn classify_api(
    code: i64,
    message: &str,
    attempt: usize,
    budget: usize,
    codes: &ApiCodes,
) -> StepOutcome {
    if code == codes.ok {
        return StepOutcome::Ok;
    }
    if codes.expired.contains(&code) {
        return StepOutcome::SessionExpired;
    }
    if Some(code) == codes.throttled {
        return if attempt < budget {
            StepOutcome::Retry
        } else {
            StepOutcome::Fatal(format!("API code {code}: throttled"))
        };
    }
    StepOutcome::Fatal(format!("API code {code}: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_classification_retries_transient_and_fails_permanent() {
        let budget = 4;
        assert_eq!(classify_http(429, 0, budget), StepOutcome::Retry);
        assert_eq!(classify_http(503, 1, budget), StepOutcome::Retry);
        assert_eq!(
            classify_http(429, budget, budget),
            StepOutcome::Fatal("HTTP 429".into())
        );
        assert_eq!(
            classify_http(404, 0, budget),
            StepOutcome::Fatal("HTTP 404".into())
        );
        assert_eq!(classify_http(200, 0, budget), StepOutcome::Ok);
        assert_eq!(classify_http(201, budget, budget), StepOutcome::Ok);
    }

    #[test]
    #[cfg(feature = "source-netease")]
    fn netease_api_codes_keep_netease_semantics() {
        let budget = 4;
        assert_eq!(
            classify_api(301, "", 0, budget, &NETEASE_CODES),
            StepOutcome::SessionExpired
        );
        assert_eq!(
            classify_api(512, "", 0, budget, &NETEASE_CODES),
            StepOutcome::SessionExpired
        );
        assert_eq!(
            classify_api(405, "", 1, budget, &NETEASE_CODES),
            StepOutcome::Retry
        );
        assert_eq!(
            classify_api(405, "", budget, budget, &NETEASE_CODES),
            StepOutcome::Fatal("API code 405: throttled".into())
        );
        assert_eq!(
            classify_api(200, "", 0, budget, &NETEASE_CODES),
            StepOutcome::Ok
        );
        assert_eq!(
            classify_api(881, "参数异常", 0, budget, &NETEASE_CODES),
            StepOutcome::Fatal("API code 881: 参数异常".into())
        );
    }

    #[test]
    #[cfg(feature = "source-qqmusic")]
    fn qq_api_codes_use_zero_ok_and_login_expiry() {
        let budget = 4;
        assert_eq!(classify_api(0, "", 0, budget, &QQ_CODES), StepOutcome::Ok);
        assert_eq!(
            classify_api(1000, "", 0, budget, &QQ_CODES),
            StepOutcome::SessionExpired
        );
        assert_eq!(
            classify_api(301, "", 0, budget, &QQ_CODES),
            StepOutcome::SessionExpired
        );
        assert_eq!(
            classify_api(55, "quota exceeded", 0, budget, &QQ_CODES),
            StepOutcome::Fatal("API code 55: quota exceeded".into())
        );
    }

    /// Pins the §8 slice 3 decision: no discernible expired code, so the
    /// classifier hands kugou no `SessionExpired` verdict at all — session
    /// expiry rides the provider's `login_expired` marker instead
    /// (AUTH_REGISTRY_DESIGN.md §9.2). 31100 is the observed fatal body code
    /// from the kugou fixture set, used here as the representative non-ok
    /// value.
    #[test]
    #[cfg(feature = "source-kugou")]
    fn kugou_api_codes_pin_the_no_discernible_expired_code_decision() {
        let budget = 4;
        assert_eq!(KUGOU_CODES.ok, 0);
        assert!(
            KUGOU_CODES.expired.is_empty(),
            "code-table inventory found no fixture-backed expiry code"
        );
        assert_eq!(KUGOU_CODES.throttled, None);
        assert_eq!(
            classify_api(0, "", 0, budget, &KUGOU_CODES),
            StepOutcome::Ok
        );
        assert_eq!(
            classify_api(31100, "参数异常", 0, budget, &KUGOU_CODES),
            StepOutcome::Fatal("API code 31100: 参数异常".into())
        );
    }
}
