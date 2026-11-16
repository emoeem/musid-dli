use crate::{
    ProviderRegistry, health::ProviderHealthMonitor, models::Track, provider::SearchQuery,
};
use futures::future::join_all;
use std::{collections::HashSet, time::Duration};

#[derive(Debug, Clone)]
pub struct AggregationOptions {
    pub timeout: Duration,
    pub deduplicate: bool,
}
impl Default for AggregationOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(8),
            deduplicate: true,
        }
    }
}

/// Concurrent provider search with an independent timeout for each source.
pub async fn aggregate_search(
    registry: &ProviderRegistry,
    query: &SearchQuery,
    options: &AggregationOptions,
) -> (Vec<Track>, Vec<(String, String)>) {
    aggregate_search_inner(registry, query, options, None).await
}

/// Search while consulting/updating provider circuit-breaker health state.
pub async fn aggregate_search_with_health(
    registry: &ProviderRegistry,
    health: &ProviderHealthMonitor,
    query: &SearchQuery,
    options: &AggregationOptions,
) -> (Vec<Track>, Vec<(String, String)>) {
    aggregate_search_inner(registry, query, options, Some(health)).await
}

async fn aggregate_search_inner(
    registry: &ProviderRegistry,
    query: &SearchQuery,
    options: &AggregationOptions,
    health: Option<&ProviderHealthMonitor>,
) -> (Vec<Track>, Vec<(String, String)>) {
    let jobs = registry.search_providers().map(|provider| {
        let provider = provider.clone();
        let query = query.clone();
        let timeout = options.timeout;
        async move {
            let id = provider.id().to_owned();
            if let Some(health) = health
                && !health.try_acquire(&id)
            {
                return (id, Err("provider circuit breaker is open".into()));
            }
            let result = tokio::time::timeout(timeout, provider.search(&query)).await;
            match result {
                Ok(Ok(page)) => {
                    if let Some(health) = health {
                        health.record_success(&id);
                    }
                    (id, Ok(page.items))
                }
                Ok(Err(error)) => {
                    if let Some(health) = health {
                        health.record_failure(&id);
                    }
                    (id, Err(error.to_string()))
                }
                Err(_) => {
                    if let Some(health) = health {
                        health.record_failure(&id);
                    }
                    (id, Err("provider search timed out".into()))
                }
            }
        }
    });
    let mut tracks = Vec::new();
    let mut errors = Vec::new();
    for (id, result) in join_all(jobs).await {
        match result {
            Ok(items) => tracks.extend(items),
            Err(error) => errors.push((id, error)),
        }
    }
    if options.deduplicate {
        deduplicate_tracks(&mut tracks);
    }
    (tracks, errors)
}

/// Conservative dedup: ISRC first; otherwise normalized title/artist and duration bucket.
pub fn deduplicate_tracks(tracks: &mut Vec<Track>) {
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
            let duration = track.duration.map(|d| d.as_secs() / 3).unwrap_or_default();
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
