//! Provider registry: typed capability slots, conflict-safe registration,
//! unregister, capability queries, descriptor metadata and the health-query
//! surface (M2 core, AUTH_REGISTRY_DESIGN.md §5; tasks.md §5.4).
use crate::artwork::ArtworkProvider;
use crate::health::{ProviderHealthConfig, ProviderHealthMonitor, ProviderHealthSnapshot};
use crate::provider::*;
use std::{collections::BTreeMap, fmt, sync::Arc};

/// Registration conflict (tasks.md §5.4; a host programming error, not a
/// platform failure, hence its own type instead of a `crate::Error` variant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// The provider id already occupies this capability slot.
    DuplicateRegistration { id: String, slot: &'static str },
}
impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::DuplicateRegistration { id, slot } => {
                write!(
                    f,
                    "provider \"{id}\" is already registered in slot \"{slot}\""
                )
            }
        }
    }
}
impl std::error::Error for RegistryError {}

/// Result of a conflict-checked registration.
pub type RegistryResult = Result<(), RegistryError>;

/// Descriptive metadata for one registered provider id. The capability
/// aggregate is the OR of every registration seen for the id (history, not
/// live state); `name`/`version`/`priority`/`supported_qualities` describe
/// the most recently registered instance. `auth` is sticky ONLY for the
/// `AuthCapability::Qr` upgrade (once anything declared Qr for this id, no
/// later registration can pull it back down; only `unregister_all` clears
/// it); every other value is last-writer-wins, matching the legacy overwrite
/// registration semantics (so re-using one id with different instances can
/// order-dependently end on Cookie or None).
///
/// Live-state views are [`ProviderRegistry::provider_ids`] and
/// [`ProviderRegistry::providers_with_capability`], which only reflect
/// instances still present in their slots.
#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    pub id: String,
    pub name: String,
    pub capabilities: ProviderCapabilities,
    /// Declared implementation version (`MusicProvider::version()`); None =
    /// not declared.
    pub version: Option<String>,
    /// Authentication capability (tasks.md §5.4 "查询是否支持认证",
    /// AUTH_REGISTRY_DESIGN.md §5.4). Upgraded to `AuthCapability::Qr` when
    /// the provider's QR slot is registered; ONLY the Qr upgrade is sticky,
    /// every other value is last-writer-wins.
    pub auth: AuthCapability,
    /// Host-side ordering hint (tasks.md §5.4 "查询 Provider 优先级";
    /// higher wins). Default 0.
    pub priority: i32,
    /// Quality tiers the provider declared (empty = not declared).
    pub supported_qualities: Vec<crate::models::StreamQuality>,
}

/// Slot names used in `RegistryError::DuplicateRegistration` and tests.
mod slot {
    pub const SEARCH: &str = "search";
    pub const STREAM: &str = "stream";
    pub const LYRICS: &str = "lyrics";
    pub const COLLECTION: &str = "collection";
    pub const PLAYLIST: &str = "playlist";
    pub const ALBUM: &str = "album";
    pub const ARTIST: &str = "artist";
    pub const CHART: &str = "chart";
    pub const LINK: &str = "link";
    pub const AUTH: &str = "auth";
    pub const QR_AUTH: &str = "qr_auth";
    /// The twelfth slot (M6, RESOURCE_DESIGN.md §1.3): artwork resolution.
    /// Empty until a provider implements [`ArtworkProvider`] (no-empty-
    /// shells rule); the conflict and unregister semantics are identical to
    /// the other slots.
    pub const ARTWORK: &str = "artwork";
}

pub struct ProviderRegistry {
    descriptors: BTreeMap<String, ProviderDescriptor>,
    search: BTreeMap<String, Arc<dyn TrackSearch>>,
    streams: BTreeMap<String, Arc<dyn StreamResolver>>,
    lyrics: BTreeMap<String, Arc<dyn LyricsProvider>>,
    collections: BTreeMap<String, Arc<dyn CollectionProvider>>,
    playlists: BTreeMap<String, Arc<dyn PlaylistProvider>>,
    albums: BTreeMap<String, Arc<dyn AlbumProvider>>,
    artists: BTreeMap<String, Arc<dyn ArtistProvider>>,
    charts: BTreeMap<String, Arc<dyn ChartProvider>>,
    links: BTreeMap<String, Arc<dyn LinkResolver>>,
    auth: BTreeMap<String, Arc<dyn AuthenticationProvider>>,
    qr_auth: BTreeMap<String, Arc<dyn QrAuthenticationProvider>>,
    /// The artwork slot (M6, RESOURCE_DESIGN.md §1.3). No registered
    /// provider fills it yet; hosts and provider slices register
    /// `ArtworkProvider` implementations here.
    artworks: BTreeMap<String, Arc<dyn ArtworkProvider>>,
    /// Registry-view health monitor. The resolve pipeline keeps its own
    /// instance (M1 boundary, resolve.rs untouched); the two do not share
    /// state (AUTH_REGISTRY_DESIGN.md §5.5).
    health: ProviderHealthMonitor,
}
impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::with_health_config(ProviderHealthConfig::default())
    }
}
impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    /// Registry with a tuned health config for the query-side monitor.
    pub fn with_health_config(config: ProviderHealthConfig) -> Self {
        Self {
            descriptors: BTreeMap::new(),
            search: BTreeMap::new(),
            streams: BTreeMap::new(),
            lyrics: BTreeMap::new(),
            collections: BTreeMap::new(),
            playlists: BTreeMap::new(),
            albums: BTreeMap::new(),
            artists: BTreeMap::new(),
            charts: BTreeMap::new(),
            links: BTreeMap::new(),
            auth: BTreeMap::new(),
            qr_auth: BTreeMap::new(),
            artworks: BTreeMap::new(),
            health: ProviderHealthMonitor::new(config),
        }
    }

    /// Merge one registration into the descriptor history. `download` is
    /// deliberately NOT merged (AUTH_REGISTRY_DESIGN.md §5.6): it is an
    /// engine-side capability (TRANSPORT_DESIGN.md §6), no provider sets it
    /// today, and a stray `download: true` must not pollute the aggregate.
    fn record(&mut self, provider: &dyn MusicProvider) {
        let id = provider.id().to_owned();
        let incoming = provider.capabilities();
        let entry = self
            .descriptors
            .entry(id.clone())
            .or_insert_with(|| ProviderDescriptor {
                id,
                name: provider.name().to_owned(),
                capabilities: ProviderCapabilities::default(),
                version: None,
                auth: AuthCapability::None,
                priority: 0,
                supported_qualities: Vec::new(),
            });
        entry.name = provider.name().to_owned();
        entry.version = provider.version().map(str::to_owned);
        entry.priority = provider.priority();
        entry.supported_qualities = provider.supported_qualities();
        // `auth` is sticky ONLY for the Qr upgrade: once anything declared
        // Qr for this id (explicitly or via the qr_auth slot), a later slot
        // registration must not silently pull it back down. Every other
        // value is last-writer-wins, matching the legacy overwrite
        // registration semantics (see the type docs).
        if entry.auth != AuthCapability::Qr {
            entry.auth = provider.auth_capability();
        }
        let current = &mut entry.capabilities;
        current.track_search |= incoming.track_search;
        current.stream_resolve |= incoming.stream_resolve;
        current.lyrics |= incoming.lyrics;
        current.playlists |= incoming.playlists;
        current.albums |= incoming.albums;
        current.artists |= incoming.artists;
        current.charts |= incoming.charts;
        current.link_resolve |= incoming.link_resolve;
        current.authentication |= incoming.authentication;
        current.artwork |= incoming.artwork;
    }

    // ---- conflict-checked registration (M2) ----

    pub fn try_register_search(&mut self, p: Arc<dyn TrackSearch>) -> RegistryResult {
        if self.search.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::SEARCH,
            });
        }
        self.record(p.as_ref());
        self.search.insert(p.id().to_owned(), p);
        Ok(())
    }
    pub fn try_register_stream_resolver(&mut self, p: Arc<dyn StreamResolver>) -> RegistryResult {
        if self.streams.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::STREAM,
            });
        }
        self.record(p.as_ref());
        self.streams.insert(p.id().to_owned(), p);
        Ok(())
    }
    pub fn try_register_lyrics(&mut self, p: Arc<dyn LyricsProvider>) -> RegistryResult {
        if self.lyrics.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::LYRICS,
            });
        }
        self.record(p.as_ref());
        self.lyrics.insert(p.id().to_owned(), p);
        Ok(())
    }
    pub fn try_register_collection(&mut self, p: Arc<dyn CollectionProvider>) -> RegistryResult {
        if self.collections.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::COLLECTION,
            });
        }
        self.record(p.as_ref());
        self.collections.insert(p.id().to_owned(), p);
        Ok(())
    }
    pub fn try_register_playlist(&mut self, p: Arc<dyn PlaylistProvider>) -> RegistryResult {
        if self.playlists.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::PLAYLIST,
            });
        }
        self.record(p.as_ref());
        self.playlists.insert(p.id().to_owned(), p);
        Ok(())
    }
    pub fn try_register_album(&mut self, p: Arc<dyn AlbumProvider>) -> RegistryResult {
        if self.albums.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::ALBUM,
            });
        }
        self.record(p.as_ref());
        self.albums.insert(p.id().to_owned(), p);
        Ok(())
    }
    pub fn try_register_artist(&mut self, p: Arc<dyn ArtistProvider>) -> RegistryResult {
        if self.artists.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::ARTIST,
            });
        }
        self.record(p.as_ref());
        self.artists.insert(p.id().to_owned(), p);
        Ok(())
    }
    pub fn try_register_chart(&mut self, p: Arc<dyn ChartProvider>) -> RegistryResult {
        if self.charts.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::CHART,
            });
        }
        self.record(p.as_ref());
        self.charts.insert(p.id().to_owned(), p);
        Ok(())
    }
    pub fn try_register_link_resolver(&mut self, p: Arc<dyn LinkResolver>) -> RegistryResult {
        if self.links.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::LINK,
            });
        }
        self.record(p.as_ref());
        self.links.insert(p.id().to_owned(), p);
        Ok(())
    }
    pub fn try_register_authentication(
        &mut self,
        p: Arc<dyn AuthenticationProvider>,
    ) -> RegistryResult {
        if self.auth.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::AUTH,
            });
        }
        self.record(p.as_ref());
        self.auth.insert(p.id().to_owned(), p);
        Ok(())
    }
    pub fn try_register_qr_authentication(
        &mut self,
        p: Arc<dyn QrAuthenticationProvider>,
    ) -> RegistryResult {
        if self.qr_auth.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::QR_AUTH,
            });
        }
        self.record(p.as_ref());
        // Registering the QR slot IS the provider declaring QR capability:
        // the descriptor no longer needs the slot's existence inferred
        // (AUTH_REGISTRY_DESIGN.md §5.4).
        if let Some(descriptor) = self.descriptors.get_mut(p.id()) {
            descriptor.auth = AuthCapability::Qr;
        }
        self.qr_auth.insert(p.id().to_owned(), p);
        Ok(())
    }
    /// The twelfth slot (M6, RESOURCE_DESIGN.md §1.3): artwork resolution.
    /// Same conflict-checked semantics as the other slots.
    pub fn try_register_artwork(&mut self, p: Arc<dyn ArtworkProvider>) -> RegistryResult {
        if self.artworks.contains_key(p.id()) {
            return Err(RegistryError::DuplicateRegistration {
                id: p.id().to_owned(),
                slot: slot::ARTWORK,
            });
        }
        self.record(p.as_ref());
        self.artworks.insert(p.id().to_owned(), p);
        Ok(())
    }

    // ---- legacy overwrite registration (behavior unchanged, now pinned by
    // tests: OR-merged capabilities, silent slot replacement) ----

    pub fn register_search(&mut self, p: Arc<dyn TrackSearch>) {
        self.record(p.as_ref());
        self.search.insert(p.id().to_owned(), p);
    }
    pub fn register_stream_resolver(&mut self, p: Arc<dyn StreamResolver>) {
        self.record(p.as_ref());
        self.streams.insert(p.id().to_owned(), p);
    }
    pub fn register_lyrics(&mut self, p: Arc<dyn LyricsProvider>) {
        self.record(p.as_ref());
        self.lyrics.insert(p.id().to_owned(), p);
    }
    pub fn register_collection(&mut self, p: Arc<dyn CollectionProvider>) {
        self.record(p.as_ref());
        self.collections.insert(p.id().to_owned(), p);
    }
    pub fn register_playlist(&mut self, p: Arc<dyn PlaylistProvider>) {
        self.record(p.as_ref());
        self.playlists.insert(p.id().to_owned(), p);
    }
    pub fn register_album(&mut self, p: Arc<dyn AlbumProvider>) {
        self.record(p.as_ref());
        self.albums.insert(p.id().to_owned(), p);
    }
    pub fn register_artist(&mut self, p: Arc<dyn ArtistProvider>) {
        self.record(p.as_ref());
        self.artists.insert(p.id().to_owned(), p);
    }
    pub fn register_chart(&mut self, p: Arc<dyn ChartProvider>) {
        self.record(p.as_ref());
        self.charts.insert(p.id().to_owned(), p);
    }
    pub fn register_link_resolver(&mut self, p: Arc<dyn LinkResolver>) {
        self.record(p.as_ref());
        self.links.insert(p.id().to_owned(), p);
    }
    pub fn register_authentication(&mut self, p: Arc<dyn AuthenticationProvider>) {
        self.record(p.as_ref());
        self.auth.insert(p.id().to_owned(), p);
    }
    pub fn register_qr_authentication(&mut self, p: Arc<dyn QrAuthenticationProvider>) {
        self.record(p.as_ref());
        if let Some(descriptor) = self.descriptors.get_mut(p.id()) {
            descriptor.auth = AuthCapability::Qr;
        }
        self.qr_auth.insert(p.id().to_owned(), p);
    }
    pub fn register_artwork(&mut self, p: Arc<dyn ArtworkProvider>) {
        self.record(p.as_ref());
        self.artworks.insert(p.id().to_owned(), p);
    }

    // ---- unregister (M2) ----

    pub fn unregister_search(&mut self, id: &str) -> Option<Arc<dyn TrackSearch>> {
        self.search.remove(id)
    }
    pub fn unregister_stream_resolver(&mut self, id: &str) -> Option<Arc<dyn StreamResolver>> {
        self.streams.remove(id)
    }
    pub fn unregister_lyrics(&mut self, id: &str) -> Option<Arc<dyn LyricsProvider>> {
        self.lyrics.remove(id)
    }
    pub fn unregister_collection(&mut self, id: &str) -> Option<Arc<dyn CollectionProvider>> {
        self.collections.remove(id)
    }
    pub fn unregister_playlist(&mut self, id: &str) -> Option<Arc<dyn PlaylistProvider>> {
        self.playlists.remove(id)
    }
    pub fn unregister_album(&mut self, id: &str) -> Option<Arc<dyn AlbumProvider>> {
        self.albums.remove(id)
    }
    pub fn unregister_artist(&mut self, id: &str) -> Option<Arc<dyn ArtistProvider>> {
        self.artists.remove(id)
    }
    pub fn unregister_chart(&mut self, id: &str) -> Option<Arc<dyn ChartProvider>> {
        self.charts.remove(id)
    }
    pub fn unregister_link_resolver(&mut self, id: &str) -> Option<Arc<dyn LinkResolver>> {
        self.links.remove(id)
    }
    pub fn unregister_authentication(
        &mut self,
        id: &str,
    ) -> Option<Arc<dyn AuthenticationProvider>> {
        self.auth.remove(id)
    }
    pub fn unregister_qr_authentication(
        &mut self,
        id: &str,
    ) -> Option<Arc<dyn QrAuthenticationProvider>> {
        self.qr_auth.remove(id)
    }
    pub fn unregister_artwork(&mut self, id: &str) -> Option<Arc<dyn ArtworkProvider>> {
        self.artworks.remove(id)
    }
    /// Remove every slot instance and the descriptor of `id`. Returns whether
    /// anything was removed.
    ///
    /// The registry-view health history for `id` is reset as well: the id
    /// has left the registration surface, so a later re-registration starts
    /// from a fresh Closed circuit instead of inheriting the previous
    /// Open/cooldown state (AUTH_REGISTRY_DESIGN.md §5.5). Single-slot
    /// unregisters do not reset (the id is still live in other slots).
    pub fn unregister_all(&mut self, id: &str) -> bool {
        let removed = self.search.remove(id).is_some()
            | self.streams.remove(id).is_some()
            | self.lyrics.remove(id).is_some()
            | self.collections.remove(id).is_some()
            | self.playlists.remove(id).is_some()
            | self.albums.remove(id).is_some()
            | self.artists.remove(id).is_some()
            | self.charts.remove(id).is_some()
            | self.links.remove(id).is_some()
            | self.auth.remove(id).is_some()
            | self.qr_auth.remove(id).is_some()
            | self.artworks.remove(id).is_some()
            | self.descriptors.remove(id).is_some();
        self.health.reset(id);
        removed
    }

    // ---- queries ----

    pub fn descriptor(&self, id: &str) -> Option<&ProviderDescriptor> {
        self.descriptors.get(id)
    }
    /// Ids that still hold at least one slot instance, sorted. This is the
    /// LIVE registration view: an id whose every slot was unregistered
    /// (individually) drops out even though its historical descriptor
    /// remains (see `descriptor`).
    pub fn provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for id in self
            .search
            .keys()
            .chain(self.streams.keys())
            .chain(self.lyrics.keys())
            .chain(self.collections.keys())
            .chain(self.playlists.keys())
            .chain(self.albums.keys())
            .chain(self.artists.keys())
            .chain(self.charts.keys())
            .chain(self.links.keys())
            .chain(self.auth.keys())
            .chain(self.qr_auth.keys())
            .chain(self.artworks.keys())
        {
            if !ids.iter().any(|seen| seen == id) {
                ids.push(id.clone());
            }
        }
        ids.sort();
        ids
    }
    /// Ids whose LIVE capabilities satisfy `pred`, sorted (tasks.md §5.4
    /// "按能力查询"). Live capabilities are the union of the
    /// `capabilities()` of the instances currently present in the slots, so
    /// a single-slot unregister immediately stops matching. The historical
    /// aggregate lives on `ProviderDescriptor::capabilities`.
    pub fn providers_with_capability(
        &self,
        pred: impl Fn(&ProviderCapabilities) -> bool,
    ) -> Vec<String> {
        let mut matched: Vec<String> = Vec::new();
        for id in self.provider_ids() {
            if pred(&self.live_capabilities(&id)) {
                matched.push(id);
            }
        }
        matched
    }
    /// Union of the `capabilities()` declared by the instances currently in
    /// the slots for `id`.
    fn live_capabilities(&self, id: &str) -> ProviderCapabilities {
        fn merge(caps: &mut ProviderCapabilities, provider: &dyn MusicProvider) {
            let incoming = provider.capabilities();
            caps.track_search |= incoming.track_search;
            caps.stream_resolve |= incoming.stream_resolve;
            caps.lyrics |= incoming.lyrics;
            caps.playlists |= incoming.playlists;
            caps.albums |= incoming.albums;
            caps.artists |= incoming.artists;
            caps.charts |= incoming.charts;
            caps.link_resolve |= incoming.link_resolve;
            caps.authentication |= incoming.authentication;
            caps.artwork |= incoming.artwork;
        }
        let mut caps = ProviderCapabilities::default();
        if let Some(p) = self.search.get(id) {
            merge(&mut caps, p.as_ref());
        }
        if let Some(p) = self.streams.get(id) {
            merge(&mut caps, p.as_ref());
        }
        if let Some(p) = self.lyrics.get(id) {
            merge(&mut caps, p.as_ref());
        }
        if let Some(p) = self.collections.get(id) {
            merge(&mut caps, p.as_ref());
        }
        if let Some(p) = self.playlists.get(id) {
            merge(&mut caps, p.as_ref());
        }
        if let Some(p) = self.albums.get(id) {
            merge(&mut caps, p.as_ref());
        }
        if let Some(p) = self.artists.get(id) {
            merge(&mut caps, p.as_ref());
        }
        if let Some(p) = self.charts.get(id) {
            merge(&mut caps, p.as_ref());
        }
        if let Some(p) = self.links.get(id) {
            merge(&mut caps, p.as_ref());
        }
        if let Some(p) = self.auth.get(id) {
            merge(&mut caps, p.as_ref());
        }
        if let Some(p) = self.qr_auth.get(id) {
            merge(&mut caps, p.as_ref());
        }
        if let Some(p) = self.artworks.get(id) {
            merge(&mut caps, p.as_ref());
        }
        caps
    }
    pub fn search_providers(&self) -> impl Iterator<Item = &Arc<dyn TrackSearch>> {
        self.search.values()
    }
    pub fn search_provider(&self, id: &str) -> Option<&Arc<dyn TrackSearch>> {
        self.search.get(id)
    }
    pub fn stream_provider(&self, id: &str) -> Option<&Arc<dyn StreamResolver>> {
        self.streams.get(id)
    }
    pub fn lyrics_provider(&self, id: &str) -> Option<&Arc<dyn LyricsProvider>> {
        self.lyrics.get(id)
    }
    pub fn collection_provider(&self, id: &str) -> Option<&Arc<dyn CollectionProvider>> {
        self.collections.get(id)
    }
    pub fn playlist_provider(&self, id: &str) -> Option<&Arc<dyn PlaylistProvider>> {
        self.playlists.get(id)
    }
    pub fn album_provider(&self, id: &str) -> Option<&Arc<dyn AlbumProvider>> {
        self.albums.get(id)
    }
    pub fn artist_provider(&self, id: &str) -> Option<&Arc<dyn ArtistProvider>> {
        self.artists.get(id)
    }
    pub fn chart_provider(&self, id: &str) -> Option<&Arc<dyn ChartProvider>> {
        self.charts.get(id)
    }
    pub fn link_provider(&self, id: &str) -> Option<&Arc<dyn LinkResolver>> {
        self.links.get(id)
    }
    pub fn authentication_provider(&self, id: &str) -> Option<&Arc<dyn AuthenticationProvider>> {
        self.auth.get(id)
    }
    pub fn qr_authentication_provider(
        &self,
        id: &str,
    ) -> Option<&Arc<dyn QrAuthenticationProvider>> {
        self.qr_auth.get(id)
    }
    pub fn artwork_provider(&self, id: &str) -> Option<&Arc<dyn ArtworkProvider>> {
        self.artworks.get(id)
    }

    // ---- health query surface (M2) ----

    /// The registry-view monitor; hosts record successes/failures against it
    /// for query-side health (the resolve pipeline keeps its own monitor).
    pub fn health_monitor(&self) -> &ProviderHealthMonitor {
        &self.health
    }
    /// Circuit snapshot for one provider id (health.rs snapshot semantics).
    pub fn health_snapshot(&self, id: &str) -> ProviderHealthSnapshot {
        self.health.snapshot(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::models::*;
    use async_trait::async_trait;
    use std::time::Duration;

    /// One stub implementing every slot trait, so conflict/unregister tests
    /// can cover all eleven slots cheaply. All methods fail with
    /// `Unsupported` (never called in these tests).
    struct Stub {
        id: String,
        name: &'static str,
        caps: ProviderCapabilities,
        version: Option<&'static str>,
        auth: AuthCapability,
        priority: i32,
        qualities: Vec<StreamQuality>,
    }
    impl Stub {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_owned(),
                name: "Stub",
                caps: ProviderCapabilities::default(),
                version: None,
                auth: AuthCapability::None,
                priority: 0,
                qualities: Vec::new(),
            }
        }
    }
    impl MusicProvider for Stub {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            self.name
        }
        fn capabilities(&self) -> ProviderCapabilities {
            self.caps
        }
        fn version(&self) -> Option<&str> {
            self.version
        }
        fn priority(&self) -> i32 {
            self.priority
        }
        fn auth_capability(&self) -> AuthCapability {
            self.auth
        }
        fn supported_qualities(&self) -> Vec<StreamQuality> {
            self.qualities.clone()
        }
    }
    impl Stub {
        fn deny(&self) -> crate::Error {
            crate::Error::Cancelled
        }
    }
    #[async_trait]
    impl TrackSearch for Stub {
        async fn search(&self, _query: &SearchQuery) -> Result<SearchPage, Error> {
            Err(self.deny())
        }
    }
    #[async_trait]
    impl StreamResolver for Stub {
        async fn resolve(
            &self,
            _track: &Track,
            _options: &ResolveOptions,
        ) -> Result<StreamInfo, Error> {
            Err(self.deny())
        }
    }
    #[async_trait]
    impl LyricsProvider for Stub {
        async fn lyrics(&self, _track: &Track) -> Result<Option<LyricDocument>, Error> {
            Err(self.deny())
        }
    }
    #[async_trait]
    impl CollectionProvider for Stub {
        async fn playlist(&self, _id: &str) -> Result<Playlist, Error> {
            Err(self.deny())
        }
    }
    #[async_trait]
    impl PlaylistProvider for Stub {
        async fn categories(&self) -> Result<Vec<PlaylistCategory>, Error> {
            Err(self.deny())
        }
        async fn playlists(
            &self,
            _category: &str,
            _page: u32,
            _page_size: u32,
        ) -> Result<Vec<Playlist>, Error> {
            Err(self.deny())
        }
        async fn search_playlists(
            &self,
            _keyword: &str,
            _page: u32,
        ) -> Result<Vec<Playlist>, Error> {
            Err(self.deny())
        }
        async fn user_playlists(
            &self,
            _page: u32,
            _page_size: u32,
        ) -> Result<Vec<Playlist>, Error> {
            Err(self.deny())
        }
    }
    #[async_trait]
    impl AlbumProvider for Stub {
        async fn album(&self, _id: &str) -> Result<Album, Error> {
            Err(self.deny())
        }
    }
    #[async_trait]
    impl ArtistProvider for Stub {
        async fn artist(&self, _id: &str) -> Result<Artist, Error> {
            Err(self.deny())
        }
    }
    #[async_trait]
    impl ChartProvider for Stub {
        async fn charts(&self) -> Result<Vec<Chart>, Error> {
            Err(self.deny())
        }
        async fn chart_tracks(&self, _id: &str) -> Result<Vec<ChartEntry>, Error> {
            Err(self.deny())
        }
    }
    #[async_trait]
    impl LinkResolver for Stub {
        async fn resolve_link(&self, _url: &str) -> Result<ResolvedLink, Error> {
            Err(self.deny())
        }
    }
    #[async_trait]
    impl AuthenticationProvider for Stub {
        async fn login_status(&self) -> Result<LoginStatus, Error> {
            Err(self.deny())
        }
        async fn logout(&self) -> Result<(), Error> {
            Err(self.deny())
        }
    }
    #[async_trait]
    impl QrAuthenticationProvider for Stub {
        async fn create_qr_login(&self) -> Result<QrLoginSession, Error> {
            Err(self.deny())
        }
        async fn poll_qr_login(&self, _key: &str) -> Result<QrLoginPoll, Error> {
            Err(self.deny())
        }
    }
    #[async_trait]
    impl crate::artwork::ArtworkProvider for Stub {
        async fn artwork(
            &self,
            _key: &crate::artwork::ArtworkKey,
            _size: crate::artwork::ArtworkSize,
        ) -> Result<Option<crate::artwork::Artwork>, Error> {
            Err(self.deny())
        }
    }

    fn arc(id: &str) -> Arc<Stub> {
        Arc::new(Stub::new(id))
    }

    #[test]
    fn try_register_conflict_keeps_the_first_instance() {
        let mut registry = ProviderRegistry::new();
        let mut first = Stub::new("dual");
        first.version = Some("1.0");
        let mut second = Stub::new("dual");
        second.version = Some("2.0");
        registry
            .try_register_search(Arc::new(first))
            .expect("first registration must succeed");
        let error = registry
            .try_register_search(Arc::new(second))
            .expect_err("duplicate id must be rejected");
        assert_eq!(
            error,
            RegistryError::DuplicateRegistration {
                id: "dual".into(),
                slot: "search",
            }
        );
        assert_eq!(
            error.to_string(),
            "provider \"dual\" is already registered in slot \"search\""
        );
        // The first instance is untouched: descriptor still reports v1.0.
        assert_eq!(
            registry.descriptor("dual").unwrap().version,
            Some("1.0".into())
        );
        // Same id, different slot: allowed (conflict is per slot).
        registry
            .try_register_stream_resolver(arc("dual"))
            .expect("a second slot for the same id must not conflict");
        assert!(registry.stream_provider("dual").is_some());
    }

    #[test]
    fn try_register_covers_all_twelve_slots_with_matching_slot_names() {
        let mut registry = ProviderRegistry::new();
        macro_rules! register_all {
            ($reg:expr, $id:expr) => {{
                let id = $id;
                $reg.try_register_search(arc(id)).unwrap();
                $reg.try_register_stream_resolver(arc(id)).unwrap();
                $reg.try_register_lyrics(arc(id)).unwrap();
                $reg.try_register_collection(arc(id)).unwrap();
                $reg.try_register_playlist(arc(id)).unwrap();
                $reg.try_register_album(arc(id)).unwrap();
                $reg.try_register_artist(arc(id)).unwrap();
                $reg.try_register_chart(arc(id)).unwrap();
                $reg.try_register_link_resolver(arc(id)).unwrap();
                $reg.try_register_authentication(arc(id)).unwrap();
                $reg.try_register_qr_authentication(arc(id)).unwrap();
                $reg.try_register_artwork(arc(id)).unwrap();
            }};
        }
        register_all!(registry, "all-slots");
        assert_eq!(registry.provider_ids(), vec!["all-slots".to_owned()]);
        macro_rules! assert_second_fails {
            ($reg:expr, $id:expr, [$($call:ident => $slot:literal),* $(,)?]) => {
                $(
                    assert_eq!(
                        $reg.$call(arc($id)).unwrap_err(),
                        RegistryError::DuplicateRegistration {
                            id: $id.to_owned(),
                            slot: $slot,
                        },
                        "slot name mismatch"
                    );
                )*
            };
        }
        assert_second_fails!(
            registry,
            "all-slots",
            [
                try_register_search => "search",
                try_register_stream_resolver => "stream",
                try_register_lyrics => "lyrics",
                try_register_collection => "collection",
                try_register_playlist => "playlist",
                try_register_album => "album",
                try_register_artist => "artist",
                try_register_chart => "chart",
                try_register_link_resolver => "link",
                try_register_authentication => "auth",
                try_register_qr_authentication => "qr_auth",
                try_register_artwork => "artwork",
            ]
        );
    }

    /// M6 (RESOURCE_DESIGN.md §1.3): the artwork slot behaves like the
    /// others (conflict, unregister, live capability merge), and stays
    /// empty when nothing registers into it.
    #[test]
    fn artwork_slot_conflicts_unregisters_and_merges_capabilities() {
        let mut registry = ProviderRegistry::new();
        assert!(
            registry.artwork_provider("artful").is_none(),
            "the slot is empty until a real implementation registers"
        );
        let mut provider = Stub::new("artful");
        provider.caps.artwork = true;
        registry.register_artwork(Arc::new(provider));
        assert!(registry.artwork_provider("artful").is_some());
        assert!(
            registry
                .providers_with_capability(|c| c.artwork)
                .contains(&"artful".to_owned())
        );
        // The descriptor aggregate merges the bit.
        assert!(registry.descriptor("artful").unwrap().capabilities.artwork);
        // unregister_all removes the slot and keeps the health-reset face.
        assert!(registry.unregister_all("artful"));
        assert!(registry.artwork_provider("artful").is_none());
        assert!(
            !registry
                .providers_with_capability(|c| c.artwork)
                .contains(&"artful".to_owned())
        );
    }

    #[test]
    fn legacy_register_overwrites_slot_and_or_merges_capabilities() {
        // Pins the pre-M2 public behavior that was previously untested
        // (AUTH_REGISTRY_DESIGN.md §5.1): aggregate = OR of registrations,
        // slot instance = last writer wins, name refreshed.
        let mut registry = ProviderRegistry::new();
        let mut first = Stub::new("overwritten");
        first.caps.track_search = true;
        let mut second = Stub::new("overwritten");
        second.caps.lyrics = true;
        second.name = "Stub v2";
        registry.register_search(Arc::new(first));
        registry.register_search(Arc::new(second));
        let descriptor = registry.descriptor("overwritten").unwrap();
        assert!(
            descriptor.capabilities.track_search,
            "first registration must stay merged"
        );
        assert!(
            descriptor.capabilities.lyrics,
            "second registration must merge"
        );
        assert_eq!(descriptor.name, "Stub v2");
    }

    #[test]
    fn unregister_removes_single_slot_then_all() {
        let mut registry = ProviderRegistry::new();
        registry.register_search(arc("gone"));
        registry.register_lyrics(arc("gone"));
        registry.register_authentication(arc("gone"));
        assert!(registry.unregister_search("gone").is_some());
        assert!(registry.search_provider("gone").is_none());
        // Descriptor is registration history: single-slot unregister keeps it.
        assert!(registry.descriptor("gone").is_some());
        assert!(registry.lyrics_provider("gone").is_some());
        assert!(registry.unregister_all("gone"));
        assert!(registry.descriptor("gone").is_none());
        assert!(registry.provider_ids().is_empty());
        assert!(registry.lyrics_provider("gone").is_none());
        assert!(registry.authentication_provider("gone").is_none());
        assert!(!registry.unregister_all("gone"));
    }

    #[test]
    fn providers_with_capability_filters_by_live_registration() {
        let mut registry = ProviderRegistry::new();
        let mut searcher = Stub::new("beta");
        searcher.caps.track_search = true;
        let mut streamer = Stub::new("alpha");
        streamer.caps.stream_resolve = true;
        registry.register_search(Arc::new(searcher));
        registry.register_stream_resolver(Arc::new(streamer));
        let mut both = Stub::new("gamma");
        both.caps.track_search = true;
        both.caps.stream_resolve = true;
        registry.register_search(Arc::new(both));
        assert_eq!(
            registry.providers_with_capability(|c| c.track_search),
            vec!["beta".to_owned(), "gamma".to_owned()]
        );
        assert_eq!(
            registry.providers_with_capability(|c| c.stream_resolve),
            vec!["alpha".to_owned(), "gamma".to_owned()]
        );
        assert!(registry.providers_with_capability(|c| c.charts).is_empty());
    }

    #[test]
    fn capability_queries_reflect_live_registration_not_history() {
        // AUTH_REGISTRY_DESIGN.md §5.2/§5.3: single-slot unregister keeps the
        // historical descriptor, but the query surface must stop matching
        // capabilities the provider no longer has live.
        let mut registry = ProviderRegistry::new();
        let mut searcher = Stub::new("solo");
        searcher.caps.track_search = true;
        registry.register_search(Arc::new(searcher));
        assert!(
            registry
                .providers_with_capability(|c| c.track_search)
                .contains(&"solo".to_owned())
        );
        registry.unregister_search("solo");
        assert!(
            registry
                .providers_with_capability(|c| c.track_search)
                .is_empty(),
            "unregistered slot must not satisfy the capability query"
        );
        assert!(
            registry.provider_ids().is_empty(),
            "an id with zero live slots leaves provider_ids"
        );
        assert!(
            registry.descriptor("solo").is_some(),
            "historical descriptor stays queryable"
        );
    }

    #[test]
    fn descriptor_metadata_plumbs_version_priority_auth_and_qualities() {
        let mut registry = ProviderRegistry::new();
        let mut provider = Stub::new("meta");
        provider.version = Some("0.2.0");
        provider.priority = 7;
        provider.auth = AuthCapability::Cookie;
        provider.qualities = vec![StreamQuality::High, StreamQuality::Lossless];
        registry.register_stream_resolver(Arc::new(provider));
        let descriptor = registry.descriptor("meta").unwrap();
        assert_eq!(descriptor.version.as_deref(), Some("0.2.0"));
        assert_eq!(descriptor.priority, 7);
        assert_eq!(descriptor.auth, AuthCapability::Cookie);
        assert_eq!(
            descriptor.supported_qualities,
            vec![StreamQuality::High, StreamQuality::Lossless]
        );
        // QR slot registration upgrades the declared auth capability.
        registry.register_qr_authentication(arc("meta"));
        assert_eq!(
            registry.descriptor("meta").unwrap().auth,
            AuthCapability::Qr
        );
    }

    #[test]
    fn descriptor_auth_upgrade_is_sticky_across_registration_order() {
        // AUTH_REGISTRY_DESIGN.md §5.4: the Qr upgrade must not depend on
        // which slot happens to be registered last.
        // QR first, then other slots.
        let mut registry = ProviderRegistry::new();
        registry.register_qr_authentication(arc("ordered"));
        registry.register_authentication(arc("ordered"));
        registry.register_search(arc("ordered"));
        assert_eq!(
            registry.descriptor("ordered").unwrap().auth,
            AuthCapability::Qr
        );
        // Other slots first, QR last (the pre-review pinned order).
        let mut registry = ProviderRegistry::new();
        registry.register_search(arc("reversed"));
        registry.register_authentication(arc("reversed"));
        registry.register_qr_authentication(arc("reversed"));
        assert_eq!(
            registry.descriptor("reversed").unwrap().auth,
            AuthCapability::Qr
        );
        // QR first, non-auth slot last: still Qr, not the later Cookie/None
        // derivation.
        let mut registry = ProviderRegistry::new();
        let mut cookie_provider = Stub::new("sticky");
        cookie_provider.auth = AuthCapability::Cookie;
        registry.register_qr_authentication(arc("sticky"));
        registry.register_authentication(Arc::new(cookie_provider));
        assert_eq!(
            registry.descriptor("sticky").unwrap().auth,
            AuthCapability::Qr
        );
        // Only unregister_all clears the descriptor (and with it the sticky
        // value).
        registry.unregister_all("sticky");
        assert!(registry.descriptor("sticky").is_none());
    }

    #[test]
    fn aggregate_download_bit_is_never_merged() {
        // AUTH_REGISTRY_DESIGN.md §5.6 decision: download is engine-side
        // (TRANSPORT_DESIGN.md §6); a provider claiming it must not pollute
        // the descriptor aggregate.
        let mut registry = ProviderRegistry::new();
        let mut provider = Stub::new("claims-download");
        provider.caps.download = true;
        provider.caps.track_search = true;
        registry.register_search(Arc::new(provider));
        assert!(
            registry
                .descriptor("claims-download")
                .unwrap()
                .capabilities
                .track_search
        );
        assert!(
            !registry
                .descriptor("claims-download")
                .unwrap()
                .capabilities
                .download
        );
        // The live view never merges the bit either, so a capability
        // predicate on `download` is always false and always matches the
        // empty set (AUTH_REGISTRY_DESIGN.md §5.6 observability boundary).
        assert!(
            registry
                .providers_with_capability(|c| c.download)
                .is_empty()
        );
    }

    #[test]
    fn health_snapshot_reflects_the_registry_monitor() {
        let mut registry = ProviderRegistry::with_health_config(ProviderHealthConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(30),
        });
        registry.register_search(arc("flaky"));
        assert_eq!(
            registry.health_snapshot("flaky").state,
            crate::health::CircuitState::Closed
        );
        registry.health_monitor().record_failure("flaky");
        registry.health_monitor().record_failure("flaky");
        assert_eq!(
            registry.health_snapshot("flaky").state,
            crate::health::CircuitState::Open
        );
        assert_eq!(registry.health_snapshot("flaky").consecutive_failures, 2);
        assert_eq!(
            registry.health_snapshot("unknown").state,
            crate::health::CircuitState::Closed
        );
    }

    #[test]
    fn unregister_all_resets_the_health_history() {
        // AUTH_REGISTRY_DESIGN.md §5.5: an id that leaves the registration
        // surface must not carry its Open/cooldown state into a later
        // re-registration.
        let mut registry = ProviderRegistry::with_health_config(ProviderHealthConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(30),
        });
        registry.register_search(arc("revived"));
        registry.health_monitor().record_failure("revived");
        registry.health_monitor().record_failure("revived");
        assert_eq!(
            registry.health_snapshot("revived").state,
            crate::health::CircuitState::Open
        );
        registry.unregister_all("revived");
        let snapshot = registry.health_snapshot("revived");
        assert_eq!(snapshot.state, crate::health::CircuitState::Closed);
        assert_eq!(snapshot.consecutive_failures, 0);
        // Re-registration starts fresh: the old Open no longer blocks.
        registry.register_search(arc("revived"));
        assert!(registry.health_monitor().try_acquire("revived"));
        assert_eq!(
            registry.health_snapshot("revived").state,
            crate::health::CircuitState::Closed
        );
        // A single-slot unregister keeps the id live, so health history stays.
        registry.health_monitor().record_failure("revived");
        registry.unregister_search("revived");
        assert_eq!(registry.health_snapshot("revived").consecutive_failures, 1);
    }
}
