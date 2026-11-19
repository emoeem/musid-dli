//! Provider circuit breaker used by search/stream orchestration, plus the
//! one-shot connectivity probe result (G10) and the M6 failure-class
//! classification (RESOURCE_DESIGN.md §4).
use crate::error::ErrorCategory;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy)]
pub struct ProviderHealthConfig {
    pub failure_threshold: u32,
    pub cooldown: Duration,
}
impl Default for ProviderHealthConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            cooldown: Duration::from_secs(30),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderHealthSnapshot {
    pub state: CircuitState,
    pub consecutive_failures: u32,
    /// Per-class failure counters since the last success/reset
    /// (RESOURCE_DESIGN.md §4.2). `consecutive_failures` stays the
    /// breaker-driving total; the class split is observability only and
    /// carries no provider text, URL or credential material.
    pub failures_by_class: HealthFailureCounts,
}

/// Health failure class (RESOURCE_DESIGN.md §4.1): the 不可达/限流/认证失效/
/// 维护中/未知 split, derived from the M2 `ErrorCategory`. Counters only;
/// no message text ever enters this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HealthFailureClass {
    /// Transport-level failure to reach the platform: Network, Timeout.
    /// Local I/O faults (`Io`) are deliberately NOT here: they describe the
    /// caller's machine, not the provider's reachability.
    Unreachable,
    /// The platform rate-limited us: RateLimited.
    RateLimited,
    /// Credential state: SessionExpired, AuthenticationRequired,
    /// AuthenticationFailed. A session event, not provider degradation
    /// (§4.2: does not advance the breaker total, but releases a failed
    /// half-open probe so the state machine can never wedge).
    AuthInvalid,
    /// Provider temporarily unavailable / maintenance: Unavailable.
    Maintenance,
    /// Everything else (including the legacy unclassified `Provider` and
    /// local `Io` faults).
    Unknown,
}

impl HealthFailureClass {
    /// The total M2 mapping (RESOURCE_DESIGN.md §4.1), one row per
    /// `ErrorCategory` variant (all 24). `Unavailable` (error.rs:97-103) is
    /// the only maintenance source; `ProviderInternal` stays Unknown (a
    /// provider bug is not maintenance); `Io` stays Unknown (a local fault
    /// is not the provider being unreachable).
    pub fn from_category(category: ErrorCategory) -> Self {
        match category {
            ErrorCategory::Network | ErrorCategory::Timeout => Self::Unreachable,
            ErrorCategory::RateLimited => Self::RateLimited,
            ErrorCategory::SessionExpired
            | ErrorCategory::AuthenticationRequired
            | ErrorCategory::AuthenticationFailed => Self::AuthInvalid,
            ErrorCategory::Unavailable => Self::Maintenance,
            _ => Self::Unknown,
        }
    }
}

/// Per-class failure counters (RESOURCE_DESIGN.md §4.2). Copy struct of five
/// `u32`s; additive over the pre-M6 snapshot, which carries no serde wire
/// format today.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HealthFailureCounts {
    pub unreachable: u32,
    pub rate_limited: u32,
    pub auth_invalid: u32,
    pub maintenance: u32,
    pub unknown: u32,
}
impl HealthFailureCounts {
    fn record(&mut self, class: HealthFailureClass) {
        match class {
            HealthFailureClass::Unreachable => self.unreachable += 1,
            HealthFailureClass::RateLimited => self.rate_limited += 1,
            HealthFailureClass::AuthInvalid => self.auth_invalid += 1,
            HealthFailureClass::Maintenance => self.maintenance += 1,
            HealthFailureClass::Unknown => self.unknown += 1,
        }
    }
}

/// One-shot connectivity probe result (G10, MODEL_ALIGNMENT_DESIGN.md §6.4;
/// ported shape of voicefox `SourceHealth`, `core/src/model/source.rs:5-12`,
/// whose live probe is one 8s-timeout search per source,
/// `source/src/manager.rs:1230-1264`). Deliberately independent of
/// [`ProviderHealthSnapshot`]: a probe is one live request with its own
/// latency/result count/detail, the snapshot is the circuit-breaker state —
/// different lifetimes and consumers, so neither extends the other.
///
/// `result_count` is `Option`: voicefox's probe is always a search so its
/// `SourceHealth.result_count` is a plain `u32`, while musid probe paths that
/// do not search have no count to report. `#[serde(default)]` keeps a JSON
/// without the key loadable as `None` (§1 R1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProbeResult {
    /// Provider id the probe ran against (voicefox carries `id` + `name`;
    /// the id is the registry key, the display name stays provider-side).
    pub provider: String,
    /// Whether the probe request succeeded.
    pub ok: bool,
    /// Wall-clock latency of the probe request in milliseconds (voicefox
    /// clamps to `u64` via `min(u64::MAX)`; same convention).
    pub latency_ms: u64,
    /// Probe search result count, `None` when the probe path has no search
    /// result to count (voicefox `source.rs:9`).
    #[serde(default)]
    pub result_count: Option<u32>,
    /// Human-readable probe outcome (voicefox: "搜索正常", the timeout text,
    /// or the error string).
    pub detail: String,
    /// Probe attempts made for this result, including retries, when the
    /// probe path counts them (`None` = not tracked; M6, RESOURCE_DESIGN.md
    /// §4.3, serde default keeps old probe JSON loadable, §1 R1).
    #[serde(default)]
    pub attempts: Option<u32>,
    /// A Retry-After hint the platform gave, in milliseconds (`None` when
    /// none; serde default as above).
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
}
enum InternalState {
    Closed,
    Open { until: Instant },
    HalfOpen { probe_in_flight: bool },
}
struct Entry {
    state: InternalState,
    failures: u32,
    /// Per-class counters since the last success/reset (§4.2). Recorded for
    /// observability; only the `failures` total drives the breaker.
    counts: HealthFailureCounts,
}
pub struct ProviderHealthMonitor {
    config: ProviderHealthConfig,
    entries: Mutex<HashMap<String, Entry>>,
}
impl ProviderHealthMonitor {
    pub fn new(config: ProviderHealthConfig) -> Self {
        Self {
            config: ProviderHealthConfig {
                failure_threshold: config.failure_threshold.max(1),
                ..config
            },
            entries: Mutex::new(HashMap::new()),
        }
    }
    /// Acquire permission to call a provider. Only one half-open probe is allowed.
    pub fn try_acquire(&self, provider: &str) -> bool {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.entry(provider.to_owned()).or_insert(Entry {
            state: InternalState::Closed,
            failures: 0,
            counts: HealthFailureCounts::default(),
        });
        match entry.state {
            InternalState::Closed => true,
            InternalState::Open { until } if Instant::now() >= until => {
                entry.state = InternalState::HalfOpen {
                    probe_in_flight: true,
                };
                true
            }
            InternalState::Open { .. } => false,
            InternalState::HalfOpen {
                probe_in_flight: false,
            } => {
                entry.state = InternalState::HalfOpen {
                    probe_in_flight: true,
                };
                true
            }
            InternalState::HalfOpen {
                probe_in_flight: true,
            } => false,
        }
    }
    pub fn record_success(&self, provider: &str) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.entry(provider.to_owned()).or_insert(Entry {
            state: InternalState::Closed,
            failures: 0,
            counts: HealthFailureCounts::default(),
        });
        entry.failures = 0;
        // Class counters cover the window since the last success/reset
        // (RESOURCE_DESIGN.md §4.2).
        entry.counts = HealthFailureCounts::default();
        entry.state = InternalState::Closed;
    }
    /// Legacy failure recording, kept byte-for-byte: delegates to the
    /// class-aware primitive with `Unknown` (RESOURCE_DESIGN.md §4.2), so
    /// every pre-M6 caller and test behaves identically.
    pub fn record_failure(&self, provider: &str) {
        self.record_failure_with_class(provider, HealthFailureClass::Unknown);
    }
    /// Class-aware failure recording with the M2 category as input
    /// (RESOURCE_DESIGN.md §4.2). Call sites that hold an `Error` pass its
    /// `category()` here instead of compressing to a string first.
    pub fn record_failure_with_category(&self, provider: &str, category: ErrorCategory) {
        self.record_failure_with_class(provider, HealthFailureClass::from_category(category));
    }
    /// Class-aware failure recording (RESOURCE_DESIGN.md §4.1/§4.2).
    /// `AuthInvalid` is a session event, not provider degradation: it is
    /// counted in the class table, does not increment the breaker total and
    /// does not open a Closed circuit (the recovery path is
    /// `AuthenticationProvider::refresh_session`, which a tripped circuit
    /// would block). It is NOT a free pass through the state machine,
    /// though: a failed half-open probe is released back to
    /// `Open { cooldown }`, otherwise `probe_in_flight` would stick `true`
    /// forever and `try_acquire` would never admit the provider again (a
    /// session expiring during the probe is the normal netease/qq case).
    /// Every other class keeps the exact breaker logic of the legacy
    /// `record_failure`.
    pub fn record_failure_with_class(&self, provider: &str, class: HealthFailureClass) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.entry(provider.to_owned()).or_insert(Entry {
            state: InternalState::Closed,
            failures: 0,
            counts: HealthFailureCounts::default(),
        });
        entry.counts.record(class);
        if class == HealthFailureClass::AuthInvalid {
            // Release a stuck half-open probe; leave Closed/Open untouched.
            if matches!(entry.state, InternalState::HalfOpen { .. }) {
                entry.state = InternalState::Open {
                    until: Instant::now() + self.config.cooldown,
                };
            }
            return;
        }
        entry.failures = entry.failures.saturating_add(1);
        if matches!(entry.state, InternalState::HalfOpen { .. })
            || entry.failures >= self.config.failure_threshold
        {
            entry.state = InternalState::Open {
                until: Instant::now() + self.config.cooldown,
            };
        } else {
            entry.state = InternalState::Closed;
        }
    }
    pub fn snapshot(&self, provider: &str) -> ProviderHealthSnapshot {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = entries.get_mut(provider) else {
            return ProviderHealthSnapshot {
                state: CircuitState::Closed,
                consecutive_failures: 0,
                failures_by_class: HealthFailureCounts::default(),
            };
        };
        let state = match entry.state {
            InternalState::Closed => CircuitState::Closed,
            InternalState::Open { until } if Instant::now() >= until => CircuitState::HalfOpen,
            InternalState::Open { .. } => CircuitState::Open,
            InternalState::HalfOpen { .. } => CircuitState::HalfOpen,
        };
        ProviderHealthSnapshot {
            state,
            consecutive_failures: entry.failures,
            failures_by_class: entry.counts,
        }
    }
    /// Drop all recorded state for `provider`; the next snapshot is a fresh
    /// Closed with zero failures. The registry calls this when a provider id
    /// leaves the registration surface (`unregister_all`) so a later
    /// re-registration does not inherit the old circuit history
    /// (AUTH_REGISTRY_DESIGN.md §5.5).
    pub fn reset(&self, provider: &str) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.remove(provider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn circuit_opens_then_allows_one_probe_and_recovers() {
        let monitor = ProviderHealthMonitor::new(ProviderHealthConfig {
            failure_threshold: 2,
            cooldown: Duration::from_millis(2),
        });
        monitor.record_failure("netease");
        assert!(monitor.try_acquire("netease"));
        monitor.record_failure("netease");
        assert_eq!(monitor.snapshot("netease").state, CircuitState::Open);
        assert!(!monitor.try_acquire("netease"));
        std::thread::sleep(Duration::from_millis(3));
        assert!(monitor.try_acquire("netease"));
        assert!(!monitor.try_acquire("netease"));
        monitor.record_success("netease");
        assert_eq!(monitor.snapshot("netease").state, CircuitState::Closed);
        assert_eq!(monitor.snapshot("netease").consecutive_failures, 0);
    }

    /// §1 R1/R8: a fully populated probe result round-trips field by field
    /// and the wire keys match the voicefox `SourceHealth` names it ports
    /// (`source.rs:5-12`, with `id`+`name` folded into `provider`).
    #[test]
    fn probe_result_round_trip_preserves_every_field() {
        let probe = ProviderProbeResult {
            provider: "netease".into(),
            ok: true,
            latency_ms: 123,
            result_count: Some(1),
            detail: "搜索正常".into(),
            attempts: Some(2),
            retry_after_ms: Some(1500),
        };
        let value = serde_json::to_value(&probe).unwrap();
        let object = value.as_object().unwrap();
        for key in [
            "provider",
            "ok",
            "latency_ms",
            "result_count",
            "detail",
            "attempts",
            "retry_after_ms",
        ] {
            assert!(object.contains_key(key), "missing wire key {key}");
        }
        assert_eq!(
            serde_json::from_value::<ProviderProbeResult>(value).unwrap(),
            probe
        );
    }

    /// G10: a probe JSON without `result_count` (a non-search probe path)
    /// loads with the documented `None` default (§1 R1). The M6 additive
    /// fields default the same way.
    #[test]
    fn probe_result_json_without_result_count_loads_with_none_default() {
        let probe: ProviderProbeResult = serde_json::from_str(
            r#"{"provider":"kw","ok":false,"latency_ms":8000,"detail":"请求超时（8 秒）"}"#,
        )
        .unwrap();
        assert_eq!(probe.provider, "kw");
        assert!(!probe.ok);
        assert_eq!(probe.latency_ms, 8000);
        assert_eq!(probe.result_count, None);
        assert_eq!(probe.detail, "请求超时（8 秒）");
        assert_eq!(probe.attempts, None);
        assert_eq!(probe.retry_after_ms, None);
    }

    /// M6 §4.1: the class mapping is total over the M2 taxonomy, pinned per
    /// category (exactly one row per `ErrorCategory` variant, all 24; the
    /// mapping is the interface between the M2 taxonomy and the health
    /// counters).
    #[test]
    fn health_failure_classes_map_from_categories() {
        use ErrorCategory::*;
        let expected: &[(ErrorCategory, HealthFailureClass)] = &[
            (Network, HealthFailureClass::Unreachable),
            (Timeout, HealthFailureClass::Unreachable),
            (RateLimited, HealthFailureClass::RateLimited),
            (SessionExpired, HealthFailureClass::AuthInvalid),
            (AuthenticationRequired, HealthFailureClass::AuthInvalid),
            (AuthenticationFailed, HealthFailureClass::AuthInvalid),
            (Unavailable, HealthFailureClass::Maintenance),
            // Everything else is Unknown: the M2 matrix minus the rows above.
            // Io is a local fault, not the provider being unreachable.
            (Provider, HealthFailureClass::Unknown),
            (Io, HealthFailureClass::Unknown),
            (PermissionDenied, HealthFailureClass::Unknown),
            (VipRequired, HealthFailureClass::Unknown),
            (RegionRestricted, HealthFailureClass::Unknown),
            (CopyrightRestricted, HealthFailureClass::Unknown),
            (NotFound, HealthFailureClass::Unknown),
            (ExpiredResource, HealthFailureClass::Unknown),
            (InvalidResponse, HealthFailureClass::Unknown),
            (Protocol, HealthFailureClass::Unknown),
            (InvalidInput, HealthFailureClass::Unknown),
            (Unsupported, HealthFailureClass::Unknown),
            (UnsupportedQuality, HealthFailureClass::Unknown),
            (UnsupportedOperation, HealthFailureClass::Unknown),
            (Cancelled, HealthFailureClass::Unknown),
            (ProviderInternal, HealthFailureClass::Unknown),
            (Serialization, HealthFailureClass::Unknown),
        ];
        // Exactly one row per category, no duplicates, nothing missed: 24
        // distinct `ErrorCategory` variants in, 24 rows out.
        let distinct: std::collections::HashSet<_> =
            expected.iter().map(|(category, _)| *category).collect();
        assert_eq!(expected.len(), 24, "one row per M2 category variant");
        assert_eq!(distinct.len(), 24, "no duplicate rows");
        for (category, class) in expected {
            assert_eq!(
                &HealthFailureClass::from_category(*category),
                class,
                "{category:?}"
            );
        }
    }

    /// M6 §4.2: the class counters fill per class, reset on success, and the
    /// legacy `record_failure` lands in `Unknown` exactly as before.
    #[test]
    fn failure_class_counters_fill_reset_and_default_to_unknown() {
        let monitor = ProviderHealthMonitor::new(ProviderHealthConfig::default());
        monitor.record_failure_with_category("p", ErrorCategory::RateLimited);
        monitor.record_failure_with_category("p", ErrorCategory::Network);
        monitor.record_failure_with_category("p", ErrorCategory::Unavailable);
        let snapshot = monitor.snapshot("p");
        assert_eq!(snapshot.failures_by_class.rate_limited, 1);
        assert_eq!(snapshot.failures_by_class.unreachable, 1);
        assert_eq!(snapshot.failures_by_class.maintenance, 1);
        assert_eq!(
            snapshot.consecutive_failures, 3,
            "every class drives the breaker"
        );
        monitor.record_success("p");
        let snapshot = monitor.snapshot("p");
        assert_eq!(snapshot.failures_by_class, HealthFailureCounts::default());
        // Legacy path: Unknown class, breaker-driving.
        monitor.record_failure("p");
        let snapshot = monitor.snapshot("p");
        assert_eq!(snapshot.failures_by_class.unknown, 1);
        assert_eq!(snapshot.consecutive_failures, 1);
        // An unknown provider snapshots to zeroed defaults.
        let fresh = monitor.snapshot("never-seen");
        assert_eq!(fresh.failures_by_class, HealthFailureCounts::default());
        assert_eq!(fresh.consecutive_failures, 0);
    }

    /// M6 §4.2: an AuthInvalid failure is a session event, not provider
    /// degradation. It must be visible in the class counters but must never
    /// trip the circuit (the recovery is `refresh_session`, which a tripped
    /// breaker would block).
    #[test]
    fn auth_invalid_failures_do_not_trip_the_circuit() {
        let monitor = ProviderHealthMonitor::new(ProviderHealthConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(30),
        });
        monitor.record_failure_with_category("p", ErrorCategory::SessionExpired);
        monitor.record_failure_with_category("p", ErrorCategory::AuthenticationFailed);
        monitor.record_failure_with_category("p", ErrorCategory::AuthenticationRequired);
        let snapshot = monitor.snapshot("p");
        assert_eq!(snapshot.failures_by_class.auth_invalid, 3);
        assert_eq!(
            snapshot.consecutive_failures, 0,
            "auth events stay out of the breaker"
        );
        assert_eq!(snapshot.state, CircuitState::Closed);
        assert!(
            monitor.try_acquire("p"),
            "circuit stays closed for auth failures"
        );
        // A real failure still trips it while the auth counters keep history.
        monitor.record_failure_with_category("p", ErrorCategory::Network);
        monitor.record_failure_with_category("p", ErrorCategory::Network);
        assert_eq!(monitor.snapshot("p").state, CircuitState::Open);
        assert_eq!(monitor.snapshot("p").failures_by_class.auth_invalid, 3);
        assert_eq!(monitor.snapshot("p").failures_by_class.unreachable, 2);
    }

    /// M6 §4.2 (review P1): an auth-class failure of the half-open probe
    /// must release the state machine back to `Open { cooldown }`. Without
    /// the release, `probe_in_flight` sticks `true` forever and
    /// `try_acquire` never admits the provider again: a session expiring
    /// during the probe (the normal netease/qq case) would wedge the
    /// breaker until process restart.
    #[test]
    fn half_open_auth_failure_releases_the_probe_instead_of_wedging() {
        let monitor = ProviderHealthMonitor::new(ProviderHealthConfig {
            failure_threshold: 2,
            cooldown: Duration::from_millis(30),
        });
        monitor.record_failure("p");
        monitor.record_failure("p");
        assert_eq!(monitor.snapshot("p").state, CircuitState::Open);
        std::thread::sleep(Duration::from_millis(40));
        // Cooldown elapsed: the half-open probe is admitted.
        assert!(monitor.try_acquire("p"));
        assert_eq!(monitor.snapshot("p").state, CircuitState::HalfOpen);
        // The probe fails with an expired session (auth class, not a
        // breaker failure).
        monitor.record_failure_with_category("p", ErrorCategory::SessionExpired);
        assert_eq!(
            monitor.snapshot("p").failures_by_class.auth_invalid,
            1,
            "the session event is counted"
        );
        assert_eq!(
            monitor.snapshot("p").consecutive_failures,
            2,
            "auth events stay out of the breaker total"
        );
        // The wedge regression: after the cooldown the provider must be
        // admissible again (pre-fix, HalfOpen{probe_in_flight:true} made
        // try_acquire false forever).
        std::thread::sleep(Duration::from_millis(40));
        assert!(
            monitor.try_acquire("p"),
            "auth-class half-open failure must not wedge the breaker"
        );
        // And a successful probe still closes the circuit cleanly.
        monitor.record_success("p");
        assert_eq!(monitor.snapshot("p").state, CircuitState::Closed);
        assert_eq!(monitor.snapshot("p").consecutive_failures, 0);
    }
}
