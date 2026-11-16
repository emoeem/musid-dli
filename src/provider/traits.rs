use crate::{Error, models::*};
use async_trait::async_trait;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub track_search: bool,
    pub stream_resolve: bool,
    pub lyrics: bool,
    pub playlists: bool,
    pub albums: bool,
    pub artists: bool,
    pub charts: bool,
    pub link_resolve: bool,
    pub authentication: bool,
    pub download: bool,
}
pub trait MusicProvider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }
}
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub keyword: String,
    pub page: u32,
    pub page_size: u32,
}
impl SearchQuery {
    pub fn new(keyword: impl Into<String>) -> Self {
        Self {
            keyword: keyword.into(),
            page: 1,
            page_size: 30,
        }
    }
}
#[derive(Debug, Clone, Default)]
pub struct SearchPage {
    pub items: Vec<Track>,
    pub total: Option<u64>,
    pub has_more: bool,
}
#[derive(Debug, Clone, Copy)]
pub struct ResolveOptions {
    pub quality: StreamQuality,
}
impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            quality: StreamQuality::High,
        }
    }
}
#[async_trait]
pub trait TrackSearch: MusicProvider {
    async fn search(&self, query: &SearchQuery) -> Result<SearchPage, Error>;
}
#[async_trait]
pub trait StreamResolver: MusicProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo, Error>;
}
#[async_trait]
pub trait LyricsProvider: MusicProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>, Error>;
}
#[async_trait]
pub trait CollectionProvider: MusicProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist, Error>;
    async fn playlist_tracks(&self, _id: &str) -> Result<Vec<Track>, Error> {
        Err(Error::Unsupported(self.id().into()))
    }
}
#[async_trait]
pub trait PlaylistProvider: MusicProvider {
    async fn categories(&self) -> Result<Vec<PlaylistCategory>, Error>;
    async fn playlists(
        &self,
        category: &str,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Playlist>, Error>;
    async fn search_playlists(&self, keyword: &str, page: u32) -> Result<Vec<Playlist>, Error>;
    async fn user_playlists(&self, page: u32, page_size: u32) -> Result<Vec<Playlist>, Error>;
}
#[async_trait]
pub trait AlbumProvider: MusicProvider {
    async fn album(&self, id: &str) -> Result<Album, Error>;
    async fn album_tracks(&self, _id: &str) -> Result<Vec<Track>, Error> {
        Err(Error::Unsupported(self.id().into()))
    }
}
#[async_trait]
pub trait ArtistProvider: MusicProvider {
    async fn artist(&self, id: &str) -> Result<Artist, Error>;
    async fn artist_top_tracks(&self, _id: &str) -> Result<Vec<Track>, Error> {
        Err(Error::Unsupported(self.id().into()))
    }
    async fn artist_albums(
        &self,
        _id: &str,
        _page: u32,
        _page_size: u32,
    ) -> Result<Vec<Album>, Error> {
        Err(Error::Unsupported(self.id().into()))
    }
}
#[async_trait]
pub trait ChartProvider: MusicProvider {
    async fn charts(&self) -> Result<Vec<Chart>, Error>;
    async fn chart_tracks(&self, id: &str) -> Result<Vec<ChartEntry>, Error>;
}
#[derive(Debug, Clone)]
pub enum ResolvedLink {
    Track(Track),
    Playlist(Playlist),
    Album(Album),
    Artist(Artist),
}
#[async_trait]
pub trait LinkResolver: MusicProvider {
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink, Error>;
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStatus {
    LoggedOut,
    LoggedIn {
        user_id: Option<String>,
        display_name: Option<String>,
    },
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrLoginSession {
    pub key: String,
    pub url: String,
    pub expires_in: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrLoginStatus {
    Waiting,
    Scanned,
    Success,
    Expired,
    RiskControl,
    ServerError,
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrLoginPoll {
    pub status: QrLoginStatus,
    pub message: String,
    pub user_id: Option<String>,
    pub display_name: Option<String>,
}
#[async_trait]
pub trait QrAuthenticationProvider: MusicProvider {
    async fn create_qr_login(&self) -> Result<QrLoginSession, Error>;
    async fn poll_qr_login(&self, key: &str) -> Result<QrLoginPoll, Error>;
}

#[async_trait]
pub trait AuthenticationProvider: MusicProvider {
    async fn login_status(&self) -> Result<LoginStatus, Error>;
    async fn logout(&self) -> Result<(), Error>;
}
