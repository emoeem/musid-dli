use crate::{
    ErrorCategory, ProviderRegistry, health::ProviderHealthMonitor, models::Track,
    provider::SearchQuery,
};
use futures::future::join_all;
use std::{
    collections::HashSet,
    sync::{Arc, atomic::AtomicBool, atomic::Ordering},
    time::Duration,
};

/// Cooperative cancellation token for the aggregation faces (M6,
/// RESOURCE_DESIGN.md §5.3). `cancel()` flips a shared flag; the
/// aggregation checks it before each page and before each provider job, so
/// further work stops as soon as the current in-flight request returns (an
/// in-flight request finishes its own timeout window; aborting a request
/// mid-flight buys nothing over the per-provider timeout). Cheap to clone.
#[derive(Clone, Default)]
pub struct SearchCancelToken(Arc<AtomicBool>);
impl std::fmt::Debug for SearchCancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchCancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}
impl SearchCancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    /// Request cancellation; the aggregate drops results gathered after
    /// this point and stops starting new pages/jobs.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct AggregationOptions {
    pub timeout: Duration,
    pub deduplicate: bool,
    /// Pages fetched per provider starting at `SearchQuery::page`
    /// (RESOURCE_DESIGN.md §5.2). Default 1 reproduces the pre-M6 single
    /// request; each page continues only while the provider reports
    /// `SearchPage::has_more`. Total fetches stay bounded by
    /// `providers x max_pages`.
    pub max_pages: u32,
    /// Width of the duration bucket used by the metadata dedup, in seconds
    /// (the legacy behavior is 3, search.rs dedup `as_secs()/3`).
    pub dedup_duration_bucket_secs: u64,
    /// Cooperative cancellation, consulted between pages and jobs (§5.3).
    pub cancel: Option<SearchCancelToken>,
}
impl Default for AggregationOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(8),
            deduplicate: true,
            max_pages: 1,
            dedup_duration_bucket_secs: 3,
            cancel: None,
        }
    }
}

/// One isolated provider failure with its M2 category (RESOURCE_DESIGN.md
/// §5.4): the classification reaches the caller instead of being compressed
/// to a string first. `message` carries the same text the legacy tuple face
/// exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSearchError {
    pub provider: String,
    pub category: ErrorCategory,
    pub message: String,
}

/// The typed aggregation result: deduped tracks plus per-provider typed
/// failures.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AggregatedSearch {
    pub tracks: Vec<Track>,
    pub failures: Vec<ProviderSearchError>,
}

/// Concurrent provider search with an independent timeout for each source.
/// Legacy tuple face over [`aggregate_search_typed`] (unchanged shape).
pub async fn aggregate_search(
    registry: &ProviderRegistry,
    query: &SearchQuery,
    options: &AggregationOptions,
) -> (Vec<Track>, Vec<(String, String)>) {
    into_legacy(aggregate_search_typed(registry, query, options).await)
}

/// Search while consulting/updating provider circuit-breaker health state.
/// Legacy tuple face over the typed inner pipeline.
pub async fn aggregate_search_with_health(
    registry: &ProviderRegistry,
    health: &ProviderHealthMonitor,
    query: &SearchQuery,
    options: &AggregationOptions,
) -> (Vec<Track>, Vec<(String, String)>) {
    into_legacy(aggregate_search_inner(registry, query, options, Some(health)).await)
}

/// Typed aggregation (RESOURCE_DESIGN.md §5.4): failures keep their M2
/// category. Timeout maps to `Timeout`; a tripped circuit maps to
/// `Unavailable` (degraded is the honest health category for an open
/// breaker).
pub async fn aggregate_search_typed(
    registry: &ProviderRegistry,
    query: &SearchQuery,
    options: &AggregationOptions,
) -> AggregatedSearch {
    aggregate_search_inner(registry, query, options, None).await
}

fn into_legacy(result: AggregatedSearch) -> (Vec<Track>, Vec<(String, String)>) {
    (
        result.tracks,
        result
            .failures
            .into_iter()
            .map(|failure| (failure.provider, failure.message))
            .collect(),
    )
}

async fn aggregate_search_inner(
    registry: &ProviderRegistry,
    query: &SearchQuery,
    options: &AggregationOptions,
    health: Option<&ProviderHealthMonitor>,
) -> AggregatedSearch {
    let max_pages = options.max_pages.max(1);
    let jobs = registry.search_providers().map(|provider| {
        let provider = provider.clone();
        let query = query.clone();
        let timeout = options.timeout;
        let cancel = options.cancel.clone();
        async move {
            let id = provider.id().to_owned();
            if let Some(health) = health
                && !health.try_acquire(&id)
            {
                return (
                    id.clone(),
                    Err(ProviderSearchError {
                        provider: id,
                        category: ErrorCategory::Unavailable,
                        message: "provider circuit breaker is open".into(),
                    }),
                );
            }
            let mut items = Vec::new();
            let mut failure = None;
            'pages: for page_index in 0..max_pages {
                if let Some(cancel) = &cancel
                    && cancel.is_cancelled()
                {
                    // Stop this provider's pagination; whether its already
                    // gathered items survive is decided once, at the
                    // aggregate level, after all jobs return.
                    break 'pages;
                }
                let page_query = SearchQuery {
                    page: query.page + page_index,
                    ..query.clone()
                };
                match tokio::time::timeout(timeout, provider.search(&page_query)).await {
                    Ok(Ok(page)) => {
                        let has_more = page.has_more;
                        let fetched = page.items.len();
                        if let Some(health) = health {
                            health.record_success(&id);
                        }
                        items.extend(page.items);
                        if !has_more || fetched == 0 {
                            break 'pages;
                        }
                    }
                    Ok(Err(error)) => {
                        let category = error.category();
                        if let Some(health) = health {
                            health.record_failure_with_category(&id, category);
                        }
                        failure = Some(ProviderSearchError {
                            provider: id.clone(),
                            category,
                            message: error.to_string(),
                        });
                        break 'pages;
                    }
                    Err(_) => {
                        if let Some(health) = health {
                            health.record_failure_with_category(&id, ErrorCategory::Timeout);
                        }
                        failure = Some(ProviderSearchError {
                            provider: id.clone(),
                            category: ErrorCategory::Timeout,
                            message: "provider search timed out".into(),
                        });
                        break 'pages;
                    }
                }
            }
            match failure {
                Some(failure) => (id, Err(failure)),
                None => (id, Ok(items)),
            }
        }
    });
    let mut tracks = Vec::new();
    let mut failures = Vec::new();
    for (_id, result) in join_all(jobs).await {
        match result {
            Ok(items) => tracks.extend(items),
            Err(error) => failures.push(error),
        }
    }
    // Aggregate-level cancellation (RESOURCE_DESIGN.md §5.3): the caller
    // cancelled, so no partial aggregate is returned, not even the results
    // of providers that finished before the flag was observed. A global
    // token is observed by every job through this one check; per-provider
    // clearing would quietly keep other providers' pre-cancel data.
    if let Some(cancel) = &options.cancel
        && cancel.is_cancelled()
    {
        tracks.clear();
    }
    if options.deduplicate {
        deduplicate_tracks_with_bucket(&mut tracks, options.dedup_duration_bucket_secs);
    }
    AggregatedSearch { tracks, failures }
}

/// Conservative dedup: ISRC first; otherwise normalized title/artist and a
/// duration bucket of [`crate::search::deduplicate_tracks_with_bucket`]'s
/// default width (3 s, the legacy behavior).
pub fn deduplicate_tracks(tracks: &mut Vec<Track>) {
    deduplicate_tracks_with_bucket(tracks, 3);
}

/// Same dedup with a caller-tunable duration bucket (RESOURCE_DESIGN.md
/// §5.5). A zero bucket is clamped to one second (a zero bucket would merge
/// every durationless pair and divide by zero).
pub fn deduplicate_tracks_with_bucket(tracks: &mut Vec<Track>, bucket_secs: u64) {
    let bucket = bucket_secs.max(1);
    let mut seen = HashSet::new();
    tracks.retain(|track| {
        let key = if let Some(isrc) = track.isrc.as_deref().filter(|s| !s.is_empty()) {
            format!("isrc:{}", isrc.to_ascii_uppercase())
        } else {
            let title = normalize(&track.title);
            let artists = track
                .artists
                .iter()
                .map(|s| normalize(s))
                .collect::<Vec<_>>()
                .join("|");
            let duration = track
                .duration
                .map(|d| d.as_secs() / bucket)
                .unwrap_or_default();
            format!("meta:{title}:{artists}:{duration}")
        };
        seen.insert(key)
    });
}
fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dedup_is_provider_agnostic_but_duration_sensitive() {
        let a = Track {
            id: crate::TrackId::new("a", "1"),
            title: "Song!".into(),
            artists: vec!["Artist".into()],
            album: None,
            album_id: None,
            duration: Some(Duration::from_secs(180)),
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: Default::default(),
        };
        let b = Track {
            id: crate::TrackId::new("b", "2"),
            ..a.clone()
        };
        let mut c = a.clone();
        c.id = crate::TrackId::new("c", "3");
        c.duration = Some(Duration::from_secs(240));
        let mut list = vec![a, b, c];
        deduplicate_tracks(&mut list);
        assert_eq!(list.len(), 2);
        // A zero bucket cannot divide by zero; it clamps to one second,
        // which keeps a 3s and a 4s track in separate buckets.
        let mut wide = vec![
            Track {
                id: crate::TrackId::new("a", "1"),
                title: "s".into(),
                artists: vec![],
                album: None,
                album_id: None,
                duration: Some(Duration::from_secs(3)),
                artwork_url: None,
                isrc: None,
                available_qualities: vec![],
                audio: None,
                extra: Default::default(),
            },
            Track {
                id: crate::TrackId::new("b", "2"),
                title: "s".into(),
                artists: vec![],
                album: None,
                album_id: None,
                duration: Some(Duration::from_secs(4)),
                artwork_url: None,
                isrc: None,
                available_qualities: vec![],
                audio: None,
                extra: Default::default(),
            },
        ];
        deduplicate_tracks_with_bucket(&mut wide, 0);
        assert_eq!(wide.len(), 2, "clamped 1s bucket keeps 3s and 4s apart");
        // A wide bucket merges the same pair (5s: both floors to 0).
        let mut narrow = vec![
            Track {
                id: crate::TrackId::new("a", "1"),
                title: "s".into(),
                artists: vec![],
                album: None,
                album_id: None,
                duration: Some(Duration::from_secs(2)),
                artwork_url: None,
                isrc: None,
                available_qualities: vec![],
                audio: None,
                extra: Default::default(),
            },
            Track {
                id: crate::TrackId::new("b", "2"),
                title: "s".into(),
                artists: vec![],
                album: None,
                album_id: None,
                duration: Some(Duration::from_secs(4)),
                artwork_url: None,
                isrc: None,
                available_qualities: vec![],
                audio: None,
                extra: Default::default(),
            },
        ];
        deduplicate_tracks_with_bucket(&mut narrow, 5);
        assert_eq!(narrow.len(), 1, "5s bucket merges 2s and 4s");
        let mut split = vec![
            Track {
                id: crate::TrackId::new("a", "1"),
                title: "s".into(),
                artists: vec![],
                album: None,
                album_id: None,
                duration: Some(Duration::from_secs(2)),
                artwork_url: None,
                isrc: None,
                available_qualities: vec![],
                audio: None,
                extra: Default::default(),
            },
            Track {
                id: crate::TrackId::new("b", "2"),
                title: "s".into(),
                artists: vec![],
                album: None,
                album_id: None,
                duration: Some(Duration::from_secs(4)),
                artwork_url: None,
                isrc: None,
                available_qualities: vec![],
                audio: None,
                extra: Default::default(),
            },
        ];
        deduplicate_tracks_with_bucket(&mut split, 1);
        assert_eq!(split.len(), 2, "1s bucket keeps 2s and 4s apart");
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    use crate::provider::{MusicProvider, SearchPage, TrackSearch};
    use async_trait::async_trait;
    use std::sync::Arc;

    struct MockProvider {
        id: &'static str,
        delay: Duration,
    }
    impl MusicProvider for MockProvider {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
    }
    #[async_trait]
    impl TrackSearch for MockProvider {
        async fn search(&self, _query: &SearchQuery) -> crate::Result<SearchPage> {
            tokio::time::sleep(self.delay).await;
            let track = Track {
                id: crate::TrackId::new(self.id, "track-1"),
                title: self.id.into(),
                artists: vec![],
                album: None,
                album_id: None,
                duration: None,
                artwork_url: None,
                isrc: None,
                available_qualities: vec![],
                audio: None,
                extra: Default::default(),
            };
            Ok(SearchPage {
                items: vec![track],
                total: Some(1),
                has_more: false,
            })
        }
    }

    #[tokio::test]
    async fn slow_provider_does_not_discard_fast_provider_results() {
        let mut registry = ProviderRegistry::new();
        registry.register_search(Arc::new(MockProvider {
            id: "fast",
            delay: Duration::ZERO,
        }));
        registry.register_search(Arc::new(MockProvider {
            id: "slow",
            delay: Duration::from_millis(50),
        }));
        let options = AggregationOptions {
            timeout: Duration::from_millis(5),
            deduplicate: false,
            ..Default::default()
        };
        let (tracks, errors) =
            aggregate_search(&registry, &SearchQuery::new("query"), &options).await;
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id.provider, "fast");
        assert_eq!(
            errors,
            vec![("slow".into(), "provider search timed out".into())]
        );
    }
}

#[cfg(test)]
mod pagination_tests {
    use super::*;
    use crate::provider::{MusicProvider, SearchPage, TrackSearch};
    use async_trait::async_trait;
    use std::sync::Arc;

    fn track(id: &str, page: u32) -> Track {
        Track {
            id: crate::TrackId::new("paged", format!("{id}-{page}")),
            title: format!("{id} page {page}"),
            artists: vec![],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: Default::default(),
        }
    }

    /// Records the pages it was asked for; `has_more` is true while more
    /// pages exist in its fixed table. Optionally cancels the shared token
    /// once a page index is reached, and optionally fails with a fresh
    /// typed error instead of serving pages (the error is rebuilt per
    /// call; `crate::Error` is deliberately not `Clone`).
    struct PagedProvider {
        pages: Vec<Vec<Track>>,
        cancel: Option<SearchCancelToken>,
        cancel_from_page: u32,
        fail_rate_limited: bool,
    }
    impl MusicProvider for PagedProvider {
        fn id(&self) -> &str {
            "paged"
        }
        fn name(&self) -> &str {
            "paged"
        }
    }
    #[async_trait]
    impl TrackSearch for PagedProvider {
        async fn search(&self, query: &SearchQuery) -> crate::Result<SearchPage> {
            if let Some(cancel) = &self.cancel
                && query.page >= self.cancel_from_page
            {
                cancel.cancel();
            }
            if self.fail_rate_limited {
                return Err(crate::Error::RateLimited {
                    provider: Some("paged".into()),
                    retry_after: None,
                    detail: Some(crate::ErrorDetail::new("search: HTTP 429")),
                });
            }
            let index = (query.page as usize).saturating_sub(1);
            let Some(items) = self.pages.get(index) else {
                return Ok(SearchPage {
                    items: vec![],
                    total: None,
                    has_more: false,
                });
            };
            Ok(SearchPage {
                items: items.clone(),
                total: Some(self.pages.len() as u64),
                has_more: index + 1 < self.pages.len(),
            })
        }
    }

    fn two_pages() -> Vec<Vec<Track>> {
        vec![vec![track("a", 1)], vec![track("a", 2)]]
    }

    #[tokio::test]
    async fn pagination_respects_has_more_and_max_pages() {
        let mut registry = ProviderRegistry::new();
        registry.register_search(Arc::new(PagedProvider {
            pages: two_pages(),
            cancel: None,
            cancel_from_page: u32::MAX,
            fail_rate_limited: false,
        }));
        // max_pages 1: only the first page (legacy behavior).
        let options = AggregationOptions {
            deduplicate: false,
            ..Default::default()
        };
        let result = aggregate_search_typed(&registry, &SearchQuery::new("q"), &options).await;
        assert_eq!(result.tracks.len(), 1);
        assert!(result.failures.is_empty());
        // max_pages 5: both pages arrive, the empty third stops the loop.
        let options = AggregationOptions {
            deduplicate: false,
            max_pages: 5,
            ..Default::default()
        };
        let result = aggregate_search_typed(&registry, &SearchQuery::new("q"), &options).await;
        assert_eq!(result.tracks.len(), 2);
        assert!(result.failures.is_empty());
    }

    #[tokio::test]
    async fn cancellation_drops_results_gathered_after_the_cancel_point() {
        let token = SearchCancelToken::new();
        // Three pages, all reporting has_more; the provider flips the
        // token during its page-2 call. A second, fast provider finishes
        // before the flag is observed: the aggregate-level check still
        // drops its results, because a global token cancels the whole
        // aggregate, not one provider's job.
        let pages = vec![
            vec![track("a", 1)],
            vec![track("a", 2)],
            vec![track("a", 3)],
        ];
        let mut registry = ProviderRegistry::new();
        registry.register_search(Arc::new(PagedProvider {
            pages,
            cancel: Some(token.clone()),
            cancel_from_page: 2,
            fail_rate_limited: false,
        }));
        registry.register_search(Arc::new(MockProvider {
            id: "fast",
            delay: Duration::ZERO,
        }));
        let options = AggregationOptions {
            deduplicate: false,
            max_pages: 5,
            cancel: Some(token.clone()),
            ..Default::default()
        };
        let result = aggregate_search_typed(&registry, &SearchQuery::new("q"), &options).await;
        // The token was observed cancelled mid-run: the whole aggregate is
        // dropped (the caller cancelled; partial data is not returned), no
        // further page is started, and the pre-cancel fast provider's
        // results do not survive either.
        assert!(token.is_cancelled());
        assert_eq!(result.tracks.len(), 0);
        assert!(result.failures.is_empty());
    }

    #[tokio::test]
    async fn cancellation_before_the_first_page_yields_nothing() {
        let token = SearchCancelToken::new();
        token.cancel();
        let mut registry = ProviderRegistry::new();
        registry.register_search(Arc::new(PagedProvider {
            pages: two_pages(),
            cancel: None,
            cancel_from_page: u32::MAX,
            fail_rate_limited: false,
        }));
        let options = AggregationOptions {
            deduplicate: false,
            cancel: Some(token),
            ..Default::default()
        };
        let result = aggregate_search_typed(&registry, &SearchQuery::new("q"), &options).await;
        assert_eq!(result.tracks.len(), 0);
        assert!(result.failures.is_empty());
    }

    #[tokio::test]
    async fn typed_categories_map_per_failure_arm() {
        let mut registry = ProviderRegistry::new();
        registry.register_search(Arc::new(PagedProvider {
            pages: two_pages(),
            cancel: None,
            cancel_from_page: u32::MAX,
            fail_rate_limited: true,
        }));
        registry.register_search(Arc::new(MockProvider {
            id: "slow",
            delay: Duration::from_millis(50),
        }));
        let options = AggregationOptions {
            timeout: Duration::from_millis(5),
            deduplicate: false,
            ..Default::default()
        };
        let result = aggregate_search_typed(&registry, &SearchQuery::new("q"), &options).await;
        assert_eq!(result.failures.len(), 2);
        let mut by_provider: std::collections::BTreeMap<&str, &ProviderSearchError> = result
            .failures
            .iter()
            .map(|f| (f.provider.as_str(), f))
            .collect();
        assert_eq!(
            by_provider.remove("paged").unwrap().category,
            ErrorCategory::RateLimited
        );
        assert_eq!(
            by_provider.remove("slow").unwrap().category,
            ErrorCategory::Timeout
        );
        // Circuit-open surfaces as Unavailable with the legacy message.
        let health = ProviderHealthMonitor::new(crate::health::ProviderHealthConfig {
            failure_threshold: 1,
            cooldown: Duration::from_secs(30),
        });
        health.record_failure("paged");
        let mut gated = ProviderRegistry::new();
        gated.register_search(Arc::new(PagedProvider {
            pages: two_pages(),
            cancel: None,
            cancel_from_page: u32::MAX,
            fail_rate_limited: false,
        }));
        let options = AggregationOptions {
            deduplicate: false,
            ..Default::default()
        };
        let result =
            aggregate_search_inner(&gated, &SearchQuery::new("q"), &options, Some(&health)).await;
        assert_eq!(result.failures.len(), 1);
        assert_eq!(result.failures[0].category, ErrorCategory::Unavailable);
        assert_eq!(
            result.failures[0].message,
            "provider circuit breaker is open"
        );
    }

    // MockProvider re-declared here so the timeout arm of the typed test
    // needs no cross-module visibility.
    struct MockProvider {
        id: &'static str,
        delay: Duration,
    }
    impl MusicProvider for MockProvider {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
    }
    #[async_trait]
    impl TrackSearch for MockProvider {
        async fn search(&self, _query: &SearchQuery) -> crate::Result<SearchPage> {
            tokio::time::sleep(self.delay).await;
            Ok(SearchPage {
                items: vec![track("slow", 1)],
                total: Some(1),
                has_more: false,
            })
        }
    }
}

#[cfg(test)]
mod health_semantics_tests {
    use super::*;
    use crate::provider::{MusicProvider, SearchPage, TrackSearch};
    use async_trait::async_trait;
    use std::sync::Arc;

    /// Fails every search with a fresh typed error (rebuilt per call because
    /// `crate::Error` is deliberately not `Clone`).
    struct FailingProvider {
        id: &'static str,
        category: ErrorCategory,
    }
    impl MusicProvider for FailingProvider {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
    }
    #[async_trait]
    impl TrackSearch for FailingProvider {
        async fn search(&self, _query: &SearchQuery) -> crate::Result<SearchPage> {
            Err(match self.category {
                ErrorCategory::RateLimited => crate::Error::RateLimited {
                    provider: Some(self.id.to_owned()),
                    retry_after: None,
                    detail: Some(crate::ErrorDetail::new("search: HTTP 429")),
                },
                ErrorCategory::SessionExpired => crate::Error::SessionExpired {
                    provider: Some(self.id.to_owned()),
                    detail: Some(crate::ErrorDetail::new("session expired")),
                },
                _ => unreachable!("test providers fail with RateLimited or SessionExpired"),
            })
        }
    }

    struct MockProvider {
        id: &'static str,
        delay: Duration,
    }
    impl MusicProvider for MockProvider {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
    }
    #[async_trait]
    impl TrackSearch for MockProvider {
        async fn search(&self, _query: &SearchQuery) -> crate::Result<SearchPage> {
            tokio::time::sleep(self.delay).await;
            Ok(SearchPage {
                items: vec![],
                total: Some(0),
                has_more: false,
            })
        }
    }

    /// M6 §4.2 re-verification pin (D38): the search face passes the real
    /// `Error::category()` into the health class table (the pre-M6 call site
    /// compressed the error to a string first, so 限流 vs 超时 were
    /// indistinguishable in the snapshot). Each breaker-driving class lands
    /// in its own counter while `consecutive_failures` stays the total.
    #[tokio::test]
    async fn provider_failures_fill_the_health_class_table_by_category() {
        let mut registry = ProviderRegistry::new();
        registry.register_search(Arc::new(FailingProvider {
            id: "limited",
            category: ErrorCategory::RateLimited,
        }));
        registry.register_search(Arc::new(MockProvider {
            id: "slow",
            delay: Duration::from_millis(50),
        }));
        let health =
            crate::health::ProviderHealthMonitor::new(crate::health::ProviderHealthConfig {
                failure_threshold: 3,
                cooldown: Duration::from_secs(30),
            });
        let options = AggregationOptions {
            timeout: Duration::from_millis(5),
            deduplicate: false,
            ..Default::default()
        };
        let result =
            aggregate_search_inner(&registry, &SearchQuery::new("q"), &options, Some(&health))
                .await;
        assert_eq!(result.failures.len(), 2);
        // The HTTP 429 is provider degradation: RateLimited class.
        let snapshot = health.snapshot("limited");
        assert_eq!(snapshot.failures_by_class.rate_limited, 1);
        assert_eq!(snapshot.failures_by_class.auth_invalid, 0);
        // The wall-clock timeout is a transport failure: Unreachable, not
        // Unknown, not RateLimited.
        let snapshot = health.snapshot("slow");
        assert_eq!(snapshot.failures_by_class.unreachable, 1);
        assert_eq!(snapshot.failures_by_class.rate_limited, 0);
        // Both classes keep driving the breaker total per provider.
        assert_eq!(health.snapshot("limited").consecutive_failures, 1);
        assert_eq!(health.snapshot("slow").consecutive_failures, 1);
    }

    /// M6 §4.2 re-verification pin (D38): a `SessionExpired` provider failure
    /// is a session signal, not provider degradation — it lands in
    /// `auth_invalid` and never steps the breaker, even with a threshold of
    /// one (the recovery is `AuthenticationProvider::refresh_session`, which
    /// a tripped circuit would block; §4.1/§4.2).
    #[tokio::test]
    async fn a_session_expired_failure_is_an_auth_signal_not_a_breaker_step() {
        let mut registry = ProviderRegistry::new();
        registry.register_search(Arc::new(FailingProvider {
            id: "expired",
            category: ErrorCategory::SessionExpired,
        }));
        let health =
            crate::health::ProviderHealthMonitor::new(crate::health::ProviderHealthConfig {
                failure_threshold: 1,
                cooldown: Duration::from_secs(30),
            });
        let options = AggregationOptions {
            deduplicate: false,
            ..Default::default()
        };
        let result =
            aggregate_search_inner(&registry, &SearchQuery::new("q"), &options, Some(&health))
                .await;
        assert_eq!(result.failures.len(), 1);
        let snapshot = health.snapshot("expired");
        assert_eq!(snapshot.failures_by_class.auth_invalid, 1);
        assert_eq!(
            snapshot.consecutive_failures, 0,
            "auth events stay out of the breaker total"
        );
        assert_eq!(snapshot.state, crate::health::CircuitState::Closed);
        assert!(
            health.try_acquire("expired"),
            "the circuit stays closed so refresh_session can run"
        );
    }
}
