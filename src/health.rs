//! Provider circuit breaker used by search/stream orchestration.
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
}
enum InternalState {
    Closed,
    Open { until: Instant },
    HalfOpen { probe_in_flight: bool },
}
struct Entry {
    state: InternalState,
    failures: u32,
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
        });
        entry.failures = 0;
        entry.state = InternalState::Closed;
    }
    pub fn record_failure(&self, provider: &str) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.entry(provider.to_owned()).or_insert(Entry {
            state: InternalState::Closed,
            failures: 0,
        });
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
        }
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
}
