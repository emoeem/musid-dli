//! Lyrics cache domains (M6, RESOURCE_DESIGN.md §8): positive search and
//! document domains plus the category-classified negative domain, all
//! bounded, all keyed by stable ids (`TrackId`, provider-side lyric id),
//! never by a song title.
//!
//! Feature layering: this module compiles under `lyrics-cache` (which
//! implies `lyrics`); the search-domain face additionally needs
//! `lyrics-search` and is gated `cfg(all(lyrics-cache, lyrics-search))`.
//! `search_many`'s legacy signature is untouched.
//!
//! Absence symmetry (RESOURCE_DESIGN.md §8.2): both positive domains cache
//! an explicit "nothing exists" (an empty search list, a `None` document)
//! so a repeat ask is answered consistently instead of re-querying every
//! source for a track that has no lyrics.
use crate::ProviderRegistry;
use crate::cache::{NegativeCache, NegativeHit, NegativeKey, NegativeTtlPolicy, TtlCache};
use crate::error::{Error, ErrorCategory, ErrorDetail};
use crate::models::{LyricDocument, LyricSearchResult, Track, TrackId};
use std::time::Duration;

/// Which refinement mode a search-domain entry belongs to (RESOURCE_DESIGN.md
/// §8.2): `search_with_refinement`'s two modes legitimately return different
/// result sets, so the mode rides every key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchMode {
    /// `full_search = true`.
    Full,
    /// `full_search = false` (refinement).
    Refine,
}
impl SearchMode {
    pub fn from_full_search(full_search: bool) -> Self {
        if full_search {
            Self::Full
        } else {
            Self::Refine
        }
    }
    /// The negative-key `mode` part (`"full"` / `"refine"`).
    pub fn key_part(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Refine => "refine",
        }
    }
}

/// Configuration of the lyrics cache (RESOURCE_DESIGN.md §8.2). Defaults:
/// search lists 10 min, documents 24 h, standard negative TTLs, capacity
/// 512 entries per domain (the negative domain shares the same bound).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsCacheConfig {
    pub search_ttl: Duration,
    pub document_ttl: Duration,
    pub negative: NegativeTtlPolicy,
    pub capacity: usize,
}
impl Default for LyricsCacheConfig {
    fn default() -> Self {
        Self {
            search_ttl: Duration::from_secs(600),
            document_ttl: Duration::from_secs(24 * 3600),
            negative: NegativeTtlPolicy::standard(),
            capacity: 512,
        }
    }
}

/// Bounded lyrics cache (RESOURCE_DESIGN.md §8.2). Positive domains are
/// `TtlCache`s with earliest-expiry eviction; the negative domain is a
/// [`NegativeCache`] carrying M2 categories.
pub struct LyricsCache {
    /// Query results keyed by the query track's stable id + mode. An empty
    /// list is a cached absence, symmetric with the document domain's
    /// `None`.
    search: TtlCache<(TrackId, SearchMode), Vec<LyricSearchResult>>,
    /// Lyrics documents keyed by `(provider, provider-side resource id)`:
    /// the provider-direct face keys by the track id, the search face by
    /// the `LyricSearchResult` id. `None` values cache an explicit
    /// "no lyrics exist" so a repeat ask is answered consistently.
    documents: TtlCache<(String, String), Option<LyricDocument>>,
    negative: NegativeCache,
}
impl LyricsCache {
    pub fn new(config: LyricsCacheConfig) -> Self {
        let capacity = config.capacity;
        Self {
            search: TtlCache::new(config.search_ttl, capacity),
            documents: TtlCache::new(config.document_ttl, capacity),
            negative: NegativeCache::new(config.negative, capacity),
        }
    }
    /// The negative domain (session-refresh face, per-source negatives).
    pub fn negative(&self) -> &NegativeCache {
        &self.negative
    }

    // ---- search domain ----

    /// `None` = not cached; `Some(vec![])` = cached absence.
    pub fn search_get(&self, id: &TrackId, mode: SearchMode) -> Option<Vec<LyricSearchResult>> {
        self.search.get(&(id.clone(), mode))
    }
    /// Store a result list; an empty list is a cached absence, symmetric
    /// with the document domain (RESOURCE_DESIGN.md §8.2).
    pub fn search_put(&self, id: &TrackId, mode: SearchMode, results: &[LyricSearchResult]) {
        self.search.insert((id.clone(), mode), results.to_vec());
    }
    /// Store a result list with a shortened TTL (the partial-success face:
    /// a provisional entry expires together with the failing sources'
    /// negative window so the candidate set never stays narrowed past it).
    pub fn search_put_with_ttl(
        &self,
        id: &TrackId,
        mode: SearchMode,
        results: &[LyricSearchResult],
        ttl: Duration,
    ) {
        self.search
            .insert_with_ttl((id.clone(), mode), results.to_vec(), ttl);
    }
    pub fn search_invalidate(&self, id: &TrackId, mode: SearchMode) {
        self.search.invalidate(&(id.clone(), mode));
    }

    // ---- document domain ----

    /// `None` = not cached; `Some(None)` = cached absence.
    pub fn document_get(&self, provider: &str, id: &str) -> Option<Option<LyricDocument>> {
        self.documents.get(&(provider.to_owned(), id.to_owned()))
    }
    pub fn document_put(&self, provider: &str, id: &str, document: Option<&LyricDocument>) {
        self.documents
            .insert((provider.to_owned(), id.to_owned()), document.cloned());
    }
    pub fn document_invalidate(&self, provider: &str, id: &str) {
        self.documents
            .invalidate(&(provider.to_owned(), id.to_owned()));
    }

    // ---- negative domain ----

    pub fn search_record_negative(
        &self,
        id: &TrackId,
        mode: SearchMode,
        source: &str,
        category: ErrorCategory,
    ) {
        self.negative.record(
            NegativeKey::LyricsSearch {
                id: id.clone(),
                mode: mode.key_part(),
                provider: source.to_owned(),
            },
            source,
            category,
        );
    }
    pub fn search_check_negative(&self, id: &TrackId, mode: SearchMode, source: &str) -> bool {
        self.negative
            .check(&NegativeKey::LyricsSearch {
                id: id.clone(),
                mode: mode.key_part(),
                provider: source.to_owned(),
            })
            .is_some()
    }
    pub fn search_invalidate_negative(&self, id: &TrackId, mode: SearchMode, source: &str) {
        self.negative.invalidate(&NegativeKey::LyricsSearch {
            id: id.clone(),
            mode: mode.key_part(),
            provider: source.to_owned(),
        });
    }
    /// The remaining lifetime of one source's negative window for this
    /// query (`None` when not negatively cached or already expired). The
    /// alignment face for provisional positive entries (RESOURCE_DESIGN.md
    /// §8.3): a narrowed result set expires together with the blocked
    /// windows that caused the narrowing.
    pub fn search_negative_remaining(
        &self,
        id: &TrackId,
        mode: SearchMode,
        source: &str,
    ) -> Option<Duration> {
        self.negative.remaining_ttl(&NegativeKey::LyricsSearch {
            id: id.clone(),
            mode: mode.key_part(),
            provider: source.to_owned(),
        })
    }
    pub fn document_record_negative(&self, provider: &str, id: &str, category: ErrorCategory) {
        self.negative.record(
            NegativeKey::LyricsDocument {
                provider: provider.to_owned(),
                id: id.to_owned(),
            },
            provider,
            category,
        );
    }
    pub fn document_check_negative(&self, provider: &str, id: &str) -> Option<NegativeHit> {
        self.negative.check(&NegativeKey::LyricsDocument {
            provider: provider.to_owned(),
            id: id.to_owned(),
        })
    }
    pub fn document_invalidate_negative(&self, provider: &str, id: &str) {
        self.negative.invalidate(&NegativeKey::LyricsDocument {
            provider: provider.to_owned(),
            id: id.to_owned(),
        });
    }
    /// Entries across both positive domains (the negative domain is
    /// queried through [`LyricsCache::negative`]).
    pub fn len(&self) -> usize {
        self.search.len() + self.documents.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Drop everything in all three domains.
    pub fn clear(&self) {
        self.search.clear();
        self.documents.clear();
        self.negative.clear();
    }
}

/// Rebuild a category-faithful `Error` for a negative-cache hit
/// (RESOURCE_DESIGN.md §8.3): the cached category is mapped back onto its
/// M2 variant, so `is_retryable()` and the health classification survive
/// the negative path (`RateLimited` stays retryable, credential states
/// stay credential states) instead of collapsing into the legacy
/// `Provider` catch-all. Categories whose variant needs data the negative
/// domain does not store (`UnsupportedQuality` needs the quality tier)
/// fall back to `Error::Provider`; the non-cacheable categories can never
/// reach the negative domain at all.
fn cached_error(provider: &str, category: ErrorCategory) -> Error {
    use ErrorCategory::*;
    let provider_some = Some(provider.to_owned());
    let detail = Some(ErrorDetail::new("negative-cached provider failure"));
    match category {
        RateLimited => Error::RateLimited {
            provider: provider_some,
            retry_after: None,
            detail,
        },
        Network => Error::Network {
            provider: provider_some,
            detail,
        },
        Timeout => Error::Timeout,
        Unavailable => Error::Unavailable {
            provider: provider_some,
            detail,
        },
        SessionExpired => Error::SessionExpired {
            provider: provider_some,
            detail,
        },
        AuthenticationRequired => Error::AuthenticationRequired {
            provider: provider_some,
            detail,
        },
        AuthenticationFailed => Error::AuthenticationFailed {
            provider: provider_some,
            code: None,
            detail,
        },
        InvalidResponse => Error::InvalidResponse {
            provider: provider_some,
            code: None,
            detail,
        },
        Protocol => Error::Protocol {
            provider: provider_some,
            detail,
        },
        ProviderInternal => Error::ProviderInternal {
            provider: provider_some,
            detail,
        },
        PermissionDenied => Error::PermissionDenied {
            provider: provider_some,
            code: None,
            detail,
        },
        VipRequired => Error::VipRequired {
            provider: provider_some,
            detail,
        },
        RegionRestricted => Error::RegionRestricted {
            provider: provider_some,
            detail,
        },
        CopyrightRestricted => Error::CopyrightRestricted {
            provider: provider_some,
            detail,
        },
        ExpiredResource => Error::ExpiredResource {
            provider: provider_some,
            detail,
        },
        NotFound => Error::NotFound,
        // Reconstructible only with data the negative domain does not keep
        // (the quality tier), or the unclassified legacy catch-all itself.
        UnsupportedQuality | Provider => Error::Provider {
            provider: provider.to_owned(),
            message: "negative-cached provider failure".into(),
        },
        // Non-cacheable categories never enter the negative domain; this
        // arm exists for totality only.
        _ => Error::Provider {
            provider: provider.to_owned(),
            message: "negative-cached provider failure".into(),
        },
    }
}

/// Provider-direct lyrics with cache support (RESOURCE_DESIGN.md §8.3):
/// document-domain short-circuit, negative short-circuit (the hit rebuilds
/// a category-faithful M2 error via `cached_error`, so retryability and
/// health classification survive; the exact original payload text is not
/// reconstructible from a category, hosts wanting it can
/// `document_invalidate_negative` and retry), category-accurate recording
/// on failure, explicit absence cached as `Some(None)` so a repeat ask
/// answers `Ok(None)` consistently, and a successful resolution
/// invalidates its stale negatives. Cross-source refusal stays the
/// provider's own guarantee (the provider id is never swapped).
pub async fn lyrics_for_track(
    registry: &ProviderRegistry,
    track: &Track,
    cache: &LyricsCache,
) -> crate::Result<Option<LyricDocument>> {
    let provider_id = track.id.provider.as_str();
    let resource_id = track.id.id.as_str();
    if let Some(cached) = cache.document_get(provider_id, resource_id) {
        return Ok(cached);
    }
    if let Some(hit) = cache.document_check_negative(provider_id, resource_id) {
        return Err(cached_error(&hit.provider, hit.category));
    }
    let Some(provider) = registry.lyrics_provider(provider_id) else {
        return Err(Error::Unsupported(provider_id.to_owned()));
    };
    match provider.lyrics(track).await {
        Ok(outcome) => {
            cache.document_put(provider_id, resource_id, outcome.as_ref());
            cache.document_invalidate_negative(provider_id, resource_id);
            Ok(outcome)
        }
        Err(error) => {
            let category = error.category();
            cache.document_record_negative(provider_id, resource_id, category);
            Err(error)
        }
    }
}

/// The cached-search orchestration, injectable over the live searcher face
/// so the cache policy (absence caching, provisional partial entries,
/// blocked-source error surfacing) is unit-testable without network.
/// Contract (RESOURCE_DESIGN.md §8.3):
///
/// 1. per-source negatives split the source list first: blocked sources
///    are skipped and their skip is reported as an error tuple, so a
///    cached list never silently narrows the candidate set;
/// 2. a positive entry answers immediately (with the blocked errors when
///    any source was skipped);
/// 3. a full-freshness entry (empty results included, a cached absence) is
///    stored ONLY when every source of the call participated: any blocked
///    or failed source caps the entry at the shortest window that caused
///    the narrowing (the failing source's category TTL, the blocked
///    source's remaining negative window), so the narrowed candidate set
///    never outlives the state that caused it;
/// 4. a cancelled run (the runner reports `Cancelled` failures) stores
///    nothing: Cancelled is non-cacheable and `Cancelled`-only failures
///    yield a zero provisional window, so a cancelled query can never
///    poison the positive domain with a fake absence;
/// 5. live failures are recorded per source with their categories
///    (non-cacheable categories are no-ops in the negative domain).
#[cfg(feature = "lyrics-search")]
pub(crate) async fn search_cached_with<F, Fut>(
    sources: &[crate::lyrics::LyricsSearchSource],
    track: &Track,
    mode: SearchMode,
    cache: &LyricsCache,
    run: F,
) -> (Vec<LyricSearchResult>, Vec<(String, String)>)
where
    F: Fn(Vec<crate::lyrics::LyricsSearchSource>) -> Fut,
    Fut: std::future::Future<
            Output = (
                Vec<LyricSearchResult>,
                Vec<crate::lyrics::LyricsSourceFailure>,
            ),
        >,
{
    use crate::lyrics::LyricsSearchSource;
    // Per-source negatives remove their source from this attempt
    // (RESOURCE_DESIGN.md §8.2): one blocked source must not suppress the
    // healthy ones, and the skip is visible to the caller. The remaining
    // windows ride along: they cap any entry stored by this call.
    let mut blocked: Vec<(String, String)> = Vec::new();
    let mut blocked_windows: Vec<Duration> = Vec::new();
    let mut live: Vec<LyricsSearchSource> = Vec::new();
    for source in sources {
        if cache.search_check_negative(&track.id, mode, source.id()) {
            blocked.push((
                source.id().to_owned(),
                "negative-cached lyrics search failure".into(),
            ));
            if let Some(remaining) = cache.search_negative_remaining(&track.id, mode, source.id()) {
                blocked_windows.push(remaining);
            }
        } else {
            live.push(*source);
        }
    }
    if live.is_empty() {
        return (
            cache.search_get(&track.id, mode).unwrap_or_default(),
            blocked,
        );
    }
    // Positive entries answer for the sources this call can serve; the
    // blocked skips ride along as errors instead of disappearing.
    if let Some(results) = cache.search_get(&track.id, mode) {
        return (results, blocked);
    }
    let (results, failures) = run(live).await;
    for failure in &failures {
        cache.search_record_negative(&track.id, mode, failure.source, failure.category);
    }
    // Windows that cap this call's entry: failing sources' category TTLs
    // and the blocked sources' remaining windows. A `Cancelled` failure
    // contributes none (non-cacheable), so a cancelled run stores nothing
    // at all instead of a fake absence.
    let mut windows: Vec<Duration> = failures
        .iter()
        .filter_map(|failure| cache.negative().ttl_for(failure.category))
        .collect();
    windows.extend(blocked_windows);
    if failures.is_empty() && blocked.is_empty() {
        // Every source participated and succeeded: full-freshness entry,
        // empty results included (a cached absence).
        cache.search_put(&track.id, mode, &results);
    } else if !results.is_empty() && !windows.is_empty() {
        // Narrowed result set (some source failed or was blocked):
        // provisional entry, expiring together with the shortest window
        // that caused the narrowing so the candidate set never outlives
        // it. When every failure is non-cacheable (no window), the put is
        // SKIPPED: a TTL-0 entry would be born expired and occupy a
        // capacity slot until the next insert.
        let provisional = windows.iter().copied().min().unwrap_or_default();
        cache.search_put_with_ttl(&track.id, mode, &results, provisional);
    }
    let mut errors = blocked;
    errors.extend(
        failures
            .into_iter()
            .map(|failure| (failure.source.to_owned(), failure.message)),
    );
    (results, errors)
}

/// Cached face of the lyrics search (RESOURCE_DESIGN.md §8.3, gated
/// `lyrics-cache` + `lyrics-search`): the orchestration of
/// `search_cached_with` over the live `search_many_typed` searcher face.
#[cfg(feature = "lyrics-search")]
pub async fn search_many_cached(
    sources: &[crate::lyrics::LyricsSearchSource],
    track: &Track,
    full_search: bool,
    per_source_timeout: Duration,
    cache: &LyricsCache,
    health: Option<&crate::health::ProviderHealthMonitor>,
    cancel: Option<&crate::search::SearchCancelToken>,
) -> (Vec<LyricSearchResult>, Vec<(String, String)>) {
    let mode = SearchMode::from_full_search(full_search);
    // `async move` so the runner owns the filtered source list (the future
    // must not borrow the closure parameter) while the shared references
    // ride along as copies.
    search_cached_with(sources, track, mode, cache, |live| async move {
        crate::lyrics::search_many_typed(
            &live,
            track,
            full_search,
            per_source_timeout,
            health,
            cancel,
        )
        .await
    })
    .await
}

#[cfg(test)]
mod tests {
    /// P2-T2: the three domains each honor the same configured capacity (the
    /// config carries ONE capacity value; each domain owns its own bound).
    /// Filling the document domain past capacity evicts the oldest; the
    /// negative domain shares the same bound value on its own entries.
    #[test]
    fn each_domain_honors_the_configured_capacity() {
        let cache = LyricsCache::new(LyricsCacheConfig {
            capacity: 4,
            ..LyricsCacheConfig::default()
        });

        // Document domain: 8 distinct keys, capacity 4 → only the newest
        // four survive.
        for index in 0..8 {
            let provider = format!("provider-{index}");
            cache.document_put(&provider, "id", None);
        }
        assert!(
            cache.document_get("provider-0", "id").is_none(),
            "oldest document entry evicted at capacity"
        );
        assert!(
            cache.document_get("provider-7", "id").is_some(),
            "newest document entry retained"
        );

        // Negative domain (same bound value, its own entries): 6 distinct
        // cacheable-category keys, capacity 4 → oldest two gone.
        for index in 0..6 {
            let provider = format!("neg-{index}");
            cache.document_record_negative(&provider, "id", ErrorCategory::NotFound);
        }
        assert!(
            cache.document_check_negative("neg-0", "id").is_none(),
            "oldest negative evicted at capacity"
        );
        assert!(
            cache.document_check_negative("neg-5", "id").is_some(),
            "newest negative retained"
        );
    }

    use super::*;
    use crate::models::{LyricFormat, LyricLine};

    fn query_track() -> Track {
        Track {
            id: TrackId::new("netease", "42"),
            title: "query song".into(),
            artists: vec!["a".into()],
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

    fn document() -> LyricDocument {
        LyricDocument {
            format: LyricFormat::Lrc,
            original: "[00:01.00]hi".into(),
            translation: None,
            romanization: None,
            lines: vec![LyricLine {
                start_ms: 1_000,
                end_ms: None,
                text: "hi".into(),
                translations: Default::default(),
                romanization: None,
                words: Vec::new(),
            }],
        }
    }

    fn result(provider: &str, id: &str) -> LyricSearchResult {
        LyricSearchResult {
            provider: provider.to_owned(),
            id: id.to_owned(),
            title: "irrelevant".into(),
            artists: vec![],
            album: None,
            duration_ms: None,
            match_score: Some(80),
        }
    }

    /// M6 §8.2 (R12): the document domain caches documents and explicit
    /// absence consistently, keyed by (provider, resource id).
    #[test]
    fn document_domain_caches_documents_and_absence() {
        let cache = LyricsCache::new(LyricsCacheConfig::default());
        assert_eq!(cache.document_get("netease", "42"), None);
        cache.document_put("netease", "42", Some(&document()));
        assert_eq!(
            cache.document_get("netease", "42").unwrap().unwrap().format,
            LyricFormat::Lrc
        );
        // A different provider id never sees the entry.
        assert_eq!(cache.document_get("qq", "42"), None);
        cache.document_put("qq", "42", None);
        assert_eq!(cache.document_get("qq", "42"), Some(None));
        assert_eq!(cache.len(), 2);
        cache.document_invalidate("netease", "42");
        assert_eq!(cache.document_get("netease", "42"), None);
    }

    #[test]
    fn document_negatives_classify_and_expire() {
        let cache = LyricsCache::new(LyricsCacheConfig::default());
        cache.document_record_negative("netease", "42", ErrorCategory::RateLimited);
        let hit = cache
            .document_check_negative("netease", "42")
            .expect("live negative");
        assert_eq!(hit.provider, "netease");
        assert_eq!(hit.category, ErrorCategory::RateLimited);
        cache.document_invalidate_negative("netease", "42");
        assert!(cache.document_check_negative("netease", "42").is_none());
        // Caller-fault categories are never cached.
        cache.document_record_negative("netease", "42", ErrorCategory::InvalidInput);
        assert!(cache.document_check_negative("netease", "42").is_none());
    }

    /// M6 §8.2: the search domain keys by TrackId + mode; the two modes
    /// never share entries; an empty list is a cached absence, symmetric
    /// with the document domain.
    #[test]
    fn search_domain_keys_by_track_id_and_mode() {
        let cache = LyricsCache::new(LyricsCacheConfig::default());
        let track = query_track();
        cache.search_put(&track.id, SearchMode::Full, &[result("lrclib", "1")]);
        assert_eq!(
            cache.search_get(&track.id, SearchMode::Full).unwrap().len(),
            1
        );
        assert_eq!(cache.search_get(&track.id, SearchMode::Refine), None);
        let other = TrackId::new("netease", "43");
        assert_eq!(cache.search_get(&other, SearchMode::Full), None);
        // Empty list = cached absence (symmetric with documents Some(None)).
        cache.search_put(&track.id, SearchMode::Refine, &[]);
        assert_eq!(
            cache.search_get(&track.id, SearchMode::Refine),
            Some(Vec::new())
        );
        // Per-source negatives.
        cache.search_record_negative(&track.id, SearchMode::Full, "qq", ErrorCategory::Timeout);
        assert!(cache.search_check_negative(&track.id, SearchMode::Full, "qq"));
        assert!(!cache.search_check_negative(&track.id, SearchMode::Full, "lrclib"));
        assert!(!cache.search_check_negative(&track.id, SearchMode::Refine, "qq"));
        cache.search_invalidate_negative(&track.id, SearchMode::Full, "qq");
        assert!(!cache.search_check_negative(&track.id, SearchMode::Full, "qq"));
        // Session-refresh face on the shared negative domain.
        cache.search_record_negative(
            &track.id,
            SearchMode::Refine,
            "lrclib",
            ErrorCategory::SessionExpired,
        );
        cache.negative().clear_provider("lrclib");
        assert!(!cache.search_check_negative(&track.id, SearchMode::Refine, "lrclib"));
    }

    /// m6-lyrics-helper-audit: the D28 alignment face passthrough.
    /// `search_negative_remaining` exposes one source's remaining negative
    /// window (the `NegativeCache::remaining_ttl` read) so the orchestration
    /// can cap a narrowed positive entry at the blocked windows; live window,
    /// absent source and invalidated source all behave as the cache face.
    #[test]
    fn search_negative_remaining_passthrough_tracks_the_window() {
        let cache = LyricsCache::new(LyricsCacheConfig::default());
        let track = query_track();
        // No negative recorded: no window.
        assert_eq!(
            cache.search_negative_remaining(&track.id, SearchMode::Full, "lrclib"),
            None
        );
        cache.search_record_negative(
            &track.id,
            SearchMode::Full,
            "lrclib",
            ErrorCategory::RateLimited,
        );
        let window = cache
            .search_negative_remaining(&track.id, SearchMode::Full, "lrclib")
            .expect("live window after recording");
        // The standard RateLimited window is 15 s; the passthrough must be
        // the entry's remaining lifetime, never the full policy TTL again.
        assert!(window <= Duration::from_secs(15) && window > Duration::ZERO);
        cache.search_invalidate_negative(&track.id, SearchMode::Full, "lrclib");
        assert_eq!(
            cache.search_negative_remaining(&track.id, SearchMode::Full, "lrclib"),
            None,
            "invalidated source reports no window"
        );
    }

    /// Review P2: a negative hit rebuilds a category-faithful M2 error, so
    /// retryability and health classification survive the negative path.
    #[test]
    fn cached_errors_rebuild_their_m2_variant() {
        let retryable = cached_error("netease", ErrorCategory::RateLimited);
        assert_eq!(retryable.category(), ErrorCategory::RateLimited);
        assert!(retryable.is_retryable(), "RateLimited stays retryable");
        assert_eq!(retryable.provider_id(), Some("netease"));
        let network = cached_error("netease", ErrorCategory::Network);
        assert!(network.is_retryable());
        let expired = cached_error("qqmusic", ErrorCategory::SessionExpired);
        assert_eq!(expired.category(), ErrorCategory::SessionExpired);
        assert!(
            !expired.is_retryable(),
            "credential states do not blind-retry"
        );
        let absent = cached_error("netease", ErrorCategory::NotFound);
        assert_eq!(absent.category(), ErrorCategory::NotFound);
        // The one category whose variant needs unstored data degrades to
        // the legacy catch-all, message-free of Debug formatting.
        let degraded = cached_error("netease", ErrorCategory::UnsupportedQuality);
        assert_eq!(degraded.category(), ErrorCategory::Provider);
        assert_eq!(
            degraded.to_string(),
            "provider netease failed: negative-cached provider failure"
        );
    }

    // ---- search orchestration over an injectable runner ----

    #[cfg(feature = "lyrics-search")]
    mod orchestration {
        use super::*;
        use crate::lyrics::{LyricsSearchSource, LyricsSourceFailure};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// The boxed runner future (type alias keeps the signature readable).
        type FakeRun = std::pin::Pin<
            Box<
                dyn std::future::Future<Output = (Vec<LyricSearchResult>, Vec<LyricsSourceFailure>)>,
            >,
        >;

        /// Runner stub: success set + failure set, counting invocations.
        /// `no_results` turns every successful source into "answered
        /// successfully with nothing" (the cached-absence scenario).
        struct FakeSearch {
            succeed: Vec<&'static str>,
            fail: Vec<(&'static str, ErrorCategory)>,
            no_results: bool,
            calls: Arc<AtomicUsize>,
        }
        impl FakeSearch {
            fn run(&self, live: Vec<LyricsSearchSource>) -> FakeRun {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let mut results = Vec::new();
                let mut failures = Vec::new();
                for source in &live {
                    let id = source.id();
                    if self.succeed.contains(&id) && !self.no_results {
                        results.push(result(id, "1"));
                    }
                    if let Some((_, category)) = self.fail.iter().find(|(name, _)| *name == id) {
                        failures.push(LyricsSourceFailure {
                            source: id,
                            category: *category,
                            message: format!("fake failure ({id})"),
                        });
                    }
                }
                Box::pin(async move { (results, failures) })
            }
        }

        fn sources() -> Vec<LyricsSearchSource> {
            vec![LyricsSearchSource::Netease, LyricsSearchSource::LRCLIB]
        }

        /// Review P2: a partial success is stored as a provisional entry
        /// aligned with the failing source's negative window, and the
        /// failures are visible in the returned errors.
        #[tokio::test]
        async fn partial_success_is_provisional_and_reports_its_failures() {
            let config = LyricsCacheConfig {
                search_ttl: Duration::from_secs(60),
                negative: NegativeTtlPolicy {
                    per_category: std::collections::BTreeMap::from([(
                        ErrorCategory::RateLimited,
                        Duration::from_millis(60),
                    )]),
                    ..NegativeTtlPolicy::standard()
                },
                ..LyricsCacheConfig::default()
            };
            let cache = LyricsCache::new(config);
            let track = query_track();
            let fake = FakeSearch {
                succeed: vec!["netease"],
                fail: vec![("lrclib", ErrorCategory::RateLimited)],
                no_results: false,
                calls: Arc::new(AtomicUsize::new(0)),
            };
            let (results, errors) =
                search_cached_with(&sources(), &track, SearchMode::Full, &cache, |live| {
                    fake.run(live)
                })
                .await;
            assert_eq!(results.len(), 1, "the healthy source's results survive");
            assert_eq!(errors.len(), 1, "the failure is visible");
            assert_eq!(errors[0].0, "lrclib");
            // Provisional entry present...
            assert!(cache.search_get(&track.id, SearchMode::Full).is_some());
            assert!(cache.search_check_negative(&track.id, SearchMode::Full, "lrclib"));
            // ...and it expires together with the negative window: the
            // candidate set never stays narrowed past it.
            std::thread::sleep(Duration::from_millis(120));
            assert_eq!(cache.search_get(&track.id, SearchMode::Full), None);
            assert!(!cache.search_check_negative(&track.id, SearchMode::Full, "lrclib"));
        }

        /// Review P2: an all-successful empty search is a cached absence,
        /// symmetric with the document domain; the second call is a pure
        /// cache hit.
        #[tokio::test]
        async fn empty_success_is_cached_as_absence() {
            let cache = LyricsCache::new(LyricsCacheConfig::default());
            let track = query_track();
            let fake = FakeSearch {
                succeed: vec!["netease", "lrclib"],
                fail: vec![],
                no_results: true,
                calls: Arc::new(AtomicUsize::new(0)),
            };
            let (results, errors) =
                search_cached_with(&sources(), &track, SearchMode::Full, &cache, |live| {
                    fake.run(live)
                })
                .await;
            assert!(results.is_empty());
            assert!(errors.is_empty());
            assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                cache.search_get(&track.id, SearchMode::Full),
                Some(Vec::new())
            );
            let (results, errors) =
                search_cached_with(&sources(), &track, SearchMode::Full, &cache, |live| {
                    fake.run(live)
                })
                .await;
            assert!(results.is_empty());
            assert!(errors.is_empty());
            assert_eq!(
                fake.calls.load(Ordering::SeqCst),
                1,
                "the second call is answered from the absence entry"
            );
        }

        /// Review P2: a positive hit never hides a blocked source; the
        /// skip is surfaced as an error tuple.
        #[tokio::test]
        async fn positive_hit_surfaces_blocked_sources() {
            let cache = LyricsCache::new(LyricsCacheConfig::default());
            let track = query_track();
            cache.search_put(
                &track.id,
                SearchMode::Full,
                &[result("netease", "1"), result("lrclib", "2")],
            );
            cache.search_record_negative(
                &track.id,
                SearchMode::Full,
                "lrclib",
                ErrorCategory::Timeout,
            );
            let fake = FakeSearch {
                succeed: vec![],
                fail: vec![],
                no_results: false,
                calls: Arc::new(AtomicUsize::new(0)),
            };
            let (results, errors) =
                search_cached_with(&sources(), &track, SearchMode::Full, &cache, |live| {
                    fake.run(live)
                })
                .await;
            assert_eq!(results.len(), 2, "the cached list is served");
            assert_eq!(errors.len(), 1, "the blocked skip is visible");
            assert_eq!(errors[0].0, "lrclib");
            assert_eq!(
                fake.calls.load(Ordering::SeqCst),
                0,
                "no searcher ran for the blocked source"
            );
        }

        /// Review P2 (round 2, issue 1): a cancelled run reports
        /// `Cancelled` failures and stores nothing. The pre-fix engine
        /// returned a silent empty success for a cancelled run, which the
        /// cache then stored as a full-freshness "no lyrics exist" absence
        /// (up to the full search TTL) and every later query short-circuited
        /// on.
        #[tokio::test]
        async fn cancelled_run_is_never_cached_as_absence() {
            let cache = LyricsCache::new(LyricsCacheConfig::default());
            let track = query_track();
            let fake = FakeSearch {
                succeed: vec!["netease", "lrclib"],
                fail: vec![
                    ("netease", ErrorCategory::Cancelled),
                    ("lrclib", ErrorCategory::Cancelled),
                ],
                no_results: true,
                calls: Arc::new(AtomicUsize::new(0)),
            };
            let (results, errors) =
                search_cached_with(&sources(), &track, SearchMode::Full, &cache, |live| {
                    fake.run(live)
                })
                .await;
            assert!(results.is_empty());
            assert_eq!(errors.len(), 2, "the cancelled run is visible");
            assert_eq!(
                cache.search_get(&track.id, SearchMode::Full),
                None,
                "a cancelled query must not poison the positive domain with a fake absence"
            );
        }

        /// Review P2 (round 2, issue 2): a narrowed success (some sources
        /// blocked, the rest successful) is provisional, expiring together
        /// with the blocked windows, so the narrowed candidate set never
        /// outlives the state that caused it even when every live source
        /// succeeded.
        #[tokio::test]
        async fn narrowed_success_with_blocked_sources_is_provisional() {
            let config = LyricsCacheConfig {
                search_ttl: Duration::from_secs(60),
                negative: NegativeTtlPolicy {
                    per_category: std::collections::BTreeMap::from([(
                        ErrorCategory::RateLimited,
                        Duration::from_millis(60),
                    )]),
                    ..NegativeTtlPolicy::standard()
                },
                ..LyricsCacheConfig::default()
            };
            let cache = LyricsCache::new(config);
            let track = query_track();
            // One source blocked with a short window; the rest succeed.
            cache.search_record_negative(
                &track.id,
                SearchMode::Full,
                "lrclib",
                ErrorCategory::RateLimited,
            );
            let fake = FakeSearch {
                succeed: vec!["netease"],
                fail: vec![],
                no_results: false,
                calls: Arc::new(AtomicUsize::new(0)),
            };
            let (results, errors) =
                search_cached_with(&sources(), &track, SearchMode::Full, &cache, |live| {
                    fake.run(live)
                })
                .await;
            assert_eq!(results.len(), 1);
            assert_eq!(errors.len(), 1, "the blocked skip is visible");
            // The narrowed entry exists now...
            assert!(cache.search_get(&track.id, SearchMode::Full).is_some());
            // ...but it expires with the blocked window, not after the full
            // search TTL: the blocked source gets its re-query chance.
            std::thread::sleep(Duration::from_millis(120));
            assert_eq!(
                cache.search_get(&track.id, SearchMode::Full),
                None,
                "the narrowed set must not outlive the blocked window"
            );
            assert!(!cache.search_check_negative(&track.id, SearchMode::Full, "lrclib"));
        }
    }
}
