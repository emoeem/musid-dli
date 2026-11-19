//! Ordered stream resolution over already-matched candidate tracks.
//!
//! M6 (RESOURCE_DESIGN.md §6): quality preference chains with an explicit
//! requested-vs-served record, structured fallback consumption on the
//! model, a caller-controlled provider order, and a negative-cache hookup
//! so repeated identical failures stop re-hitting the provider. All
//! pre-M6 signatures and failure messages are unchanged; `ResolveOptions`
//! itself is deliberately not extended (its struct literals live in every
//! provider test/example).
use crate::{
    Error, ErrorCategory, ProviderRegistry,
    cache::{NegativeCache, NegativeKey, StreamCache},
    health::ProviderHealthMonitor,
    matcher::rank_alternatives,
    models::{StreamInfo, StreamQuality, Track},
    provider::ResolveOptions,
};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveFailure {
    pub provider: String,
    pub message: String,
}

/// Quality preference chain (RESOURCE_DESIGN.md §6.2): tiers in preference
/// order, first feasible tier wins. `Unknown` is never a preference (it is
/// the "undeclared" tier, models/stream.rs:2-4) and duplicates are dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QualityChain {
    tiers: Vec<StreamQuality>,
}
impl QualityChain {
    pub fn from_tiers(tiers: impl IntoIterator<Item = StreamQuality>) -> Self {
        let mut seen = Vec::new();
        for tier in tiers {
            if tier != StreamQuality::Unknown && !seen.contains(&tier) {
                seen.push(tier);
            }
        }
        Self { tiers: seen }
    }
    pub fn tiers(&self) -> &[StreamQuality] {
        &self.tiers
    }
    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }
}

/// Requested-vs-served record of the winning chain attempt
/// (RESOURCE_DESIGN.md §6.2): `requested` is the tier the winning attempt
/// asked for, `served` is what the provider actually returned
/// (`StreamInfo::quality`), `degraded` is the exact inequality (`Unknown`
/// served counts as degraded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityOutcome {
    pub requested: StreamQuality,
    pub served: StreamQuality,
    pub degraded: bool,
}

/// Optional collaborators for stream resolution: health tracking, caching
/// and (M6) negative caching.
///
/// The cache is keyed by (track id, requested quality), so entries are only
/// reused for identical quality requests and expired URLs are never
/// returned. The negative cache (§6.4) records category-classified failures
/// so a repeat request is answered from the negative domain instead of
/// re-hitting the provider; `NegativeCache::clear_provider` is the
/// session-refresh face.
#[derive(Default, Clone, Copy)]
pub struct ResolvePipeline<'a> {
    pub health: Option<&'a ProviderHealthMonitor>,
    pub cache: Option<&'a StreamCache>,
    pub negative: Option<&'a NegativeCache>,
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
        pipeline.negative,
    )
    .await
}

/// Resolve through a quality preference chain (RESOURCE_DESIGN.md §6.2):
/// tiers outer-loop, candidates inner-loop; the first tier that yields any
/// successful resolve wins and carries its [`QualityOutcome`]. An empty
/// chain resolves nothing (the caller decides whether that is an error via
/// the returned failure list). All failure lists from the last tried tier
/// are returned when every tier fails.
pub async fn resolve_candidates_with_chain(
    registry: &ProviderRegistry,
    pipeline: ResolvePipeline<'_>,
    candidates: &[Track],
    chain: &QualityChain,
    per_provider_timeout: Duration,
) -> std::result::Result<(StreamInfo, QualityOutcome), Vec<ResolveFailure>> {
    let mut last_failures = Vec::new();
    for tier in chain.tiers() {
        let options = ResolveOptions { quality: *tier };
        match resolve_candidates_inner(
            registry,
            candidates,
            &options,
            per_provider_timeout,
            pipeline.health,
            pipeline.cache,
            pipeline.negative,
        )
        .await
        {
            Ok(stream) => {
                let served = stream.quality;
                return Ok((
                    stream,
                    QualityOutcome {
                        requested: *tier,
                        served,
                        degraded: served != *tier,
                    },
                ));
            }
            Err(failures) => last_failures = failures,
        }
    }
    if last_failures.is_empty() {
        Err(vec![ResolveFailure {
            provider: "registry".into(),
            message: Error::NotFound.to_string(),
        }])
    } else {
        Err(last_failures)
    }
}

/// Stable provider-priority reorder (RESOURCE_DESIGN.md §6.4): candidates
/// whose provider is listed come first, in list order; unlisted providers
/// keep their relative order after them. `MusicProvider::priority()` stays
/// a descriptor hint; the registry never sorts. This helper is the
/// caller's explicit decision surface.
pub fn order_candidates_by_priority(candidates: &[Track], priority: &[&str]) -> Vec<Track> {
    let rank = |provider: &str| {
        priority
            .iter()
            .position(|listed| *listed == provider)
            .unwrap_or(usize::MAX)
    };
    let mut ordered = candidates.to_vec();
    // `sort_by_key` is stable: equal ranks preserve the input order.
    ordered.sort_by_key(|track| rank(&track.id.provider));
    ordered
}

async fn resolve_candidates_inner(
    registry: &ProviderRegistry,
    candidates: &[Track],
    options: &ResolveOptions,
    per_provider_timeout: Duration,
    health: Option<&ProviderHealthMonitor>,
    cache: Option<&StreamCache>,
    negative: Option<&NegativeCache>,
) -> std::result::Result<StreamInfo, Vec<ResolveFailure>> {
    let negative_key_for = |track: &Track| NegativeKey::Stream {
        id: track.id.clone(),
        quality: options.quality,
    };
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
            if let Some(negative) = negative {
                negative.invalidate(&negative_key_for(track));
            }
            return Ok(stream);
        }
        if let Some(negative) = negative
            && let Some(hit) = negative.check(&negative_key_for(track))
        {
            failures.push(ResolveFailure {
                provider: provider_id.to_owned(),
                message: format!("negative-cached failure: {:?}", hit.category),
            });
            continue;
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
                if let Some(negative) = negative {
                    negative.invalidate(&negative_key_for(track));
                }
                return Ok(stream);
            }
            Ok(Ok(_)) => {
                if let Some(health) = health {
                    health
                        .record_failure_with_category(provider_id, ErrorCategory::InvalidResponse);
                }
                if let Some(negative) = negative {
                    negative.record(
                        negative_key_for(track),
                        provider_id,
                        ErrorCategory::InvalidResponse,
                    );
                }
                failures.push(ResolveFailure {
                    provider: provider_id.to_owned(),
                    message: "provider returned an invalid stream URL".into(),
                });
            }
            Ok(Err(error)) => {
                let category = error.category();
                if let Some(health) = health {
                    health.record_failure_with_category(provider_id, category);
                }
                if let Some(negative) = negative {
                    negative.record(negative_key_for(track), provider_id, category);
                }
                failures.push(ResolveFailure {
                    provider: provider_id.to_owned(),
                    message: error.to_string(),
                });
            }
            Err(_) => {
                if let Some(health) = health {
                    health.record_failure_with_category(provider_id, ErrorCategory::Timeout);
                }
                if let Some(negative) = negative {
                    negative.record(negative_key_for(track), provider_id, ErrorCategory::Timeout);
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
        pipeline.negative,
    )
    .await
}

/// Alternative-aware resolution through a quality chain: the ranked
/// candidate order of [`resolve_with_alternatives_cached`] combined with
/// the tier walk and outcome record of [`resolve_candidates_with_chain`].
pub async fn resolve_with_alternatives_chain(
    registry: &ProviderRegistry,
    pipeline: ResolvePipeline<'_>,
    target: &Track,
    alternatives: &[Track],
    minimum_match_score: u8,
    chain: &QualityChain,
    per_provider_timeout: Duration,
) -> std::result::Result<(StreamInfo, QualityOutcome), Vec<ResolveFailure>> {
    let ranked = rank_alternatives(target, alternatives, minimum_match_score);
    let mut candidates = Vec::with_capacity(ranked.len() + 1);
    candidates.push(target.clone());
    candidates.extend(ranked.into_iter().map(|candidate| candidate.track));
    resolve_candidates_with_chain(registry, pipeline, &candidates, chain, per_provider_timeout)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorDetail;
    use crate::TrackId;
    use crate::cache::NegativeTtlPolicy;
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
                size_is_advisory: false,
                checksum_algorithm: None,
                fallback_endpoints: vec![],
            })
        }
    }

    /// Fails every resolve with a typed error (for the negative-cache face).
    struct AlwaysFailingResolver {
        id: &'static str,
        calls: AtomicUsize,
    }
    impl MusicProvider for AlwaysFailingResolver {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
    }
    #[async_trait]
    impl StreamResolver for AlwaysFailingResolver {
        async fn resolve(
            &self,
            _track: &Track,
            _options: &ResolveOptions,
        ) -> crate::Result<StreamInfo> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::RateLimited {
                provider: Some(self.id.to_owned()),
                retry_after: None,
                detail: Some(ErrorDetail::new("stream resolve: HTTP 429")),
            })
        }
    }

    /// Serves a fixed tier, failing the tiers listed in `fail_tiers` with
    /// `UnsupportedQuality` (for the quality-chain face).
    struct TieredResolver {
        id: &'static str,
        calls: AtomicUsize,
        serve: StreamQuality,
        fail_tiers: Vec<StreamQuality>,
    }
    impl MusicProvider for TieredResolver {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.id
        }
    }
    #[async_trait]
    impl StreamResolver for TieredResolver {
        async fn resolve(
            &self,
            _track: &Track,
            options: &ResolveOptions,
        ) -> crate::Result<StreamInfo> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_tiers.contains(&options.quality) {
                return Err(Error::UnsupportedQuality {
                    provider: Some(self.id.to_owned()),
                    quality: options.quality,
                    detail: None,
                });
            }
            Ok(StreamInfo {
                url: "https://cdn.test/a.mp3".into(),
                quality: self.serve,
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
                size_is_advisory: false,
                checksum_algorithm: None,
                fallback_endpoints: vec![],
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
            audio: None,
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
            negative: None,
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
            negative: None,
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

    // ---- M6: negative-cache hookup (RESOURCE_DESIGN.md §6.4, R10) ----

    #[tokio::test]
    async fn failures_are_negatively_cached_and_skipped() {
        let resolver = std::sync::Arc::new(AlwaysFailingResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let negative = NegativeCache::new(NegativeTtlPolicy::standard(), 8);
        let track = candidate();
        let options = ResolveOptions {
            quality: StreamQuality::High,
        };
        let pipeline = ResolvePipeline {
            negative: Some(&negative),
            ..ResolvePipeline::default()
        };
        let first = resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &options,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].message, "rate limited: stream resolve: HTTP 429");
        // The second call is answered from the negative domain: the
        // provider is not hit again.
        let second = resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &options,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(second[0].message, "negative-cached failure: RateLimited");
        // The session-refresh face clears the provider domain wholesale and
        // the provider is reachable again.
        negative.clear_provider("mock");
        let third = resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &options,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        assert_eq!(third[0].message, "rate limited: stream resolve: HTTP 429");
    }

    #[tokio::test]
    async fn a_success_invalidates_its_stale_negative_entry() {
        let resolver = std::sync::Arc::new(CountingResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let negative = NegativeCache::new(NegativeTtlPolicy::standard(), 8);
        let track = candidate();
        let options = ResolveOptions {
            quality: StreamQuality::High,
        };
        // A stale negative (e.g. recorded by an earlier flaky window) would
        // block this provider; the successful resolve must clear it.
        negative.record(
            NegativeKey::Stream {
                id: track.id.clone(),
                quality: StreamQuality::High,
            },
            "mock",
            ErrorCategory::RateLimited,
        );
        let pipeline = ResolvePipeline {
            negative: Some(&negative),
            ..ResolvePipeline::default()
        };
        // Pre-clear like a session refresh would; then a success keeps the
        // domain clean.
        negative.clear_provider("mock");
        resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &options,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert!(negative.is_empty());
        // A direct record without a clear still blocks (guard against the
        // invalidate path misfiring).
        negative.record(
            NegativeKey::Stream {
                id: track.id.clone(),
                quality: StreamQuality::High,
            },
            "mock",
            ErrorCategory::RateLimited,
        );
        let blocked = resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &options,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(blocked[0].message, "negative-cached failure: RateLimited");
    }

    #[tokio::test]
    async fn an_expired_negative_entry_releases_the_provider() {
        // Expiry semantics at the resolve face (§6.4 rule 1): the negative
        // entry skips the provider only inside its category TTL; once it
        // expires, `NegativeCache::check` removes it on sight and the
        // provider is hit again.
        let resolver = std::sync::Arc::new(AlwaysFailingResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let mut policy = NegativeTtlPolicy::standard();
        policy
            .per_category
            .insert(ErrorCategory::RateLimited, Duration::from_millis(150));
        let negative = NegativeCache::new(policy, 8);
        let track = candidate();
        let options = ResolveOptions {
            quality: StreamQuality::High,
        };
        let pipeline = ResolvePipeline {
            negative: Some(&negative),
            ..ResolvePipeline::default()
        };
        let first = resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &options,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(first[0].message, "rate limited: stream resolve: HTTP 429");
        // Inside the TTL window the repeat is answered from the negative
        // domain.
        let second = resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &options,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(second[0].message, "negative-cached failure: RateLimited");
        // After expiry the provider is reachable again (and re-records).
        tokio::time::sleep(Duration::from_millis(250)).await;
        let third = resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &options,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        assert_eq!(third[0].message, "rate limited: stream resolve: HTTP 429");
    }

    #[tokio::test]
    async fn a_negative_entry_for_one_quality_does_not_block_another() {
        // §6.4 key shape `NegativeKey::Stream { track.id, options.quality }`:
        // a Lossless-scoped failure must not suppress the same track
        // requested at High (mirrors the positive cache's per-quality
        // scoping pin).
        let resolver = std::sync::Arc::new(TieredResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
            serve: StreamQuality::High,
            fail_tiers: vec![StreamQuality::Lossless],
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let negative = NegativeCache::new(NegativeTtlPolicy::standard(), 8);
        let track = candidate();
        let pipeline = ResolvePipeline {
            negative: Some(&negative),
            ..ResolvePipeline::default()
        };
        let lossless = ResolveOptions {
            quality: StreamQuality::Lossless,
        };
        let failures = resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &lossless,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(failures[0].message, "unsupported quality (Lossless)");
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        // The High request carries a different negative key, so the provider
        // is reached and succeeds.
        let high = ResolveOptions {
            quality: StreamQuality::High,
        };
        let stream = resolve_candidates_cached(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &high,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(stream.quality, StreamQuality::High);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }

    // ---- M6: quality chain (RESOURCE_DESIGN.md §6.2, R8) ----

    #[tokio::test]
    async fn quality_chain_prefers_the_first_feasible_tier() {
        let resolver = std::sync::Arc::new(TieredResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
            serve: StreamQuality::High,
            fail_tiers: vec![StreamQuality::Lossless],
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let track = candidate();
        let chain = QualityChain::from_tiers(vec![
            StreamQuality::Lossless,
            StreamQuality::High,
            StreamQuality::High,    // duplicate dropped
            StreamQuality::Unknown, // never a preference
        ]);
        assert_eq!(
            chain.tiers(),
            &[StreamQuality::Lossless, StreamQuality::High]
        );
        let pipeline = ResolvePipeline::default();
        let (stream, outcome) = resolve_candidates_with_chain(
            &registry,
            pipeline,
            std::slice::from_ref(&track),
            &chain,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(stream.quality, StreamQuality::High);
        assert_eq!(
            outcome,
            QualityOutcome {
                requested: StreamQuality::High,
                served: StreamQuality::High,
                degraded: false,
            }
        );
        assert_eq!(
            resolver.calls.load(Ordering::SeqCst),
            2,
            "lossless failed, high succeeded"
        );
        // An empty chain resolves nothing.
        let empty = QualityChain::from_tiers(vec![StreamQuality::Unknown]);
        let failures = resolve_candidates_with_chain(
            &registry,
            pipeline,
            &[track],
            &empty,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(failures[0].provider, "registry");
    }

    #[tokio::test]
    async fn quality_chain_records_degradation() {
        let resolver = std::sync::Arc::new(TieredResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
            serve: StreamQuality::Standard,
            fail_tiers: vec![StreamQuality::Lossless, StreamQuality::High],
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let track = candidate();
        let chain = QualityChain::from_tiers(vec![
            StreamQuality::Lossless,
            StreamQuality::High,
            StreamQuality::Standard,
        ]);
        let (stream, outcome) = resolve_candidates_with_chain(
            &registry,
            TieredPipeline::default(),
            std::slice::from_ref(&track),
            &chain,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(stream.quality, StreamQuality::Standard);
        assert_eq!(
            outcome,
            QualityOutcome {
                requested: StreamQuality::Standard,
                served: StreamQuality::Standard,
                degraded: false,
            }
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn quality_chain_records_a_lower_served_tier_as_degraded() {
        // The provider accepts the High request but serves Standard: a real
        // requested-vs-served downgrade (not a tier failure), so the
        // outcome must carry requested=High, served=Standard, degraded=true
        // (§6.2 exact inequality).
        let resolver = std::sync::Arc::new(TieredResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
            serve: StreamQuality::Standard,
            fail_tiers: vec![StreamQuality::Lossless],
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let track = candidate();
        let chain = QualityChain::from_tiers(vec![StreamQuality::Lossless, StreamQuality::High]);
        let (stream, outcome) = resolve_candidates_with_chain(
            &registry,
            ResolvePipeline::default(),
            std::slice::from_ref(&track),
            &chain,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(stream.quality, StreamQuality::Standard);
        assert_eq!(
            outcome,
            QualityOutcome {
                requested: StreamQuality::High,
                served: StreamQuality::Standard,
                degraded: true,
            }
        );
        assert_eq!(
            resolver.calls.load(Ordering::SeqCst),
            2,
            "lossless failed, high accepted-but-downgraded"
        );
    }

    #[tokio::test]
    async fn an_unknown_served_tier_counts_as_degraded() {
        // §6.2: `Unknown` served counts as degraded (it is the undeclared
        // tier, models/stream.rs:2-4, never a real preference).
        let resolver = std::sync::Arc::new(TieredResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
            serve: StreamQuality::Unknown,
            fail_tiers: vec![StreamQuality::Lossless],
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let track = candidate();
        let chain = QualityChain::from_tiers(vec![StreamQuality::Lossless, StreamQuality::High]);
        let (_, outcome) = resolve_candidates_with_chain(
            &registry,
            ResolvePipeline::default(),
            std::slice::from_ref(&track),
            &chain,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            QualityOutcome {
                requested: StreamQuality::High,
                served: StreamQuality::Unknown,
                degraded: true,
            }
        );
    }

    // ---- M6: provider order helper (RESOURCE_DESIGN.md §6.4, R8) ----

    #[test]
    fn order_candidates_by_priority_is_stable() {
        let alpha = Track {
            id: TrackId::new("alpha", "1"),
            ..candidate_provider_track("alpha")
        };
        let beta = Track {
            id: TrackId::new("beta", "1"),
            ..candidate_provider_track("beta")
        };
        let gamma = Track {
            id: TrackId::new("gamma", "1"),
            ..candidate_provider_track("gamma")
        };
        let alpha2 = Track {
            id: TrackId::new("alpha", "2"),
            ..candidate_provider_track("alpha")
        };
        let ordered =
            order_candidates_by_priority(&[alpha, beta, gamma, alpha2], &["gamma", "alpha"]);
        let providers: Vec<&str> = ordered.iter().map(|t| t.id.provider.as_str()).collect();
        assert_eq!(providers, vec!["gamma", "alpha", "alpha", "beta"]);
    }

    fn candidate_provider_track(provider: &str) -> Track {
        Track {
            id: TrackId::new(provider, "x"),
            title: "t".into(),
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

    /// Local alias so the chain tests read cleanly; identical to
    /// `ResolvePipeline::default()`.
    type TieredPipeline<'a> = ResolvePipeline<'a>;

    // ---- M6 §4.2 re-verification pin (D38): the resolve face feeds the
    // health class table with the real M2 category instead of the compressed
    // string (the pre-M6 call site folded `Error` into its message first, so
    // 限流/认证失效/维护中 were indistinguishable in the snapshot). ----

    #[tokio::test]
    async fn resolve_failures_fill_the_health_class_table_by_category() {
        let resolver = std::sync::Arc::new(AlwaysFailingResolver {
            id: "mock",
            calls: AtomicUsize::new(0),
        });
        let mut registry = ProviderRegistry::new();
        registry.register_stream_resolver(resolver.clone());
        let health = ProviderHealthMonitor::new(crate::health::ProviderHealthConfig {
            failure_threshold: 3,
            cooldown: Duration::from_secs(30),
        });
        let pipeline = ResolvePipeline {
            cache: None,
            health: Some(&health),
            negative: None,
        };
        let options = ResolveOptions {
            quality: StreamQuality::High,
        };
        let result = resolve_candidates_cached(
            &registry,
            pipeline,
            &[candidate()],
            &options,
            Duration::from_secs(1),
        )
        .await;
        assert!(result.is_err(), "the rate-limited provider must fail");
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        let snapshot = health.snapshot("mock");
        // The real category (RateLimited) reached the class table, and the
        // non-auth classes stay out of the auth counter.
        assert_eq!(snapshot.failures_by_class.rate_limited, 1);
        assert_eq!(snapshot.failures_by_class.auth_invalid, 0);
        // A rate-limited failure is provider degradation: it advances the
        // breaker-driving total exactly as the pre-M6 record_failure did.
        assert_eq!(snapshot.consecutive_failures, 1);
        assert_eq!(snapshot.state, crate::health::CircuitState::Closed);
    }
}
