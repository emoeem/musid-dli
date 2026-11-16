//! Ordered stream resolution over already-matched candidate tracks.
use crate::{
    Error, ProviderRegistry,
    cache::StreamCache,
    health::ProviderHealthMonitor,
    matcher::rank_alternatives,
    models::{StreamInfo, Track},
    provider::ResolveOptions,
};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveFailure {
    pub provider: String,
    pub message: String,
}

/// Optional collaborators for stream resolution: health tracking and caching.
///
/// The cache is keyed by (track id, requested quality), so entries are only
/// reused for identical quality requests and expired URLs are never returned.
#[derive(Default, Clone, Copy)]
pub struct ResolvePipeline<'a> {
    pub health: Option<&'a ProviderHealthMonitor>,
    pub cache: Option<&'a StreamCache>,
}

pub async fn resolve_candidates(
    registry: &ProviderRegistry,
    candidates: &[Track],
    options: &ResolveOptions,
    per_provider_timeout: Duration,
) -> std::result::Result<StreamInfo, Vec<ResolveFailure>> {
    resolve_candidates_inner(
        registry,
        candidates,
        options,
        per_provider_timeout,
        None,
        None,
    )
    .await
}

pub async fn resolve_candidates_with_health(
    registry: &ProviderRegistry,
    health: &ProviderHealthMonitor,
    candidates: &[Track],
    options: &ResolveOptions,
    per_provider_timeout: Duration,
) -> std::result::Result<StreamInfo, Vec<ResolveFailure>> {
    resolve_candidates_inner(
        registry,
        candidates,
        options,
        per_provider_timeout,
        Some(health),
        None,
    )
    .await
}

/// Resolve candidates with cache support: hits short-circuit the provider,
/// successful resolutions are stored for the next identical request.
pub async fn resolve_candidates_cached(
    registry: &ProviderRegistry,
    pipeline: ResolvePipeline<'_>,
    candidates: &[Track],
    options: &ResolveOptions,
    per_provider_timeout: Duration,
) -> std::result::Result<StreamInfo, Vec<ResolveFailure>> {
    resolve_candidates_inner(
        registry,
        candidates,
        options,
        per_provider_timeout,
        pipeline.health,
        pipeline.cache,
    )
    .await
}

async fn resolve_candidates_inner(
    registry: &ProviderRegistry,
    candidates: &[Track],
    options: &ResolveOptions,
    per_provider_timeout: Duration,
    health: Option<&ProviderHealthMonitor>,
    cache: Option<&StreamCache>,
) -> std::result::Result<StreamInfo, Vec<ResolveFailure>> {
    let mut failures = Vec::new();
    for track in candidates {
        let provider_id = track.id.provider.as_str();
        let Some(provider) = registry.stream_provider(provider_id) else {
            failures.push(ResolveFailure {
                provider: provider_id.to_owned(),
                message: "stream capability unavailable".into(),
            });
            continue;
        };
        if let Some(cache) = cache
            && let Some(stream) = cache.get(&track.id, options.quality)
        {
            if let Some(health) = health {
                health.record_success(provider_id);
            }
            return Ok(stream);
        }
        if let Some(health) = health
            && !health.try_acquire(provider_id)
        {
            failures.push(ResolveFailure {
                provider: provider_id.to_owned(),
                message: "provider circuit breaker is open".into(),
            });
            continue;
        }
        match tokio::time::timeout(per_provider_timeout, provider.resolve(track, options)).await {
            Ok(Ok(stream))
                if stream.url.starts_with("https://") || stream.url.starts_with("http://") =>
            {
                if let Some(health) = health {
                    health.record_success(provider_id);
                }
                if let Some(cache) = cache {
                    cache.insert(track.id.clone(), options.quality, stream.clone());
                }
                return Ok(stream);
            }
            Ok(Ok(_)) => {
                if let Some(health) = health {
                    health.record_failure(provider_id);
                }
                failures.push(ResolveFailure {
                    provider: provider_id.to_owned(),
                    message: "provider returned an invalid stream URL".into(),
                });
            }
            Ok(Err(error)) => {
                if let Some(health) = health {
                    health.record_failure(provider_id);
                }
                failures.push(ResolveFailure {
                    provider: provider_id.to_owned(),
                    message: error.to_string(),
                });
            }
            Err(_) => {
                if let Some(health) = health {
                    health.record_failure(provider_id);
                }
                failures.push(ResolveFailure {
                    provider: provider_id.to_owned(),
                    message: "provider resolution timed out".into(),
                });
            }
        }
    }
    if failures.is_empty() {
        Err(vec![ResolveFailure {
            provider: "registry".into(),
            message: Error::NotFound.to_string(),
        }])
    } else {
        Err(failures)
    }
}

/// Resolve the original source first, then metadata-matched alternatives by confidence.
pub async fn resolve_with_alternatives(
    registry: &ProviderRegistry,
    target: &Track,
    alternatives: &[Track],
    minimum_match_score: u8,
    options: &ResolveOptions,
    per_provider_timeout: Duration,
) -> std::result::Result<StreamInfo, Vec<ResolveFailure>> {
    let ranked = rank_alternatives(target, alternatives, minimum_match_score);
    let mut candidates = Vec::with_capacity(ranked.len() + 1);
    candidates.push(target.clone());
    candidates.extend(ranked.into_iter().map(|candidate| candidate.track));
    resolve_candidates(registry, &candidates, options, per_provider_timeout).await
}

pub async fn resolve_with_alternatives_and_health(
    registry: &ProviderRegistry,
    health: &ProviderHealthMonitor,
    target: &Track,
    alternatives: &[Track],
    minimum_match_score: u8,
    options: &ResolveOptions,
    per_provider_timeout: Duration,
) -> std::result::Result<StreamInfo, Vec<ResolveFailure>> {
    let ranked = rank_alternatives(target, alternatives, minimum_match_score);
    let mut candidates = Vec::with_capacity(ranked.len() + 1);
    candidates.push(target.clone());
    candidates.extend(ranked.into_iter().map(|candidate| candidate.track));
    resolve_candidates_with_health(registry, health, &candidates, options, per_provider_timeout)
        .await
}

/// Alternative-aware resolution with health tracking and caching combined.
pub async fn resolve_with_alternatives_cached(
    registry: &ProviderRegistry,
    pipeline: ResolvePipeline<'_>,
    target: &Track,
    alternatives: &[Track],
    minimum_match_score: u8,
    options: &ResolveOptions,
    per_provider_timeout: Duration,
) -> std::result::Result<StreamInfo, Vec<ResolveFailure>> {
    let ranked = rank_alternatives(target, alternatives, minimum_match_score);
    let mut candidates = Vec::with_capacity(ranked.len() + 1);
    candidates.push(target.clone());
    candidates.extend(ranked.into_iter().map(|candidate| candidate.track));
    resolve_candidates_inner(
        registry,
        &candidates,
        options,
        per_provider_timeout,
        pipeline.health,
        pipeline.cache,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TrackId;
    use crate::models::{AudioFormat, StreamQuality};
    use crate::provider::{MusicProvider, StreamResolver};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingResolver {
        id: &'static str,
        calls: AtomicUsize,
    }
    impl MusicProvider for CountingResolver {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
    }
    #[async_trait]
    impl StreamResolver for CountingResolver {
        async fn resolve(
            &self,
            _track: &Track,
            _options: &ResolveOptions,
        ) -> crate::Result<StreamInfo> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(StreamInfo {
                url: "https://cdn.test/a.mp3".into(),
                quality: StreamQuality::High,
                format: AudioFormat::Mp3,
                duration: None,
                expires_at_unix: None,
                content_length: None,
                checksum: None,
                headers: vec![],
                fallback_urls: vec![],
                index_range: None,
                init_range: None,
                max_chunk_size: None,
            })
        }
    }

    fn candidate() -> Track {
        Track {
            id: TrackId::new("mock", "1"),
            title: "t".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            extra: Default::default(),
        }
    }

    #[tokio::test]
    async fn cache_hit_skips_provider_resolution() {
        let resolver = std::sync::Arc::new(CountingResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let cache = StreamCache::new(Duration::from_secs(60), 8);
        let track = candidate();
        let options = ResolveOptions {
            quality: StreamQuality::High,
        };
        let pipeline = ResolvePipeline {
            cache: Some(&cache),
            health: None,
        };
        let first = resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &options,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(first.url, "https://cdn.test/a.mp3");
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        let second = resolve_candidates_cached(
            &registry,
            pipeline,
            &[track],
            &options,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(second.url, "https://cdn.test/a.mp3");
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cache_entries_are_scoped_per_requested_quality() {
        let resolver = std::sync::Arc::new(CountingResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let cache = StreamCache::new(Duration::from_secs(60), 8);
        let track = candidate();
        let high = ResolveOptions {
            quality: StreamQuality::High,
        };
        let lossless = ResolveOptions {
            quality: StreamQuality::Lossless,
        };
        let pipeline = ResolvePipeline {
            cache: Some(&cache),
            health: None,
        };
        resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &high,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &lossless,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn resolution_without_cache_always_reaches_the_provider() {
        let resolver = std::sync::Arc::new(CountingResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let track = candidate();
        let options = ResolveOptions {
            quality: StreamQuality::High,
        };
        for _ in 0..2 {
            resolve_candidates(
                &registry,
                std::slice::from_ref(&track),
                &options,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        }
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }
}
