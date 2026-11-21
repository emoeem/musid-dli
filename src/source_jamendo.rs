//! Jamendo provider migrated from VoiceFox's `lx-source::jamendo`.
//! Creative Commons music catalog: no API key, no login, no anti-bot; the
//! web endpoints only need an `x-jam-call` signature header
//! (`$sha1(path + nonce)*nonce~`) and a fixed `x-jam-version`. The search
//! endpoints answer with plain JSON arrays. Stream URLs come in `download`
//! and `stream` groups with flac/mp3/ogg tiers; the best available tier is
//! always served (the catalog is freely downloadable, so lossless-first is
//! correct here rather than a downgrade). The catalog exposes no lyrics.
use crate::{
    Error, ErrorDetail, Result,
    models::{AudioFormat, LyricDocument, Playlist, StreamInfo, StreamQuality, Track, TrackId},
    provider::{
        AlbumProvider, CollectionProvider, LyricsProvider, MusicProvider, PlaylistProvider,
        ResolveOptions, ResolvedLink, SearchPage, SearchQuery, StreamResolver, TrackSearch,
    },
    session::SessionStore,
    transport::{ClientProfile, Http, RetryPolicy, TransportError, TransportRequest},
};
use async_trait::async_trait;
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
    http: Http,
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
        // Client construction moved onto the unified transport layer: the
        // profile carries exactly today's knobs (15s total timeout, follow
        // redirects); the build fallback shape lives in `build_client`
        // (TRANSPORT_DESIGN.md §4.3).
        let http = Http::from_profile(&ClientProfile::provider_default());
        Self {
            http,
            sessions: SessionStore::default(),
        }
    }

    /// Test seam: inject a transport handle (a mock in unit tests). The
    /// production constructor is [`JamendoProvider::new`]. Test-only so
    /// non-test builds see no dead constructor.
    #[cfg(test)]
    pub fn with_transport(http: Http) -> Self {
        Self {
            http,
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
    /// Adds the fixed web identity headers to a [`TransportRequest`].
    /// Platform-specific and kept in the provider: the UA/Referer pair is the
    /// web identity the search endpoints expect (TRANSPORT_DESIGN.md §1).
    fn request(&self, request: TransportRequest) -> TransportRequest {
        request
            .header("user-agent", UA_WEB)
            .header("referer", REFERER)
    }
    /// GET one JSON endpoint through the unified transport layer.
    ///
    /// Declared behavior upgrade (TRANSPORT_DESIGN.md §5.4/§5.5 row 3, slice
    /// 6, review P-1): this face was single-shot — `api_get` had no retry
    /// loop, so any transport error or 429/5xx was terminal. Through
    /// `send_raw` it joins the [`RetryPolicy::standard()`] ladder: transport
    /// errors, 429 and 5xx retry up to 4 extra attempts with exponential
    /// backoff (0.5/1/2/4s, 8s cap) + ≤250ms jitter + a server `Retry-After`
    /// clamped to 10s — worst case ≈ 83.5s per call, ≈116s when a
    /// `Retry-After` rides every retry. A mid-body connection reset (terminal
    /// "api request failed: ..." today) also becomes retryable (§5.5 row 4).
    /// Resolve-timeout reconciliation: jamendo's `resolve()` is zero-network
    /// (the stream URL is embedded at search time, live-verified 2026-11-17),
    /// so no face of this provider sits on the streaming resolve path and the
    /// resolve-level `per_provider_timeout` (resolve.rs:118) cannot cancel
    /// this ladder mid-flight; only the search faces gained the ladder, and
    /// hosts must simply tolerate a slower failing search (≈83.5s worst case
    /// vs ≈15s before) or pass a reduced `transport_retries`.
    ///
    /// Everything platform-specific stays here: the URL query encoding, the
    /// `x-jam-call` signature (`$sha1(path + nonce)*nonce~`, `x_jam_call`),
    /// `x-jam-version` and `x-requested-with`. Error texts keep today's
    /// shapes: "api request: HTTP {status}" for terminal statuses (the
    /// engine's `classify_http` Fatal text is exactly "HTTP {status}") and
    /// "api request failed: {e}" for send errors. One declared text drift:
    /// parse failures now carry the serde_json message (buffered-body decode)
    /// instead of reqwest's "error decoding response body: ..." wrapper
    /// (§5.5 row 7 class).
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
        let request = self
            .request(TransportRequest::get(url))
            .header("x-jam-call", x_jam_call(path, nonce.as_str()))
            .header("x-jam-version", X_JAM_VERSION)
            .header("x-requested-with", "XMLHttpRequest");
        let response = match self.http.send_raw(&request, &RetryPolicy::standard()).await {
            Ok(response) => response,
            // classify_http Fatal text is "HTTP {status}", so this keeps
            // today's "api request: HTTP {status}" shape byte-exact.
            Err(TransportError::Status(message)) => {
                return Err(perr(format!("api request: {message}")));
            }
            Err(error) => return Err(perr(format!("api request failed: {error}"))),
        };
        response
            .json()
            .map_err(|error| perr(format!("api response parse failed: {error}")))
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}
/// The typed landing for the four faces the platform/build does not
/// implement (AUTH_REGISTRY_DESIGN.md §1.3, bilibili §9.4 shape): the
/// provider id rides the typed slot and the site reason the detail slot,
/// where the legacy `Error::Unsupported` payload could not answer
/// `provider_id()`. Capability flags stay false, so callers that consult
/// them never reach these.
fn unsupported_operation(site: &str) -> Error {
    Error::UnsupportedOperation {
        provider: Some(ID.into()),
        detail: Some(ErrorDetail::new(site)),
    }
}
/// The typed landing for the five container faces (`/api/albums`,
/// `/api/playlists`, `/api/tracks`): those faces are implemented, but the
/// endpoints hang anonymously since the 2026-11-17 live check — a temporary
/// upstream condition ("until Jamendo restores them", see the impl blocks
/// below), not a missing implementation. The honest category is therefore
/// `Unavailable` (retryable), with the pre-migration refusal text preserved
/// in the detail slot; the capability bits stay false and unchanged, so
/// capability-consulting hosts never reach these.
fn container_unavailable() -> Error {
    Error::Unavailable {
        provider: Some(ID.into()),
        detail: Some(ErrorDetail::new(
            "jamendo container endpoints are unavailable anonymously",
        )),
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
        audio: None,
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
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
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
            size_is_advisory: true,
            checksum_algorithm: None,
            fallback_endpoints: vec![],
        })
    }
}
// Album / playlist container endpoints (`/api/albums`, `/api/playlists`,
// `/api/tracks`) hang anonymously since the 2026-11-17 live check, so the
// container capabilities stay false until Jamendo restores them. The five
// faces below are implemented; their refusal is the temporary upstream
// condition, hence `Unavailable` (`container_unavailable`), not
// `UnsupportedOperation`.
#[async_trait]
impl AlbumProvider for JamendoProvider {
    async fn album(&self, _id: &str) -> Result<crate::models::Album> {
        Err(container_unavailable())
    }
    async fn album_tracks(&self, _id: &str) -> Result<Vec<Track>> {
        Err(container_unavailable())
    }
}
#[async_trait]
impl CollectionProvider for JamendoProvider {
    async fn playlist(&self, _id: &str) -> Result<Playlist> {
        Err(container_unavailable())
    }
    async fn playlist_tracks(&self, _id: &str) -> Result<Vec<Track>> {
        Err(container_unavailable())
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
        Err(container_unavailable())
    }
}
#[async_trait]
impl PlaylistProvider for JamendoProvider {
    async fn categories(&self) -> Result<Vec<crate::models::PlaylistCategory>> {
        // No category endpoint exists upstream.
        Err(unsupported_operation(
            "no playlist category endpoint exists upstream",
        ))
    }
    async fn playlists(
        &self,
        _category: &str,
        _page: u32,
        _page_size: u32,
    ) -> Result<Vec<Playlist>> {
        // Upstream exposes no directory endpoint; search covers discovery.
        Err(unsupported_operation(
            "no playlist directory endpoint exists upstream; search covers discovery",
        ))
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
        // Jamendo has no account system on these endpoints: no user identity,
        // no user playlist endpoint.
        Err(unsupported_operation(
            "no account system; upstream exposes no user playlist endpoint",
        ))
    }
}
/// Jamendo's CC catalog exposes no lyrics.
#[async_trait]
impl LyricsProvider for JamendoProvider {
    async fn lyrics(&self, _track: &Track) -> Result<Option<LyricDocument>> {
        Err(unsupported_operation("the CC catalog exposes no lyrics"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::LinkResolver as _;
    use crate::transport::{HttpMethod, MockTransport, TransportResponse};
    use serde_json::json;
    use std::sync::Arc;

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

    /// The nine legacy `Error::Unsupported` sites split by semantics
    /// (AUTH_REGISTRY_DESIGN.md §9.6): the five container faces are
    /// implemented but the endpoints hang anonymously (2026-11-17 live
    /// check) → `Unavailable` (retryable, site text preserved in the detail
    /// slot); the four faces the platform does not offer →
    /// `UnsupportedOperation` (typed provider id + site reason, replacing
    /// the `Unsupported` payload that could not answer `provider_id()`).
    #[tokio::test]
    async fn unsupported_sites_split_into_unavailable_and_unsupported_operation() {
        let provider = JamendoProvider::new();
        // The five container faces → Unavailable.
        let container_errors = [
            provider.album("1").await.unwrap_err(),
            provider.album_tracks("1").await.unwrap_err(),
            provider.playlist("1").await.unwrap_err(),
            provider.playlist_tracks("1").await.unwrap_err(),
            provider
                .resolve_link("https://www.jamendo.com/track/123")
                .await
                .unwrap_err(),
        ];
        for error in &container_errors {
            assert_eq!(error.category(), crate::ErrorCategory::Unavailable);
            assert_eq!(error.provider_id(), Some("jamendo"));
            assert_eq!(
                error.detail(),
                Some("jamendo container endpoints are unavailable anonymously")
            );
            assert_eq!(
                error.to_string(),
                "provider unavailable: jamendo container endpoints are unavailable anonymously"
            );
            assert!(error.is_retryable(), "{error:?}");
        }
        // Unrecognized links keep the parse guard ahead of the container
        // refusal.
        assert!(matches!(
            provider.resolve_link("周杰伦").await,
            Err(Error::InvalidInput(_))
        ));
        // The four faces the platform does not offer → UnsupportedOperation.
        let track = Track {
            id: TrackId::new(ID, "1"),
            title: "t".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: BTreeMap::new(),
        };
        let sites = [
            (
                provider.categories().await.unwrap_err(),
                "no playlist category endpoint exists upstream",
            ),
            (
                provider.playlists("cat", 1, 10).await.unwrap_err(),
                "no playlist directory endpoint exists upstream; search covers discovery",
            ),
            (
                provider.user_playlists(1, 10).await.unwrap_err(),
                "no account system; upstream exposes no user playlist endpoint",
            ),
            (
                provider.lyrics(&track).await.unwrap_err(),
                "the CC catalog exposes no lyrics",
            ),
        ];
        for (error, detail) in &sites {
            assert_eq!(error.category(), crate::ErrorCategory::UnsupportedOperation);
            assert_eq!(error.provider_id(), Some("jamendo"));
            assert_eq!(error.detail(), Some(*detail));
            assert_eq!(
                error.to_string(),
                format!("unsupported operation: {detail}")
            );
            assert!(!error.is_retryable(), "{error:?}");
        }
    }

    // ── fixture-driven tests (see tests/fixtures/jamendo/README.md) ──────

    /// Loads a reconstructed vector/response fixture from tests/fixtures/jamendo
    /// (see the README there for the derivation of these shapes and vectors).
    macro_rules! fixture {
        ($name:expr) => {
            serde_json::from_str::<Value>(include_str!(concat!(
                "../tests/fixtures/jamendo/",
                $name
            )))
            .expect("jamendo fixture must be valid JSON")
        };
    }

    #[test]
    fn x_jam_call_goldens_and_fixed_headers_match_the_audit() {
        let fixture = fixture!("x_jam_call.json");
        let constants = &fixture["source_constants"];
        // The constants double as a pin on the source literals (4gvfvv has no
        // documented origin anywhere upstream; it is pinned, not explained).
        assert_eq!(BASE, constants["base"].as_str().unwrap());
        assert_eq!(REFERER, constants["referer"].as_str().unwrap());
        assert_eq!(X_JAM_VERSION, constants["x_jam_version"].as_str().unwrap());
        assert_eq!(UA_WEB, constants["user_agent"].as_str().unwrap());
        for case in fixture["cases"].as_array().unwrap() {
            let path = case["path"].as_str().unwrap();
            let nonce = case["nonce"].as_str().unwrap();
            let header = x_jam_call(path, nonce);
            assert_eq!(header, case["expected"].as_str().unwrap());
            // Shape: the echoed nonce must byte-match the hashed input.
            assert!(header.starts_with('$'));
            assert!(header.ends_with('~'));
            assert!(header.contains(&format!("*{nonce}~")));
        }
    }

    #[test]
    fn pick_stream_goldens_cover_tiers_and_the_unknown_key_trap() {
        let fixture = fixture!("pick_stream.json");
        for case in fixture["cases"].as_array().unwrap() {
            let item = &case["item"];
            let note = case["note"].as_str().unwrap_or("");
            let expected = &case["expected"];
            match (expected.is_null(), pick_stream(item)) {
                (true, None) => {}
                (false, Some((url, format, quality))) => {
                    if let Some(expected_url) = expected["url"].as_str() {
                        assert_eq!(url, expected_url, "{note}");
                    }
                    if let Some(prefix) = expected["url_prefix"].as_str() {
                        assert!(url.starts_with(prefix), "{note}: {url}");
                    }
                    if let Some(not_prefix) = expected["not_url_prefix"].as_str() {
                        assert!(!url.starts_with(not_prefix), "{note}: {url}");
                    }
                    assert_eq!(
                        format!("{format:?}"),
                        expected["format"].as_str().unwrap(),
                        "{note}"
                    );
                    assert_eq!(
                        format!("{quality:?}"),
                        expected["quality"].as_str().unwrap(),
                        "{note}"
                    );
                }
                (false, None) => panic!("unexpected None: {note}"),
                (true, Some(picked)) => panic!("unexpected pick {picked:?}: {note}"),
            }
        }
    }

    #[test]
    fn track_fixture_pins_search_time_stream_embedding_and_harmless_fields() {
        let fixture = fixture!("track_parse.json");
        let track = jam_parse_track(&fixture["item"]).expect("golden item must parse");
        let expected = &fixture["expected"];
        // The numeric id becomes the string id and the track_id extra.
        assert_eq!(track.id.provider, ID);
        assert_eq!(track.id.id, expected["track_id"]);
        assert_eq!(
            track.extra.get("track_id").map(String::as_str),
            Some(expected["track_id"].as_str().unwrap())
        );
        assert_eq!(track.title, expected["title"]);
        assert_eq!(
            track.artists,
            expected["artists"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(track.album.as_deref(), expected["album"].as_str());
        assert_eq!(track.album_id.as_deref(), expected["album_id"].as_str());
        assert_eq!(
            track.duration.map(|d| d.as_secs()),
            expected["duration_secs"].as_u64()
        );
        assert_eq!(track.artwork_url.as_deref(), expected["artwork"].as_str());
        let qualities = track
            .available_qualities
            .iter()
            .map(|quality| format!("{quality:?}"))
            .collect::<Vec<_>>();
        let expected_qualities = expected["available_qualities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|quality| quality.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(qualities, expected_qualities);
        // The search-time embedded stream: URL shape only, never the from=
        // token bytes (their validity is unknown, see the fixture README).
        let stream_url = track.extra.get("stream_url").map(String::as_str).unwrap();
        assert!(stream_url.starts_with(expected["stream_url_prefix"].as_str().unwrap()));
        assert_eq!(
            track.extra.get("stream_format").map(String::as_str),
            Some(expected["stream_format"].as_str().unwrap())
        );
        assert_eq!(
            track.extra.get("stream_quality").map(String::as_str),
            Some(expected["stream_quality"].as_str().unwrap())
        );
        for case in fixture["filtered"].as_array().unwrap() {
            assert!(
                jam_parse_track(&case["item"]).is_none(),
                "{}",
                case["note"].as_str().unwrap_or("")
            );
        }
        let fallback = &fixture["cover_fallback"];
        let track = jam_parse_track(&fallback["item"]).unwrap();
        assert_eq!(
            track.artwork_url.as_deref(),
            fallback["expected_artwork"].as_str()
        );
    }

    #[test]
    fn search_fixture_pins_the_bare_array_envelope_and_fallback_tier() {
        let fixture = fixture!("search_response.json");
        // /api/search answers with a plain array, not a {data: [...]} wrapper.
        let items = fixture.as_array().expect("search envelope is a bare array");
        assert_eq!(items.len(), 4);
        let tracks = items.iter().filter_map(jam_parse_track).collect::<Vec<_>>();
        assert_eq!(
            tracks.len(),
            2,
            "the id-0 and empty-name items are filtered"
        );
        assert_eq!(
            tracks.iter().map(|t| t.id.id.as_str()).collect::<Vec<_>>(),
            ["1538710", "741555"]
        );
        // Track 1 embeds the download-group /mp32/ URL, not the stream-group
        // format=mp31 URL (192k VBR vs 96k measured, 2026-11-18).
        let url = tracks[0]
            .extra
            .get("stream_url")
            .map(String::as_str)
            .unwrap();
        assert!(url.starts_with("https://prod-1.storage.jamendo.com/download/track/1538710/mp32/"));
        assert!(!url.contains("format=mp31"));
        // Track 2 has an empty download dict, so the stream group's mp31 URL
        // is embedded instead.
        let url = tracks[1]
            .extra
            .get("stream_url")
            .map(String::as_str)
            .unwrap();
        assert!(url.contains("trackid=741555"));
        assert!(url.contains("format=mp31"));
        assert_eq!(
            tracks[1].extra.get("stream_quality").map(String::as_str),
            Some("Low")
        );
    }

    #[tokio::test]
    async fn resolve_serves_the_search_embedded_url_without_network() {
        let fixture = fixture!("search_response.json");
        let track = jam_parse_track(&fixture.as_array().unwrap()[0]).unwrap();
        let provider = JamendoProvider::new();
        // resolve() is zero-network: the URL was embedded at search time.
        let info = provider
            .resolve(&track, &ResolveOptions::default())
            .await
            .unwrap();
        assert_eq!(
            info.url,
            "https://prod-1.storage.jamendo.com/download/track/1538710/mp32/"
        );
        assert_eq!(info.format, AudioFormat::Mp3);
        assert_eq!(info.quality, StreamQuality::Low);
        assert_eq!(info.duration.map(|d| d.as_secs()), Some(245));
        assert!(info.headers.is_empty());
        // A track whose search item had no usable group resolves to an honest
        // error, not a fabricated URL.
        let bare = jam_parse_track(&json!({"id": 55, "name": "No groups"})).unwrap();
        assert!(
            provider
                .resolve(&bare, &ResolveOptions::default())
                .await
                .is_err()
        );
        // Foreign-provider tracks are rejected before anything else.
        let mut foreign = track.clone();
        foreign.id.provider = "other".into();
        assert!(matches!(
            provider.resolve(&foreign, &ResolveOptions::default()).await,
            Err(Error::InvalidInput(_))
        ));
    }

    // ── unified transport migration tests (mock transport, no network):
    // the provider runs against Http::from_shared_mock and the recorded
    // request log pins the migrated request face (TRANSPORT_DESIGN.md §7.6).

    fn mock_json(status: u16, body: &str) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: body.as_bytes().to_vec(),
        }
    }

    /// The recorded `x-jam-call` value must still be `$sha1(path + nonce)*
    /// nonce~` with a fresh in-(0.1,1.0) nonce: the signature stays in the
    /// provider, so the nonce is regenerated per request and the digest must
    /// verify against the nonce echoed in the header itself.
    fn assert_x_jam_call_shape(header: &str, path: &str) {
        assert!(
            header.starts_with('$') && header.ends_with('~'),
            "shape must be $sha1*nonce~: {header}"
        );
        let inner = &header[1..header.len() - 1];
        let (digest, nonce) = inner.split_once('*').expect("digest*nonce");
        let value = nonce.parse::<f64>().expect("nonce must be a float");
        assert!((0.1..1.0).contains(&value), "nonce in (0.1, 1.0): {value}");
        let expected = hex::encode(Sha1::digest(format!("{path}{nonce}").as_bytes()));
        assert_eq!(digest, expected, "signature must be sha1(path+nonce)");
    }

    #[tokio::test]
    async fn api_get_runs_through_send_raw_with_signed_headers() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert_eq!(request.method, HttpMethod::Get);
            assert_eq!(
                request.url,
                "https://www.jamendo.com/api/search?query=love&type=track&limit=20&identities=www"
            );
            assert!(request.idempotent, "GET defaults to idempotent");
            for (name, value) in [
                ("user-agent", UA_WEB),
                ("referer", REFERER),
                ("x-jam-version", X_JAM_VERSION),
                ("x-requested-with", "XMLHttpRequest"),
            ] {
                assert!(
                    request
                        .headers
                        .iter()
                        .any(|(key, sent)| key == name && sent == value),
                    "header {name} must ride the request"
                );
            }
            let call = request
                .headers
                .iter()
                .find(|(key, _)| key == "x-jam-call")
                .map(|(_, value)| value.as_str())
                .expect("x-jam-call must ride the request");
            assert_x_jam_call_shape(call, "/api/search");
            Ok(mock_json(200, r#"[{"id":1,"name":"T"}]"#))
        }));
        let provider = JamendoProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let value = provider
            .api_get(
                "/api/search",
                &[
                    ("query", "love"),
                    ("type", "track"),
                    ("limit", "20"),
                    ("identities", "www"),
                ],
            )
            .await
            .unwrap();
        assert_eq!(value[0]["id"], 1);
        assert_eq!(mock.recorded().len(), 1, "success path is a single send");
    }

    #[tokio::test]
    async fn api_get_retries_transport_errors_and_5xx_then_succeeds() {
        // Declared upgrade (TRANSPORT_DESIGN.md §5.5 row 3): today's
        // single-shot face retries a transport error and a 503 under the
        // unified ladder and lands the third send.
        let mock = Arc::new(MockTransport::script([
            Err(TransportError::Transport("conn reset".to_owned())),
            Ok(mock_json(503, "")),
            Ok(mock_json(200, r#"[{"id":2,"name":"T"}]"#)),
        ]));
        let provider = JamendoProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let value = provider
            .api_get("/api/search", &[("query", "x")])
            .await
            .unwrap();
        assert_eq!(value[0]["id"], 2);
        let recorded = mock.recorded();
        assert_eq!(
            recorded.len(),
            3,
            "transport error + 503 retried, third send succeeds"
        );
        assert!(recorded.iter().all(|request| request.idempotent));
    }

    #[tokio::test]
    async fn api_get_fatal_status_keeps_today_error_text() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(404, "gone"))]));
        let provider = JamendoProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider
            .api_get("/api/search", &[("query", "x")])
            .await
            .expect_err("404 must stay terminal");
        assert_eq!(
            error.to_string(),
            "provider jamendo failed: api request: HTTP 404",
            "the classify_http Fatal text keeps the pre-migration shape"
        );
        assert_eq!(mock.recorded().len(), 1, "404 never retries");
    }

    #[tokio::test]
    async fn api_get_parse_failure_keeps_label_text() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(200, "not json"))]));
        let provider = JamendoProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider
            .api_get("/api/search", &[("query", "x")])
            .await
            .expect_err("malformed body must stay terminal");
        assert!(
            error
                .to_string()
                .starts_with("provider jamendo failed: api response parse failed: "),
            "got: {error}"
        );
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn search_goes_through_the_migrated_api_get() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"[{"id":100,"name":"Track","artist":{"name":"Artist"},
                 "download":{"mp33":"https://cdn/a.mp3"}}]"#,
        ))]));
        let provider = JamendoProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let page = provider
            .search(&SearchQuery::new("love"))
            .await
            .expect("search must parse the bare-array envelope");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id.id, "100");
        assert_eq!(page.items[0].artists, vec!["Artist"]);
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0]
                .url
                .starts_with("https://www.jamendo.com/api/search?query=love"),
            "the search query must reach the migrated face: {}",
            recorded[0].url
        );
    }
}
