//! Jamendo provider migrated from VoiceFox's `lx-source::jamendo`.
//! Creative Commons music catalog: no API key, no login, no anti-bot; the
//! web endpoints only need an `x-jam-call` signature header
//! (`$sha1(path + nonce)*nonce~`) and a fixed `x-jam-version`. The search
//! endpoints answer with plain JSON arrays. Stream URLs come in `download`
//! and `stream` groups with flac/mp3/ogg tiers; the best available tier is
//! always served (the catalog is freely downloadable, so lossless-first is
//! correct here rather than a downgrade). The catalog exposes no lyrics.
use crate::{
    Error, Result,
    models::{AudioFormat, LyricDocument, Playlist, StreamInfo, StreamQuality, Track, TrackId},
    provider::{
        AlbumProvider, CollectionProvider, LyricsProvider, MusicProvider, PlaylistProvider,
        ResolveOptions, ResolvedLink, SearchPage, SearchQuery, StreamResolver, TrackSearch,
    },
    session::SessionStore,
};
use async_trait::async_trait;
use reqwest::{Client, RequestBuilder, header};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::{collections::BTreeMap, time::Duration};

const ID: &str = "jamendo";
const UA_WEB: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/143.0 Safari/537.36";
const REFERER: &str = "https://www.jamendo.com/search?q=musicdl";
const X_JAM_VERSION: &str = "4gvfvv";
const BASE: &str = "https://www.jamendo.com";

#[derive(Clone)]
pub struct JamendoProvider {
    client: Client,
    sessions: SessionStore,
}
impl std::fmt::Debug for JamendoProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JamendoProvider")
            .field("session_store", &"redacted")
            .finish()
    }
}
impl Default for JamendoProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl JamendoProvider {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            sessions: SessionStore::default(),
        }
    }
    pub fn with_session_store(mut self, sessions: SessionStore) -> Self {
        self.sessions = sessions;
        self
    }
    pub fn session_store(&self) -> SessionStore {
        self.sessions.clone()
    }
    /// Register all currently implemented Jamendo capabilities in one step.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_playlist(provider);
    }
    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        request
            .header(header::USER_AGENT, UA_WEB)
            .header(header::REFERER, REFERER)
    }
    async fn api_get(&self, path: &str, params: &[(&str, &str)]) -> Result<Value> {
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
            format!("{BASE}{path}")
        } else {
            format!("{BASE}{path}?{query}")
        };
        let nonce = random_nonce();
        let response = self
            .request(self.client.get(&url))
            .header("x-jam-call", x_jam_call(path, nonce.as_str()))
            .header("x-jam-version", X_JAM_VERSION)
            .header("x-requested-with", "XMLHttpRequest")
            .send()
            .await
            .map_err(|e| perr(format!("api request failed: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("api request: HTTP {}", response.status())));
        }
        response
            .json()
            .await
            .map_err(|e| perr(format!("api response parse failed: {e}")))
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}
/// `x-jam-call` signature: `$sha1(path + nonce)*nonce~`.
fn x_jam_call(path: &str, nonce: &str) -> String {
    let digest = Sha1::digest(format!("{path}{nonce}").as_bytes());
    format!("${}*{nonce}~", hex::encode(digest))
}
/// Nonce in (0.1, 1.0) formatted like the upstream float.
fn random_nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(100_000_000);
    format!("{:.3}", 0.1 + (nanos % 900) as f64 / 1000.0)
}
fn jam_value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
        .or_else(|| value.as_i64().map(|n| n.to_string()))
        .unwrap_or_default()
}
fn jam_non_zero_id(value: &Value) -> Option<String> {
    let id = jam_value_string(value);
    (!id.is_empty() && id != "0").then_some(id)
}
fn jam_cover_of(item: &Value) -> Option<String> {
    item["cover"]["big"]["size300"]
        .as_str()
        .or_else(|| item["image"].as_str())
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
}
/// Descriptions arrive as strings or per-language objects.
fn jam_description_of(item: &Value) -> Option<String> {
    if let Some(value) = item["description"].as_str() {
        let value = value.trim();
        return (!value.is_empty() && value != "0").then(|| value.to_owned());
    }
    item["description"]
        .as_object()
        .and_then(|map| map.values().find_map(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}
/// Pick the best tier from the `download` / `stream` URL groups
/// (flac > mp33 > mp32 > mp3 > ogg); the catalog is freely downloadable.
fn pick_stream(item: &Value) -> Option<(String, AudioFormat, StreamQuality)> {
    let group = if item["download"]
        .as_object()
        .is_some_and(|map| !map.is_empty())
    {
        &item["download"]
    } else {
        &item["stream"]
    };
    for (key, format, quality) in [
        ("flac", AudioFormat::Flac, StreamQuality::Lossless),
        ("mp33", AudioFormat::Mp3, StreamQuality::High),
        ("mp32", AudioFormat::Mp3, StreamQuality::High),
        ("mp3", AudioFormat::Mp3, StreamQuality::Low),
        ("ogg", AudioFormat::Ogg, StreamQuality::Low),
    ] {
        if let Some(url) = group[key].as_str().filter(|url| !url.is_empty()) {
            return Some((url.to_owned(), format, quality));
        }
    }
    None
}
fn jam_parse_track(item: &Value) -> Option<Track> {
    let id = jam_non_zero_id(&item["id"])?;
    let title = item["name"].as_str()?.trim().to_owned();
    if title.is_empty() {
        return None;
    }
    let mut extra = BTreeMap::new();
    extra.insert("track_id".into(), id.clone());
    // Search responses carry the full download/stream URL groups; embedding
    // the best URL here keeps resolution offline (the batch /api/tracks
    // endpoint hangs anonymously, live-verified 2026-11-17).
    if let Some((url, format, quality)) = pick_stream(item) {
        extra.insert("stream_url".into(), url);
        extra.insert("stream_format".into(), format!("{format:?}"));
        extra.insert("stream_quality".into(), format!("{quality:?}"));
    }
    Some(Track {
        id: TrackId::new(ID, id),
        title,
        artists: vec![
            item["artist"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ],
        album: item["album"]["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        album_id: jam_non_zero_id(&item["albumId"])
            .or_else(|| jam_non_zero_id(&item["album"]["id"])),
        duration: {
            let seconds = item["duration"].as_u64().unwrap_or_default();
            (seconds > 0).then(|| Duration::from_secs(seconds))
        },
        artwork_url: jam_cover_of(item),
        isrc: None,
        available_qualities: pick_stream(item)
            .map(|(_, _, quality)| vec![quality])
            .unwrap_or_default(),
        extra,
    })
}
fn jam_parse_playlist(item: &Value) -> Option<Playlist> {
    let id = jam_non_zero_id(&item["id"])?;
    let title = item["name"].as_str()?.trim().to_owned();
    if title.is_empty() {
        return None;
    }
    Some(Playlist {
        id: TrackId::new(ID, id.clone()),
        title,
        description: jam_description_of(item),
        artwork_url: jam_cover_of(item),
        track_count: None,
        tracks: vec![],
    })
}
/// Link shapes: `jamendo.com/{track,album,playlist}/{numeric id}`.
#[allow(dead_code)]
enum JamendoLink {
    Track(String),
    Album(String),
    Playlist(String),
}
fn link_target(link: &str) -> Option<JamendoLink> {
    let trimmed = link.trim();
    for (segment, build) in [("track", 0usize), ("album", 1usize), ("playlist", 2usize)] {
        let marker = format!("jamendo.com/{segment}/");
        if let Some(index) = trimmed.find(&marker) {
            let id = trimmed[index + marker.len()..]
                .split(['/', '?', '#'])
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned();
            if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
                return match build {
                    0 => Some(JamendoLink::Track(id)),
                    1 => Some(JamendoLink::Album(id)),
                    _ => Some(JamendoLink::Playlist(id)),
                };
            }
        }
    }
    None
}
impl MusicProvider for JamendoProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        "Jamendo"
    }
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            // Container endpoints hang anonymously; capabilities stay false.
            playlists: false,
            albums: false,
            link_resolve: false,
            ..Default::default()
        }
    }
}
#[async_trait]
impl TrackSearch for JamendoProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.keyword.trim().is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        // The search endpoint answers with a plain array, not an envelope.
        let value = self
            .api_get(
                "/api/search",
                &[
                    ("query", q.keyword.trim()),
                    ("type", "track"),
                    ("limit", "20"),
                    ("identities", "www"),
                ],
            )
            .await?;
        let items = value
            .as_array()
            .map(|items| items.iter().filter_map(jam_parse_track).collect::<Vec<_>>())
            .unwrap_or_default();
        Ok(SearchPage {
            has_more: false,
            total: Some(items.len() as u64),
            items,
        })
    }
}
#[async_trait]
impl StreamResolver for JamendoProvider {
    async fn resolve(&self, track: &Track, _options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to Jamendo".into(),
            ));
        }
        // The best tier is always served (the catalog is freely downloadable)
        // and it was already embedded at search time: the batch /api/tracks
        // endpoint hangs anonymously, so no extra request is made here.
        let url = track
            .extra
            .get("stream_url")
            .cloned()
            .ok_or_else(|| perr("track carries no embedded stream URL"))?;
        let format = match track.extra.get("stream_format").map(String::as_str) {
            Some("Flac") => AudioFormat::Flac,
            Some("Ogg") => AudioFormat::Ogg,
            _ => AudioFormat::Mp3,
        };
        let achieved = match track.extra.get("stream_quality").map(String::as_str) {
            Some("Lossless") => StreamQuality::Lossless,
            Some("High") => StreamQuality::High,
            _ => StreamQuality::Low,
        };
        Ok(StreamInfo {
            url,
            quality: achieved,
            format,
            duration: track.duration,
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
// Album / playlist container endpoints (`/api/albums`, `/api/playlists`,
// `/api/tracks`) hang anonymously since the 2026-11-17 live check, so the
// container capabilities stay false until Jamendo restores them.
#[async_trait]
impl AlbumProvider for JamendoProvider {
    async fn album(&self, _id: &str) -> Result<crate::models::Album> {
        Err(Error::Unsupported(
            "jamendo container endpoints are unavailable anonymously".into(),
        ))
    }
    async fn album_tracks(&self, _id: &str) -> Result<Vec<Track>> {
        Err(Error::Unsupported(
            "jamendo container endpoints are unavailable anonymously".into(),
        ))
    }
}
#[async_trait]
impl CollectionProvider for JamendoProvider {
    async fn playlist(&self, _id: &str) -> Result<Playlist> {
        Err(Error::Unsupported(
            "jamendo container endpoints are unavailable anonymously".into(),
        ))
    }
    async fn playlist_tracks(&self, _id: &str) -> Result<Vec<Track>> {
        Err(Error::Unsupported(
            "jamendo container endpoints are unavailable anonymously".into(),
        ))
    }
}
#[async_trait]
impl crate::provider::LinkResolver for JamendoProvider {
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink> {
        let target = link_target(url).ok_or_else(|| {
            Error::InvalidInput("unrecognized Jamendo track/album/playlist link".into())
        })?;
        // Container fetches hang anonymously (live-verified); only the parsed
        // kind can be reported honestly.
        let _ = target;
        Err(Error::Unsupported(
            "jamendo container endpoints are unavailable anonymously".into(),
        ))
    }
}
#[async_trait]
impl PlaylistProvider for JamendoProvider {
    async fn categories(&self) -> Result<Vec<crate::models::PlaylistCategory>> {
        Err(Error::Unsupported(ID.into()))
    }
    async fn playlists(
        &self,
        _category: &str,
        _page: u32,
        _page_size: u32,
    ) -> Result<Vec<Playlist>> {
        // Upstream exposes no directory endpoint; search covers discovery.
        Err(Error::Unsupported(ID.into()))
    }
    async fn search_playlists(&self, keyword: &str, _page: u32) -> Result<Vec<Playlist>> {
        if keyword.trim().is_empty() {
            return Err(Error::InvalidInput(
                "playlist search keyword is empty".into(),
            ));
        }
        let value = self
            .api_get(
                "/api/search",
                &[
                    ("query", keyword.trim()),
                    ("type", "playlist"),
                    ("limit", "20"),
                    ("identities", "www"),
                ],
            )
            .await?;
        Ok(value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(jam_parse_playlist)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default())
    }
    async fn user_playlists(&self, _page: u32, _page_size: u32) -> Result<Vec<Playlist>> {
        Err(Error::Unsupported(ID.into()))
    }
}
/// Jamendo's CC catalog exposes no lyrics.
#[async_trait]
impl LyricsProvider for JamendoProvider {
    async fn lyrics(&self, _track: &Track) -> Result<Option<LyricDocument>> {
        Err(Error::Unsupported(ID.into()))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn x_jam_call_matches_the_documented_shape() {
        let header = x_jam_call("/api/search", "0.5");
        assert!(header.starts_with('$'));
        assert!(header.ends_with('~'));
        assert!(header.contains("*0.5"));
        let expected = format!("${}*0.5~", hex::encode(Sha1::digest(b"/api/search0.5")));
        assert_eq!(header, expected);
        assert_eq!(header, x_jam_call("/api/search", "0.5"));
    }
    #[test]
    fn picks_the_best_available_stream() {
        let flac = json!({
            "download": {"flac": "https://cdn/a.flac", "mp3": "https://cdn/a.mp3"}
        });
        let (url, format, quality) = pick_stream(&flac).unwrap();
        assert_eq!(url, "https://cdn/a.flac");
        assert_eq!(format, AudioFormat::Flac);
        assert_eq!(quality, StreamQuality::Lossless);
        // Empty download group falls back to the streaming group.
        let streaming = json!({
            "download": {},
            "stream": {"mp32": "https://cdn/b.mp3", "ogg": "https://cdn/b.ogg"}
        });
        let (url, format, _) = pick_stream(&streaming).unwrap();
        assert_eq!(url, "https://cdn/b.mp3");
        assert_eq!(format, AudioFormat::Mp3);
        assert!(pick_stream(&json!({})).is_none());
    }
    #[test]
    fn parses_tracks_and_playlists() {
        let track = json!({
            "id": 100,
            "name": "Track",
            "duration": 200,
            "album": {"id": 7, "name": "Album"},
            "artist": {"name": "Artist"},
            "cover": {"big": {"size300": "https://cdn/c.jpg"}},
            "download": {"flac": "https://cdn/a.flac"}
        });
        let track = jam_parse_track(&track).unwrap();
        assert_eq!(track.id.id, "100");
        assert_eq!(track.artists, vec!["Artist"]);
        assert_eq!(track.album.as_deref(), Some("Album"));
        assert_eq!(track.duration.map(|d| d.as_secs()), Some(200));
        assert!(track.available_qualities.contains(&StreamQuality::Lossless));
        assert_eq!(track.extra.get("track_id").map(String::as_str), Some("100"));
        let playlist = jam_parse_playlist(&json!({
            "id": 5,
            "name": "Mix",
            "user_name": "someone",
            "image": "https://cdn/p.jpg"
        }))
        .unwrap();
        assert_eq!(playlist.id.id, "5");
        assert!(playlist.artwork_url.is_some());
        assert!(jam_parse_playlist(&json!({"id": 0, "name": "x"})).is_none());
    }
    #[test]
    fn album_description_falls_back_to_any_language() {
        let item = json!({"description": {"fr": "Bonjour", "en": "Hello"}});
        let description = jam_description_of(&item).unwrap();
        assert!(description == "Hello" || description == "Bonjour");
        assert_eq!(jam_description_of(&json!({"description": ""})), None);
    }
    #[test]
    fn recognises_jamendo_links() {
        assert!(matches!(
            link_target("https://www.jamendo.com/track/123"),
            Some(JamendoLink::Track(_))
        ));
        assert!(matches!(
            link_target("https://www.jamendo.com/album/456?lang=en"),
            Some(JamendoLink::Album(_))
        ));
        assert!(matches!(
            link_target("https://www.jamendo.com/playlist/789"),
            Some(JamendoLink::Playlist(_))
        ));
        assert!(link_target("https://www.jamendo.com/").is_none());
        assert!(link_target("周杰伦").is_none());
    }
    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = JamendoProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        assert!(descriptor.capabilities.stream_resolve);
        // Container endpoints hang anonymously; capabilities stay false.
        assert!(!descriptor.capabilities.playlists);
        assert!(!descriptor.capabilities.albums);
        assert!(!descriptor.capabilities.link_resolve);
        // The CC catalog has no lyric API.
        assert!(!descriptor.capabilities.lyrics);
        assert!(!descriptor.capabilities.authentication);
    }
}
