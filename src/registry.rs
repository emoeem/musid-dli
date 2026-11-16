use crate::provider::*;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    pub id: String,
    pub name: String,
    pub capabilities: ProviderCapabilities,
}
#[derive(Default)]
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
}
impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }
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
            });
        entry.name = provider.name().to_owned();
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
        current.download |= incoming.download;
    }
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
        self.qr_auth.insert(p.id().to_owned(), p);
    }
    pub fn descriptor(&self, id: &str) -> Option<&ProviderDescriptor> {
        self.descriptors.get(id)
    }
    pub fn provider_ids(&self) -> Vec<String> {
        self.descriptors.keys().cloned().collect()
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
    pub fn qr_authentication_provider(
        &self,
        id: &str,
    ) -> Option<&Arc<dyn QrAuthenticationProvider>> {
        self.qr_auth.get(id)
    }
}
