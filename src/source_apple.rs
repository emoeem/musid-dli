//! Apple Music provider migrated from VoiceFox's `lx-source::apple`.
//!
//! Contract limits stated up front: Apple Music full tracks are DRM-protected
//! and only **30-second preview URLs** are obtainable (`attributes.previews`).
//! Every StreamInfo this provider hands out is a preview; the capability set
//! reports Low quality only and the docs say so. Search, song/album/playlist
//! metadata, previews and plain-text lyrics come from the unsigned AMP API
//! with a web-embedded JWT token.
use crate::{
    Error, Result,
    models::{
        AudioFormat, LyricDocument, LyricFormat, Playlist, PlaylistCategory, StreamInfo,
        StreamQuality, Track, TrackId,
    },
    provider::{
        AlbumProvider, AuthenticationProvider, CollectionProvider, LoginStatus, LyricsProvider,
        MusicProvider, PlaylistProvider, ResolveOptions, ResolvedLink, SearchPage, SearchQuery,
        StreamResolver, TrackSearch,
    },
    session::SessionStore,
};
use async_trait::async_trait;
use reqwest::{Client, RequestBuilder, header};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::OnceLock,
    time::{Duration, Instant},
};

const ID: &str = "applemusic";
const UA_WEB: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/143.0 Safari/537.36";
const HOME: &str = "https://music.apple.com";
const AMP: &str = "https://amp-api.music.apple.com";
const STOREFRONT: &str = "us";

#[derive(Clone)]
pub struct AppleMusicProvider {
    client: Client,
    sessions: SessionStore,
    /// Web-embedded JWT, fetched once and probed against a minimal search.
    token: std::sync::Arc<OnceLock<String>>,
    token_fetched_at: std::sync::Arc<std::sync::Mutex<Option<Instant>>>,
}
impl std::fmt::Debug for AppleMusicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppleMusicProvider")
            .field("session_store", &"redacted")
            .finish()
    }
}
impl Default for AppleMusicProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl AppleMusicProvider {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            sessions: SessionStore::default(),
            token: std::sync::Arc::new(OnceLock::new()),
            token_fetched_at: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
    pub fn with_session_store(mut self, sessions: SessionStore) -> Self {
        self.sessions = sessions;
        self
    }
    pub fn session_store(&self) -> SessionStore {
        self.sessions.clone()
    }
    /// Register all currently implemented Apple Music capabilities.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_lyrics(provider.clone());
        registry.register_album(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_link_resolver(provider);
    }
    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        request.header(header::USER_AGENT, UA_WEB)
    }
    async fn raw_text(&self, url: &str, label: &str) -> Result<String> {
        let response = self
            .request(self.client.get(url))
            .send()
            .await
            .map_err(|e| perr(format!("{label}: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("{label}: HTTP {}", response.status())));
        }
        response
            .text()
            .await
            .map_err(|e| perr(format!("{label}: {e}")))
    }
    /// Homepage → script path without regex. The bundle name moved over
    /// time (`index-legacy~…`, `index-legacy-…`, plain `index~…`), so all
    /// known marker shapes are tried before giving up.
    fn script_path(homepage: &str) -> Option<String> {
        const MARKERS: [&str; 3] = ["/assets/index-legacy", "/assets/index~", "/assets/index-"];
        for marker in MARKERS {
            let Some(start) = homepage.find(marker) else {
                continue;
            };
            let tail = &homepage[start..];
            let end = tail.find(['"', '\'']).unwrap_or(tail.len());
            let path = tail[..end].trim_end().to_owned();
            if path.ends_with(".js") {
                return Some(path);
            }
        }
        None
    }
    /// JWT candidates: `eyJ…` tokens of at least 40 JWT-ish characters.
    fn extract_tokens(script: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut from = 0;
        while let Some(pos) = script[from..].find("eyJ") {
            let start = from + pos;
            let token: String = script[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                .collect();
            if token.len() >= 40 {
                tokens.push(token);
            }
            from = start + 3;
        }
        tokens.dedup();
        tokens
    }
    async fn token(&self) -> Result<String> {
        if let Some(token) = self.token.get() {
            return Ok(token.clone());
        }
        // Token rotation guard: re-fetch at most once per minute.
        if let Some(fetched) = *self
            .token_fetched_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            && fetched.elapsed() < Duration::from_secs(60)
        {
            return Err(perr("Apple Music token unavailable; retry shortly"));
        }
        *self
            .token_fetched_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let homepage = self.raw_text(HOME, "homepage").await?;
        let path = Self::script_path(&homepage)
            .ok_or_else(|| perr("homepage script not found (page layout changed)"))?;
        let script = self.raw_text(&format!("{HOME}/{path}"), "script").await?;
        let candidates = Self::extract_tokens(&script);
        if candidates.is_empty() {
            return Err(perr("script contains no token candidates"));
        }
        // Probe each candidate with a minimal search; a specific song id would
        // 404 on storefront grounds, which says nothing about the token.
        for candidate in candidates {
            let probe = self
                .client
                .get(format!(
                    "{AMP}/v1/catalog/{STOREFRONT}/search?term=music&types=songs&limit=1"
                ))
                .header(header::USER_AGENT, UA_WEB)
                .header(header::AUTHORIZATION, format!("Bearer {candidate}"))
                .header(header::ORIGIN, HOME)
                .send()
                .await;
            if let Ok(response) = probe
                && response.status().is_success()
            {
                let _ = self.token.set(candidate.clone());
                return Ok(candidate);
            }
        }
        Err(perr("no usable Apple Music token found"))
    }
    async fn amp_get(&self, uri: &str, params: &[(&str, &str)]) -> Result<Value> {
        let token = self.token().await?;
        let query = params
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(key),
                    urlencoding::encode(value)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let url = if query.is_empty() {
            format!("{AMP}{uri}")
        } else {
            format!("{AMP}{uri}?{query}")
        };
        let response = self
            .request(self.client.get(&url))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::ORIGIN, HOME)
            .send()
            .await
            .map_err(|e| perr(format!("AMP request failed: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("AMP request: HTTP {}", response.status())));
        }
        response
            .json()
            .await
            .map_err(|e| perr(format!("AMP response parse failed: {e}")))
    }
    async fn container_tracks(&self, kind: &str, id: &str) -> Result<(Playlist, Vec<Track>)> {
        let json = self
            .amp_get(
                &format!("/v1/catalog/{STOREFRONT}/{kind}s/{id}"),
                &[("include", "tracks"), ("limit[tracks]", "100")],
            )
            .await?;
        let item = json["data"]
            .as_array()
            .and_then(|items| items.first())
            .ok_or(Error::NotFound)?;
        let playlist = apple_parse_playlist(item).ok_or(Error::NotFound)?;
        let tracks = item["relationships"]["tracks"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(apple_parse_song)
            .collect::<Vec<_>>();
        Ok((playlist, tracks))
    }
    async fn fetch_song_detail(&self, id: &str) -> Result<(Track, Option<String>)> {
        let json = self
            .amp_get(
                &format!("/v1/catalog/{STOREFRONT}/songs/{id}"),
                &[("extend", "extendedAssetUrls")],
            )
            .await?;
        let item = json["data"]
            .as_array()
            .and_then(|items| items.first())
            .ok_or(Error::NotFound)?;
        let mut track = apple_parse_song(item).ok_or(Error::NotFound)?;
        // Song details carry the album id in relationships (search results
        // do not), so album resolution goes through the detail call.
        let album_id = item["relationships"]["albums"]["data"]
            .as_array()
            .and_then(|albums| albums.first())
            .and_then(|album| album["id"].as_str())
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        if album_id.is_some() {
            track.album_id = album_id;
        }
        let preview = item["attributes"]["previews"]
            .as_array()
            .and_then(|previews| previews.first())
            .and_then(|preview| preview["url"].as_str())
            .filter(|url| !url.is_empty())
            .map(str::to_owned);
        Ok((track, preview))
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}
/// Cover templates carry `{w}`/`{h}` placeholders.
fn artwork_url(template: &str, size: u32) -> Option<String> {
    let template = template.trim();
    if template.is_empty() {
        return None;
    }
    Some(
        template
            .replace("{w}", &size.to_string())
            .replace("{h}", &size.to_string()),
    )
}
fn apple_parse_song(item: &Value) -> Option<Track> {
    let id = item["id"].as_str()?.trim().to_owned();
    let attributes = item.get("attributes")?;
    let title = attributes["name"].as_str()?.trim().to_owned();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    let mut extra = BTreeMap::new();
    extra.insert("song_id".into(), id.clone());
    // Every stream handed out is a 30-second preview.
    extra.insert("preview_only".into(), "true".into());
    Some(Track {
        id: TrackId::new(ID, id),
        title,
        artists: vec![
            attributes["artistName"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ],
        album: attributes["albumName"]
            .as_str()
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        album_id: None,
        duration: {
            let millis = attributes["durationInMillis"].as_u64().unwrap_or_default();
            (millis > 0).then(|| Duration::from_millis(millis))
        },
        artwork_url: attributes["artwork"]["url"]
            .as_str()
            .and_then(|template| artwork_url(template, 600)),
        isrc: None,
        available_qualities: vec![StreamQuality::Low],
        extra,
    })
}
fn apple_parse_playlist(item: &Value) -> Option<Playlist> {
    let id = item["id"].as_str()?.trim().to_owned();
    let attributes = item.get("attributes")?;
    let title = attributes["name"].as_str()?.trim().to_owned();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(Playlist {
        id: TrackId::new(ID, id),
        title,
        description: attributes["description"]["standard"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        artwork_url: attributes["artwork"]["url"]
            .as_str()
            .and_then(|template| artwork_url(template, 600)),
        track_count: attributes["trackCount"].as_u64().map(|n| n as u32),
        tracks: vec![],
    })
}
impl MusicProvider for AppleMusicProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        "Apple Music"
    }
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            track_search: true,
            // Stream resolution exists, but every URL is a 30-second preview.
            stream_resolve: true,
            lyrics: true,
            playlists: true,
            albums: true,
            link_resolve: true,
            ..Default::default()
        }
    }
}
#[async_trait]
impl TrackSearch for AppleMusicProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.keyword.trim().is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        let json = self
            .amp_get(
                &format!("/v1/catalog/{STOREFRONT}/search"),
                &[
                    ("term", q.keyword.trim()),
                    ("types", "songs"),
                    ("limit", "30"),
                ],
            )
            .await?;
        let items = json["results"]["songs"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(apple_parse_song)
            .collect::<Vec<_>>();
        let total = items.len() as u64;
        Ok(SearchPage {
            items,
            total: Some(total),
            has_more: false,
        })
    }
}
/// Streams are the 30-second previews; quality stays Low by contract.
#[async_trait]
impl StreamResolver for AppleMusicProvider {
    async fn resolve(&self, track: &Track, _options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to Apple Music".into(),
            ));
        }
        let song_id = track
            .extra
            .get("song_id")
            .cloned()
            .unwrap_or_else(|| track.id.id.clone());
        let (_, preview) = self.fetch_song_detail(&song_id).await?;
        let url = preview.ok_or_else(|| perr("no preview available for this track"))?;
        Ok(StreamInfo {
            url,
            quality: StreamQuality::Low,
            format: AudioFormat::M4a,
            duration: Some(Duration::from_secs(30)),
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
/// Plain-text lyrics from `relationships.lyrics`; the catalog has no timing.
#[async_trait]
impl LyricsProvider for AppleMusicProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to Apple Music".into(),
            ));
        }
        let song_id = track
            .extra
            .get("song_id")
            .cloned()
            .unwrap_or_else(|| track.id.id.clone());
        let json = self
            .amp_get(
                &format!("/v1/catalog/{STOREFRONT}/songs/{song_id}"),
                &[("include", "lyrics")],
            )
            .await?;
        let text = json["data"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["relationships"]["lyrics"]["data"].as_array())
            .and_then(|lyrics| lyrics.first())
            .and_then(|lyric| lyric["attributes"]["text"].as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        match text {
            Some(original) => Ok(Some(LyricDocument {
                format: LyricFormat::PlainText,
                lines: vec![],
                original,
                translation: None,
                romanization: None,
            })),
            None => Ok(None),
        }
    }
}
#[async_trait]
impl AlbumProvider for AppleMusicProvider {
    async fn album(&self, id: &str) -> Result<crate::models::Album> {
        let (playlist, tracks) = self.container_tracks("album", id).await?;
        Ok(crate::models::Album {
            id: playlist.id,
            title: playlist.title,
            artists: vec![],
            artwork_url: playlist.artwork_url,
            track_count: Some(tracks.len() as u32),
        })
    }
    async fn album_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let (_, tracks) = self.container_tracks("album", id).await?;
        Ok(tracks)
    }
}
#[async_trait]
impl CollectionProvider for AppleMusicProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist> {
        let (playlist, _) = self.container_tracks("playlist", id).await?;
        Ok(playlist)
    }
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let (_, tracks) = self.container_tracks("playlist", id).await?;
        Ok(tracks)
    }
}
#[async_trait]
impl crate::provider::LinkResolver for AppleMusicProvider {
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink> {
        // Apple links embed the kind and id: music.apple.com/{storefront}/{kind}/{id}
        let lower = url.to_ascii_lowercase();
        let mut parts = lower.split('/').filter(|part| !part.is_empty());
        let mut kind = None;
        let mut id = None;
        while let Some(part) = parts.next() {
            if matches!(part, "song" | "album" | "playlist") {
                kind = Some(part.to_owned());
                id = parts.next().map(str::to_owned);
                break;
            }
        }
        let Some(kind) = kind else {
            return Err(Error::InvalidInput(
                "unrecognized Apple Music song/album/playlist link".into(),
            ));
        };
        let Some(id) = id.filter(|id| !id.is_empty()) else {
            return Err(Error::InvalidInput("apple link has no id".into()));
        };
        match kind.as_str() {
            "song" => {
                let (track, _) = self.fetch_song_detail(&id).await?;
                Ok(ResolvedLink::Track(track))
            }
            "album" => {
                let (playlist, tracks) = self.container_tracks("album", &id).await?;
                Ok(ResolvedLink::Album(crate::models::Album {
                    id: playlist.id,
                    title: playlist.title,
                    artists: vec![],
                    artwork_url: playlist.artwork_url,
                    track_count: Some(tracks.len() as u32),
                }))
            }
            _ => {
                let (playlist, _) = self.container_tracks("playlist", &id).await?;
                Ok(ResolvedLink::Playlist(playlist))
            }
        }
    }
}
/// Apple exposes no login surface to the catalog API: always anonymous.
#[async_trait]
impl AuthenticationProvider for AppleMusicProvider {
    async fn login_status(&self) -> Result<LoginStatus> {
        Ok(LoginStatus::LoggedOut)
    }
    async fn logout(&self) -> Result<()> {
        Ok(())
    }
}
/// Apple has no public playlist-search-type endpoint in this shape; search
/// covers songs and the curated categories would need a hardcoded table.
#[async_trait]
impl PlaylistProvider for AppleMusicProvider {
    async fn categories(&self) -> Result<Vec<PlaylistCategory>> {
        Err(Error::Unsupported(ID.into()))
    }
    async fn playlists(
        &self,
        _category: &str,
        _page: u32,
        _page_size: u32,
    ) -> Result<Vec<Playlist>> {
        Err(Error::Unsupported(ID.into()))
    }
    async fn search_playlists(&self, keyword: &str, _page: u32) -> Result<Vec<Playlist>> {
        if keyword.trim().is_empty() {
            return Err(Error::InvalidInput(
                "playlist search keyword is empty".into(),
            ));
        }
        let json = self
            .amp_get(
                &format!("/v1/catalog/{STOREFRONT}/search"),
                &[
                    ("term", keyword.trim()),
                    ("types", "playlists"),
                    ("limit", "20"),
                ],
            )
            .await?;
        Ok(json["results"]["playlists"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(apple_parse_playlist)
            .collect())
    }
    async fn user_playlists(&self, _page: u32, _page_size: u32) -> Result<Vec<Playlist>> {
        Err(Error::Unsupported(ID.into()))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_script_path_without_regex() {
        let homepage = r#"<script src="/assets/index-legacy-abc123.js" crossorigin>"#;
        assert_eq!(
            AppleMusicProvider::script_path(homepage).as_deref(),
            Some("/assets/index-legacy-abc123.js")
        );
        assert!(AppleMusicProvider::script_path("<html></html>").is_none());
    }
    #[test]
    fn extracts_jwt_candidates() {
        let script =
            r#"var a="eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9.extra-token-abc-123-456";var b="short""#;
        let tokens = AppleMusicProvider::extract_tokens(script);
        assert_eq!(tokens.len(), 1);
        assert!(tokens[0].starts_with("eyJ"));
    }
    #[test]
    fn artwork_templates_are_sized() {
        assert_eq!(
            artwork_url("https://is1-ssl.mzstatic.com/x/{w}x{h}.jpg", 600).as_deref(),
            Some("https://is1-ssl.mzstatic.com/x/600x600.jpg")
        );
        assert!(artwork_url("", 600).is_none());
    }
    #[test]
    fn parses_song_attributes() {
        let item = json!({
            "id": "1440833098",
            "attributes": {
                "name": "晴天",
                "artistName": "周杰伦",
                "albumName": "叶惠美",
                "durationInMillis": 269000,
                "artwork": {"url": "https://is1-ssl.mzstatic.com/{w}x{h}.jpg"},
                "previews": [{"url": "https://audio-ssl.itunes.cn/a.m4a"}]
            }
        });
        let track = apple_parse_song(&item).unwrap();
        assert_eq!(track.id.id, "1440833098");
        assert_eq!(track.artists, vec!["周杰伦"]);
        assert_eq!(track.duration.map(|d| d.as_millis()), Some(269000));
        assert_eq!(track.available_qualities, vec![StreamQuality::Low]);
        assert_eq!(
            track.extra.get("preview_only").map(String::as_str),
            Some("true")
        );
        assert!(apple_parse_song(&json!({"id": "1"})).is_none());
    }
    #[test]
    fn parses_playlist_attributes() {
        let item = json!({
            "id": "pl.abc",
            "attributes": {
                "name": "今日热榜",
                "trackCount": 50,
                "artwork": {"url": "https://is1-ssl.mzstatic.com/{w}x{h}.jpg"}
            }
        });
        let playlist = apple_parse_playlist(&item).unwrap();
        assert_eq!(playlist.id.id, "pl.abc");
        assert_eq!(playlist.track_count, Some(50));
    }
    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = AppleMusicProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        // Streams exist but are previews only; the quality contract stays Low.
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.lyrics);
        assert!(descriptor.capabilities.playlists);
        assert!(descriptor.capabilities.albums);
        assert!(!descriptor.capabilities.authentication);
        assert!(registry.stream_provider(ID).is_some());
    }
}
