//! In-memory, expiry-aware caches (M6 core, RESOURCE_DESIGN.md §2).
//!
//! Three faces share one rule set: stable keys (provider ids, `TrackId`,
//! URLs, content hashes; never a song title), a hard capacity with
//! earliest-expiry eviction, and check-and-purge expiry on access. The
//! stream cache keeps its pre-M6 public API byte-for-byte; the namespace,
//! per-provider TTL, generic `TtlCache` and `NegativeCache` faces are pure
//! additions.
//!
//! Lock poisoning is tolerated everywhere (`unwrap_or_else(|e|
//! e.into_inner())`): a panicking reader must not wedge the pipeline.
use crate::error::ErrorCategory;
use crate::models::{StreamInfo, StreamQuality, TrackId};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::RwLock,
    time::{Duration, Instant},
};

/// The namespace of the legacy unnamed `StreamCache` API (RESOURCE_DESIGN.md
/// §2.4): every pre-M6 caller reads and writes this domain.
const GLOBAL_NAMESPACE: &str = "";

struct Entry {
    value: StreamInfo,
    expires_at: Instant,
}
pub struct StreamCache {
    /// Keyed by `(namespace, track id, requested quality)`. The namespace is
    /// the M6 session domain (§2.4): same provider, different account =
    /// disjoint entries. The empty namespace is the legacy global domain.
    entries: RwLock<HashMap<(String, TrackId, StreamQuality), Entry>>,
    ttl: Duration,
    /// Per-provider TTL overrides (§2.5). `None` for a provider falls back to
    /// `ttl`. Interior-mutable so the cache stays `&self` everywhere.
    provider_ttls: RwLock<BTreeMap<String, Duration>>,
    capacity: usize,
}
impl StreamCache {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
            provider_ttls: RwLock::new(BTreeMap::new()),
            capacity: capacity.max(1),
        }
    }
    /// Override the TTL for one provider's entries. `None` removes the
    /// override (back to the constructor default). The absolute URL expiry
    /// cap still applies on top (§2.5).
    pub fn set_provider_ttl(&self, provider: &str, ttl: Option<Duration>) {
        let mut ttls = self
            .provider_ttls
            .write()
            .unwrap_or_else(|e| e.into_inner());
        match ttl {
            Some(ttl) => {
                ttls.insert(provider.to_owned(), ttl);
            }
            None => {
                ttls.remove(provider);
            }
        }
    }
    /// The effective TTL for a track's provider: the override when present,
    /// else the constructor default (RESOURCE_DESIGN.md §2.5).
    fn effective_ttl(&self, id: &TrackId) -> Duration {
        self.provider_ttls
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id.provider)
            .copied()
            .unwrap_or(self.ttl)
    }
    pub fn get(&self, id: &TrackId, quality: StreamQuality) -> Option<StreamInfo> {
        self.get_in(GLOBAL_NAMESPACE, id, quality)
    }
    /// Read inside one namespace (§2.4). A legacy `get` is namespace `""`.
    pub fn get_in(
        &self,
        namespace: &str,
        id: &TrackId,
        quality: StreamQuality,
    ) -> Option<StreamInfo> {
        let key = (namespace.to_owned(), id.clone(), quality);
        // Double-checked read (see TtlCache::get): hits take the read lock,
        // only expired hits escalate to the write lock.
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(&key)?;
        if entry.expires_at > Instant::now() {
            return Some(entry.value.clone());
        }
        drop(entries);
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        match entries.get(&key) {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.value.clone()),
            Some(_) => {
                entries.remove(&key);
                None
            }
            None => None,
        }
    }
    pub fn insert(&self, id: TrackId, quality: StreamQuality, value: StreamInfo) {
        self.insert_in(GLOBAL_NAMESPACE, id, quality, value)
    }
    /// Insert inside one namespace (§2.4). TTL: the provider override when
    /// one is set, else the constructor default, then capped by the URL's
    /// own `expires_at_unix` exactly as before (cache.rs:40-52 rule).
    pub fn insert_in(
        &self,
        namespace: &str,
        id: TrackId,
        quality: StreamQuality,
        value: StreamInfo,
    ) {
        let ttl = self.effective_ttl(&id);
        let now = Instant::now();
        let expiry = match value.expires_at_unix {
            Some(unix) => {
                let absolute =
                    std::time::SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(unix));
                match absolute
                    .and_then(|time| time.duration_since(std::time::SystemTime::now()).ok())
                {
                    Some(remaining) => now + remaining.min(ttl),
                    None => now,
                }
            }
            None => now + ttl,
        };
        let key = (namespace.to_owned(), id.clone(), quality);
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, entry| entry.expires_at > now);
        if entries.len() >= self.capacity
            && !entries.contains_key(&key)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            Entry {
                value,
                expires_at: expiry,
            },
        );
    }
    /// A scoped view of one namespace (§2.4). The intended namespace content
    /// is `"{provider}\u{1f}{account_id}"` from `SessionStore` keys
    /// (session.rs), so two accounts of one provider never share URL
    /// entries. Entries live in the same bounded map, so a namespace can
    /// never exceed the cache capacity.
    pub fn namespaced(&self, namespace: &str) -> NamespacedStreamCache<'_> {
        NamespacedStreamCache {
            cache: self,
            namespace: namespace.to_owned(),
        }
    }
    /// Drop every entry in every namespace. The legacy face clears wide on
    /// purpose: a "clear" from a caller that never learned about namespaces
    /// must not leave stale entries behind in one.
    pub fn clear(&self) {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    /// Total entry count across all namespaces (the bounded map is shared).
    pub fn len(&self) -> usize {
        self.entries.read().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Borrowing per-namespace view over a [`StreamCache`] (RESOURCE_DESIGN.md
/// §2.4). Cheap to construct; all storage stays in the parent cache. Debug
/// shows only the namespace: the parent cache's Debug is not derived and
/// its values are the host's data.
pub struct NamespacedStreamCache<'a> {
    cache: &'a StreamCache,
    namespace: String,
}
impl std::fmt::Debug for NamespacedStreamCache<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamespacedStreamCache")
            .field("namespace", &self.namespace)
            .finish()
    }
}
impl NamespacedStreamCache<'_> {
    pub fn get(&self, id: &TrackId, quality: StreamQuality) -> Option<StreamInfo> {
        self.cache.get_in(&self.namespace, id, quality)
    }
    pub fn insert(&self, id: TrackId, quality: StreamQuality, value: StreamInfo) {
        self.cache.insert_in(&self.namespace, id, quality, value)
    }
    pub fn is_empty(&self) -> bool {
        self.cache
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .all(|(namespace, _, _)| namespace != &self.namespace)
    }
}

/// Generic bounded TTL cache (RESOURCE_DESIGN.md §2.2): the storage behind
/// the lyrics and artwork domains. Same eviction and expiry rules as
/// [`StreamCache`]; values carry no absolute expiry of their own (an
/// entry-expiring value type like `StreamInfo` keeps using `StreamCache`).
pub struct TtlCache<K, V>
where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
{
    entries: RwLock<HashMap<K, TtlEntry<V>>>,
    ttl: Duration,
    capacity: usize,
}
struct TtlEntry<V> {
    value: V,
    expires_at: Instant,
}
impl<K, V> TtlCache<K, V>
where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
{
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
            capacity: capacity.max(1),
        }
    }
    pub fn get(&self, key: &K) -> Option<V> {
        // Double-checked read: the fast path holds only the read lock, so
        // concurrent cache HITS never serialize against each other; the
        // write lock is escalated only for the rare expired-hit removal.
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(key)?;
        if entry.expires_at > Instant::now() {
            return Some(entry.value.clone());
        }
        drop(entries);
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        match entries.get(key) {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.value.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }
    pub fn insert(&self, key: K, value: V) {
        self.insert_with_ttl(key, value, self.ttl);
    }
    /// Insert with an entry-specific TTL (shorter overrides only; a longer
    /// request is capped at the constructor TTL so a caller cannot extend
    /// the cache beyond its configured budget).
    pub fn insert_with_ttl(&self, key: K, value: V, ttl: Duration) {
        let ttl = ttl.min(self.ttl);
        let now = Instant::now();
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, entry| entry.expires_at > now);
        if entries.len() >= self.capacity
            && !entries.contains_key(&key)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            TtlEntry {
                value,
                expires_at: now + ttl,
            },
        );
    }
    pub fn invalidate(&self, key: &K) {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
    pub fn clear(&self) {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    pub fn len(&self) -> usize {
        self.entries.read().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What a failed request failed for, keyed by stable ids only
/// (RESOURCE_DESIGN.md §2.2/§2.3). No song title ever enters a negative key:
/// every variant carries a provider id, `TrackId`, provider-side resource
/// id, or a host-supplied pre-hashed key. (A track-search negative variant
/// existed in the first M6 draft and was removed in review: the
/// aggregation face takes no cache collaborator yet, so the variant was an
/// unwired type surface, RESOURCE_DESIGN.md §12.)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NegativeKey {
    /// Stream resolution failed for this (track, quality).
    Stream { id: TrackId, quality: StreamQuality },
    /// Lyrics search failed for this query track (stable `TrackId`) on one
    /// lyrics source; `mode` is `"full"` or `"refine"` (the two
    /// `search_many` modes return different result sets). Per-source keys:
    /// one blocked source must not suppress the healthy ones (the
    /// per-source isolation of the search face).
    LyricsSearch {
        id: TrackId,
        mode: &'static str,
        provider: String,
    },
    /// Lyrics document fetch failed for (provider, provider-side lyric id).
    LyricsDocument { provider: String, id: String },
    /// A host-supplied pre-hashed stable key. The string is stored as-is;
    /// hosts hashing their own domain keys own the title-free rule.
    Custom(String),
}

/// TTL policy of the negative domain (RESOURCE_DESIGN.md §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegativeTtlPolicy {
    /// TTL for categories with no specific entry (default 10 min).
    pub default_ttl: Duration,
    /// Per-category override (the M2 classification, error.rs).
    pub per_category: BTreeMap<ErrorCategory, Duration>,
    /// Categories that must never be cached: they describe the caller or
    /// the build, not the provider's state. `record` on these is a no-op.
    pub non_cacheable: BTreeSet<ErrorCategory>,
}
impl NegativeTtlPolicy {
    /// The documented standard policy (RESOURCE_DESIGN.md §2.3):
    /// - retryable categories (Network/Timeout/RateLimited/Unavailable) 15 s;
    /// - the whole credential trio (SessionExpired, AuthenticationRequired,
    ///   AuthenticationFailed) 30 s: "a stale credential state must not
    ///   outlive the fix for it" applies to a fresh login just as much as
    ///   to a session refresh, and [`NegativeCache::clear_provider`]
    ///   invalidates the provider's entries wholesale for both;
    /// - transient contract/protocol/internal failures (InvalidResponse,
    ///   Protocol, ProviderInternal) and the legacy unclassified
    ///   `Provider` 60 s: these are flaky-provider states, not stable
    ///   resource facts;
    /// - everything else (NotFound, region/vip/copyright, ...) 10 min;
    /// - caller/build faults not cached at all.
    pub fn standard() -> Self {
        let short = Duration::from_secs(15);
        let auth = Duration::from_secs(30);
        let light = Duration::from_secs(60);
        let per_category = BTreeMap::from([
            (ErrorCategory::Network, short),
            (ErrorCategory::Timeout, short),
            (ErrorCategory::RateLimited, short),
            (ErrorCategory::Unavailable, short),
            (ErrorCategory::SessionExpired, auth),
            (ErrorCategory::AuthenticationRequired, auth),
            (ErrorCategory::AuthenticationFailed, auth),
            (ErrorCategory::Provider, light),
            (ErrorCategory::InvalidResponse, light),
            (ErrorCategory::Protocol, light),
            (ErrorCategory::ProviderInternal, light),
        ]);
        let non_cacheable = BTreeSet::from([
            ErrorCategory::Cancelled,
            ErrorCategory::InvalidInput,
            ErrorCategory::Serialization,
            ErrorCategory::Io,
            ErrorCategory::Unsupported,
            ErrorCategory::UnsupportedOperation,
        ]);
        Self {
            default_ttl: Duration::from_secs(600),
            per_category,
            non_cacheable,
        }
    }
    /// The TTL for one category, `None` when the category must not be
    /// cached at all.
    pub fn ttl_for(&self, category: ErrorCategory) -> Option<Duration> {
        if self.non_cacheable.contains(&category) {
            return None;
        }
        Some(
            self.per_category
                .get(&category)
                .copied()
                .unwrap_or(self.default_ttl),
        )
    }
}

/// One recorded failure: who failed, with which M2 category, until when.
/// Deliberately message-free (RESOURCE_DESIGN.md §0): the category carries
/// the semantics, no provider text is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegativeHit {
    pub provider: String,
    pub category: ErrorCategory,
}

struct NegativeEntry {
    provider: String,
    category: ErrorCategory,
    expires_at: Instant,
}

/// Bounded negative cache (RESOURCE_DESIGN.md §2.2/§2.3): a repeat request
/// that would hit the same failure is answered from this domain instead of
/// re-hitting the provider, until the category TTL expires.
pub struct NegativeCache {
    entries: RwLock<HashMap<NegativeKey, NegativeEntry>>,
    policy: NegativeTtlPolicy,
    capacity: usize,
}
impl NegativeCache {
    pub fn new(policy: NegativeTtlPolicy, capacity: usize) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            policy,
            capacity: capacity.max(1),
        }
    }
    /// The configured TTL for one category (`None` when the category must
    /// not be cached). The policy passthrough for faces that need to align
    /// their own entry lifetimes with the negative window (the lyrics
    /// search domain stores partial results provisionally, RESOURCE_DESIGN.md
    /// §8.3).
    pub fn ttl_for(&self, category: ErrorCategory) -> Option<Duration> {
        self.policy.ttl_for(category)
    }
    /// The remaining lifetime of the live negative entry for `key`
    /// (`None` when absent or expired; read-only, expiry purging stays
    /// with [`NegativeCache::check`]/[`NegativeCache::record`]). The other
    /// alignment face: a caller that stores a narrowed result set while
    /// some sources are blocked caps its entry at the shortest blocked
    /// window, so the narrowed set never outlives the state that caused it
    /// (RESOURCE_DESIGN.md §8.3).
    pub fn remaining_ttl(&self, key: &NegativeKey) -> Option<Duration> {
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(key)?;
        let now = Instant::now();
        (entry.expires_at > now).then(|| entry.expires_at - now)
    }
    /// Record one failure. No-op for non-cacheable categories (§2.3).
    pub fn record(&self, key: NegativeKey, provider: &str, category: ErrorCategory) {
        let Some(ttl) = self.policy.ttl_for(category) else {
            return;
        };
        let now = Instant::now();
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        entries.retain(|_, entry| entry.expires_at > now);
        if entries.len() >= self.capacity
            && !entries.contains_key(&key)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            NegativeEntry {
                provider: provider.to_owned(),
                category,
                expires_at: now + ttl,
            },
        );
    }
    /// The live negative entry for `key`, if any. An expired entry is
    /// removed on sight (the StreamCache get rule, cache.rs:29-36).
    pub fn check(&self, key: &NegativeKey) -> Option<NegativeHit> {
        // Double-checked read (see TtlCache::get): live hits take the read
        // lock, only expired hits escalate to the write lock.
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(key)?;
        if entry.expires_at > Instant::now() {
            return Some(NegativeHit {
                provider: entry.provider.clone(),
                category: entry.category,
            });
        }
        drop(entries);
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        match entries.get(key) {
            Some(entry) if entry.expires_at > Instant::now() => Some(NegativeHit {
                provider: entry.provider.clone(),
                category: entry.category,
            }),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }
    /// Drop one key (e.g. a success invalidated its own stale negative).
    pub fn invalidate(&self, key: &NegativeKey) {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
    /// Drop every entry recorded against `provider`. The session-refresh
    /// face (RESOURCE_DESIGN.md §2.3/§6.4): a refreshed or replaced session
    /// invalidates the provider's credential-shaped failures wholesale.
    pub fn clear_provider(&self, provider: &str) {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, entry| entry.provider != provider);
    }
    pub fn clear(&self) {
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    pub fn len(&self) -> usize {
        self.entries.read().unwrap_or_else(|e| e.into_inner()).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AudioFormat, StreamQuality};
    fn sample_stream(expires_in_secs: Option<u64>) -> StreamInfo {
        let expires_at_unix = expires_in_secs.map(|secs| {
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .expect("clock before unix epoch")
                .as_secs()
                + secs
        });
        StreamInfo {
            url: "https://example.invalid/audio".into(),
            quality: StreamQuality::High,
            format: AudioFormat::Mp3,
            duration: None,
            expires_at_unix,
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
        }
    }
    #[test]
    fn expired_stream_urls_are_not_cached() {
        let cache = StreamCache::new(Duration::from_secs(300), 8);
        let stream = StreamInfo {
            url: "https://example.invalid/audio".into(),
            quality: StreamQuality::High,
            format: AudioFormat::Mp3,
            duration: None,
            expires_at_unix: Some(0),
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
        };
        let id = TrackId::new("test", "1");
        cache.insert(id.clone(), StreamQuality::High, stream);
        assert!(cache.get(&id, StreamQuality::High).is_none());
    }
    #[test]
    fn different_qualities_do_not_share_entries() {
        let cache = StreamCache::new(Duration::from_secs(60), 8);
        let id = TrackId::new("test", "q");
        cache.insert(id.clone(), StreamQuality::High, sample_stream(Some(60)));
        assert!(cache.get(&id, StreamQuality::High).is_some());
        assert!(cache.get(&id, StreamQuality::Lossless).is_none());
        cache.insert(id.clone(), StreamQuality::Lossless, sample_stream(Some(60)));
        assert_eq!(cache.len(), 2);
    }
    #[test]
    fn capacity_evicts_the_earliest_expiring_entry() {
        let cache = StreamCache::new(Duration::from_secs(300), 2);
        let a = TrackId::new("test", "a");
        let b = TrackId::new("test", "b");
        let c = TrackId::new("test", "c");
        let q = StreamQuality::High;
        cache.insert(a.clone(), q, sample_stream(Some(10)));
        cache.insert(b.clone(), q, sample_stream(Some(20)));
        cache.insert(c.clone(), q, sample_stream(Some(30)));
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&a, q).is_none());
        assert!(cache.get(&b, q).is_some());
        assert!(cache.get(&c, q).is_some());
    }
    #[test]
    fn reinserting_the_same_id_does_not_evict_itself() {
        let cache = StreamCache::new(Duration::from_secs(300), 1);
        let id = TrackId::new("test", "a");
        let q = StreamQuality::High;
        cache.insert(id.clone(), q, sample_stream(Some(10)));
        cache.insert(id.clone(), q, sample_stream(Some(20)));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&id, q).is_some());
    }
    #[test]
    fn entries_without_url_expiry_live_for_the_full_ttl() {
        let cache = StreamCache::new(Duration::from_millis(50), 4);
        let id = TrackId::new("test", "ttl");
        cache.insert(id.clone(), StreamQuality::High, sample_stream(None));
        assert!(cache.get(&id, StreamQuality::High).is_some());
        std::thread::sleep(Duration::from_millis(120));
        assert!(cache.get(&id, StreamQuality::High).is_none());
        assert!(cache.is_empty());
    }
    #[test]
    fn url_expiry_far_in_the_future_is_capped_by_ttl() {
        let cache = StreamCache::new(Duration::from_millis(60), 4);
        let id = TrackId::new("test", "cap");
        cache.insert(id.clone(), StreamQuality::High, sample_stream(Some(3_600)));
        assert!(cache.get(&id, StreamQuality::High).is_some());
        std::thread::sleep(Duration::from_millis(130));
        assert!(cache.get(&id, StreamQuality::High).is_none());
    }
    #[test]
    fn clear_removes_all_entries() {
        let cache = StreamCache::new(Duration::from_secs(60), 4);
        let id = TrackId::new("test", "x");
        cache.insert(id.clone(), StreamQuality::High, sample_stream(Some(60)));
        cache.clear();
        assert!(cache.is_empty());
        assert!(cache.get(&id, StreamQuality::High).is_none());
    }

    // ---- M6: namespace domain (RESOURCE_DESIGN.md §2.4) ----

    #[test]
    fn namespaced_entries_are_invisible_across_namespaces() {
        let cache = StreamCache::new(Duration::from_secs(60), 8);
        let id = TrackId::new("test", "acct");
        let q = StreamQuality::High;
        cache
            .namespaced("netease\u{1f}alice")
            .insert(id.clone(), q, sample_stream(Some(60)));
        // Same ids, different account namespace: disjoint.
        assert!(cache.namespaced("netease\u{1f}bob").get(&id, q).is_none());
        // The legacy unnamed domain is unaffected either way.
        assert!(cache.get(&id, q).is_none());
        cache
            .namespaced("netease\u{1f}alice")
            .insert(id.clone(), q, sample_stream(Some(60)));
        assert!(cache.namespaced("netease\u{1f}alice").get(&id, q).is_some());
        // Legacy insert lands in the global domain, invisible to namespaces.
        let global = TrackId::new("test", "legacy");
        cache.insert(global.clone(), q, sample_stream(Some(60)));
        assert!(cache.get(&global, q).is_some());
        assert!(
            cache
                .namespaced("netease\u{1f}alice")
                .get(&global, q)
                .is_none()
        );
        // len is the shared bounded map total.
        assert_eq!(cache.len(), 2);
    }

    // ---- M6: per-provider TTL (RESOURCE_DESIGN.md §2.5) ----

    #[test]
    fn provider_ttl_overrides_default_and_caps_by_url_expiry() {
        let cache = StreamCache::new(Duration::from_secs(300), 8);
        let fast = TrackId::new("short-lived", "1");
        let slow = TrackId::new("long-lived", "2");
        cache.set_provider_ttl("short-lived", Some(Duration::from_millis(40)));
        cache.insert(fast.clone(), StreamQuality::High, sample_stream(None));
        cache.insert(slow.clone(), StreamQuality::High, sample_stream(None));
        assert!(cache.get(&fast, StreamQuality::High).is_some());
        assert!(cache.get(&slow, StreamQuality::High).is_some());
        std::thread::sleep(Duration::from_millis(90));
        // The overridden provider expired early; the default one lives on.
        assert!(cache.get(&fast, StreamQuality::High).is_none());
        assert!(cache.get(&slow, StreamQuality::High).is_some());
        // The URL's own absolute expiry still caps the override.
        cache.set_provider_ttl("short-lived", Some(Duration::from_secs(300)));
        let expiring = TrackId::new("short-lived", "3");
        cache.insert(
            expiring.clone(),
            StreamQuality::High,
            sample_stream(Some(0)),
        );
        assert!(cache.get(&expiring, StreamQuality::High).is_none());
        // Removing the override restores the constructor default.
        cache.set_provider_ttl("short-lived", None);
        let restored = TrackId::new("short-lived", "4");
        cache.insert(restored.clone(), StreamQuality::High, sample_stream(None));
        std::thread::sleep(Duration::from_millis(10));
        assert!(cache.get(&restored, StreamQuality::High).is_some());
    }

    // ---- M6: generic TTL cache (RESOURCE_DESIGN.md §2.2) ----

    #[test]
    fn ttl_cache_expires_and_evicts_earliest() {
        let cache = TtlCache::new(Duration::from_millis(60), 2);
        cache.insert("a", 1);
        assert_eq!(cache.get(&"a"), Some(1));
        std::thread::sleep(Duration::from_millis(90));
        assert_eq!(cache.get(&"a"), None);
        assert!(cache.is_empty());
        // Earliest-expiry eviction.
        cache.insert_with_ttl("a", 1, Duration::from_millis(20));
        cache.insert("b", 2);
        std::thread::sleep(Duration::from_millis(40));
        cache.insert("c", 3);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(2));
        // Re-inserting an existing key does not evict itself.
        cache.insert("b", 22);
        assert_eq!(cache.get(&"b"), Some(22));
        // A longer per-entry TTL is capped at the constructor TTL.
        cache.insert_with_ttl("b", 23, Duration::from_secs(3600));
        assert_eq!(cache.get(&"b"), Some(23));
        cache.invalidate(&"b");
        assert_eq!(cache.get(&"b"), None);
    }

    // ---- M6: negative cache (RESOURCE_DESIGN.md §2.3) ----

    fn standard_negative() -> NegativeCache {
        NegativeCache::new(NegativeTtlPolicy::standard(), 8)
    }

    #[test]
    fn negative_cache_records_and_expires_per_category() {
        let cache = standard_negative();
        let key = NegativeKey::Stream {
            id: TrackId::new("netease", "1"),
            quality: StreamQuality::High,
        };
        // Retryable category: short TTL.
        cache.record(key.clone(), "netease", ErrorCategory::RateLimited);
        let hit = cache.check(&key).expect("live negative entry");
        assert_eq!(hit.provider, "netease");
        assert_eq!(hit.category, ErrorCategory::RateLimited);
        cache.invalidate(&key);
        assert!(cache.check(&key).is_none());
        // Expired entries vanish on sight.
        let policy = NegativeTtlPolicy {
            per_category: BTreeMap::from([(ErrorCategory::Network, Duration::from_millis(20))]),
            ..NegativeTtlPolicy::standard()
        };
        let cache = NegativeCache::new(policy, 8);
        cache.record(key.clone(), "netease", ErrorCategory::Network);
        assert!(cache.check(&key).is_some());
        std::thread::sleep(Duration::from_millis(50));
        assert!(cache.check(&key).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn negative_cache_skips_non_cacheable_categories() {
        let cache = standard_negative();
        let key = NegativeKey::Custom("caller-fault".into());
        for category in [
            ErrorCategory::Cancelled,
            ErrorCategory::InvalidInput,
            ErrorCategory::Serialization,
            ErrorCategory::Io,
            ErrorCategory::Unsupported,
            ErrorCategory::UnsupportedOperation,
        ] {
            cache.record(key.clone(), "netease", category);
            assert!(
                cache.check(&key).is_none(),
                "{category:?} must never be negatively cached"
            );
        }
        assert!(cache.is_empty());
    }

    #[test]
    fn negative_cache_clear_provider_drops_only_that_provider() {
        let cache = standard_negative();
        let a = NegativeKey::LyricsDocument {
            provider: "netease".into(),
            id: "1".into(),
        };
        let b = NegativeKey::LyricsDocument {
            provider: "qq".into(),
            id: "2".into(),
        };
        cache.record(a.clone(), "netease", ErrorCategory::SessionExpired);
        cache.record(b.clone(), "qq", ErrorCategory::NotFound);
        // The session-refresh face: credential-shaped failures of one
        // provider are dropped wholesale.
        cache.clear_provider("netease");
        assert!(cache.check(&a).is_none());
        assert!(cache.check(&b).is_some());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn negative_cache_evicts_earliest_at_capacity() {
        let cache = NegativeCache::new(NegativeTtlPolicy::standard(), 1);
        let a = NegativeKey::Custom("a".into());
        let b = NegativeKey::Custom("b".into());
        cache.record(a.clone(), "p", ErrorCategory::NotFound);
        std::thread::sleep(Duration::from_millis(5));
        cache.record(b.clone(), "p", ErrorCategory::NotFound);
        assert_eq!(cache.len(), 1);
        assert!(cache.check(&a).is_none());
        assert!(cache.check(&b).is_some());
    }

    #[test]
    fn negative_keys_are_stable_id_shaped() {
        // Pins the key contract: every variant carries an id/hash/url-class
        // payload, none carries a song title. (A track-search variant was
        // removed in review: the aggregation face takes no cache
        // collaborator yet, RESOURCE_DESIGN.md §12.)
        let keys = [
            NegativeKey::Stream {
                id: TrackId::new("netease", "1"),
                quality: StreamQuality::High,
            },
            NegativeKey::LyricsSearch {
                id: TrackId::new("netease", "1"),
                mode: "full",
                provider: "lrclib".into(),
            },
            NegativeKey::LyricsDocument {
                provider: "netease".into(),
                id: "9".into(),
            },
            NegativeKey::Custom("host-hash".into()),
        ];
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
    }

    /// Review P2: the credential trio shares the short 30 s TTL, and the
    /// transient contract/protocol/internal states share the light 60 s
    /// one; the stable resource facts keep the 10 min default.
    #[test]
    fn standard_policy_ttls_are_asymmetric_per_principle() {
        let policy = NegativeTtlPolicy::standard();
        let secs = |category| policy.ttl_for(category).unwrap().as_secs();
        for category in [
            ErrorCategory::Network,
            ErrorCategory::Timeout,
            ErrorCategory::RateLimited,
            ErrorCategory::Unavailable,
        ] {
            assert_eq!(secs(category), 15, "{category:?} retryable window");
        }
        for category in [
            ErrorCategory::SessionExpired,
            ErrorCategory::AuthenticationRequired,
            ErrorCategory::AuthenticationFailed,
        ] {
            assert_eq!(secs(category), 30, "{category:?} credential window");
        }
        for category in [
            ErrorCategory::Provider,
            ErrorCategory::InvalidResponse,
            ErrorCategory::Protocol,
            ErrorCategory::ProviderInternal,
        ] {
            assert_eq!(secs(category), 60, "{category:?} light window");
        }
        assert_eq!(
            secs(ErrorCategory::NotFound),
            600,
            "stable resource facts keep the default"
        );
        assert_eq!(policy.ttl_for(ErrorCategory::InvalidInput), None);
    }

    /// m6-lyrics-helper-audit: `remaining_ttl` expiry semantics (the D28
    /// alignment face, cache.rs:431-443 doc). A live entry reports a
    /// positive remaining window that shrinks with the clock; an absent key
    /// reports `None`; an expired entry reports `None` while staying in the
    /// map, because `remaining_ttl` is deliberately read-only and expiry
    /// purging stays with `check`/`record` (cache.rs:431-433). A caller
    /// aligning a provisional positive entry to this window (the lyrics
    /// cache, lyrics_cache.rs `search_negative_remaining`) must therefore
    /// never treat `None` as "no state" vs "expired state" - both mean the
    /// window is over.
    #[test]
    fn remaining_ttl_reports_live_windows_and_leaves_purging_to_check() {
        let policy = NegativeTtlPolicy {
            per_category: BTreeMap::from([(ErrorCategory::Network, Duration::from_millis(80))]),
            ..NegativeTtlPolicy::standard()
        };
        let cache = NegativeCache::new(policy, 8);
        let key = NegativeKey::LyricsSearch {
            id: TrackId::new("netease", "1"),
            mode: "full",
            provider: "lrclib".into(),
        };
        // Absent key: no window.
        assert_eq!(cache.remaining_ttl(&key), None);
        cache.record(key.clone(), "lrclib", ErrorCategory::Network);
        let remaining = cache.remaining_ttl(&key).expect("live window");
        assert!(remaining <= Duration::from_millis(80) && remaining > Duration::ZERO);
        std::thread::sleep(Duration::from_millis(25));
        let shrunk = cache
            .remaining_ttl(&key)
            .expect("window shrinks, not grows");
        assert!(shrunk < remaining, "{shrunk:?} must be below {remaining:?}");
        // Expired: `None`, but the entry is not purged by the read.
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(cache.remaining_ttl(&key), None);
        assert_eq!(cache.len(), 1, "remaining_ttl never purges");
        // The purge belongs to `check` alone.
        assert!(cache.check(&key).is_none());
        assert_eq!(cache.len(), 0);
    }
}
