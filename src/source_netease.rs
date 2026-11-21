//! NetEase Cloud Music provider migrated from VoiceFox's `lx-source::wy`.
//! Search, stream resolution and lyrics are implemented; QR login and collections remain pending.
use crate::{
    Error, ErrorDetail, Result,
    lrc::parse_lrc,
    models::{
        Album, Artist, AudioFormat, Chart, ChartEntry, ChecksumAlgorithm, LyricDocument,
        LyricFormat, Playlist, PlaylistCategory, StreamInfo, StreamQuality, Track, TrackId,
    },
    provider::{
        AlbumProvider, ArtistProvider, AuthState, AuthenticationProvider, ChartProvider,
        CollectionProvider, LinkResolver, LoginStatus, LyricsProvider, MusicProvider,
        PlaylistProvider, QrAuthenticationProvider, QrLoginKind, QrLoginPoll, QrLoginSession,
        QrLoginStatus, ResolveOptions, ResolvedLink, SearchPage, SearchQuery, StreamResolver,
        TrackSearch,
    },
    retry::NETEASE_CODES,
    session::{ProviderSession, SessionStore},
    transport::{ClientProfile, Http, RetryPolicy, TransportError, TransportRequest},
};
use aes::{
    Aes128,
    cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use async_trait::async_trait;
use md5::{Digest, Md5};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
const ID: &str = "netease";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 Chrome/91.0.4472.164 NeteaseMusicDesktop/3.0.18.203152";
const REFERER: &str = "https://music.163.com/";
const SEARCH_PATH: &str = "/api/search/song/list/page";
const PLAYER_PATH: &str = "/api/song/enhance/player/url/v1";
const LYRIC_PATH: &str = "/api/song/lyric/v1";
#[derive(Clone)]
pub struct NeteaseProvider {
    http: Http,
    sessions: SessionStore,
}
impl std::fmt::Debug for NeteaseProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NeteaseProvider")
            .field("session_store", &"redacted")
            .finish()
    }
}
impl Default for NeteaseProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl NeteaseProvider {
    pub fn new() -> Self {
        // Client construction moved onto the unified transport layer: the
        // profile carries exactly today's knobs (15s total timeout + NetEase
        // UA); the build fallback shape lives in `build_client`
        // (TRANSPORT_DESIGN.md §4.3).
        let http = Http::from_profile(&ClientProfile {
            user_agent: Some(UA.to_owned()),
            ..ClientProfile::provider_default()
        });
        Self {
            http,
            sessions: SessionStore::default(),
        }
    }

    /// Test seam: inject a transport handle (a mock in unit tests). The
    /// production constructor is [`NeteaseProvider::new`], whose profile
    /// carries the client-level NetEase UA. Test-only so non-test builds see
    /// no dead constructor.
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
    /// Derive the unified [`AuthState`] from the stored session (no network
    /// I/O). The `login_expired` token written by
    /// [`Self::mark_session_expired`] is the Expired bit; a session holding
    /// the login cookie (`MUSIC_U`) is `Authenticated`; anything else is
    /// `LoggedOut`. This is the AuthState wiring for the M2 auth state
    /// machine (AUTH_REGISTRY_DESIGN.md §2, §8 slice 1): `login_status`
    /// reports through this state instead of hand-classifying the session.
    fn auth_state(&self) -> AuthState {
        match self.sessions.get(ID) {
            None => AuthState::LoggedOut,
            Some(session) if !session.cookies.contains_key("MUSIC_U") => AuthState::LoggedOut,
            Some(session) if session.tokens.contains_key("login_expired") => AuthState::Expired,
            Some(_) => AuthState::Authenticated,
        }
    }
    /// Register all currently implemented NetEase capabilities in one step.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_lyrics(provider.clone());
        registry.register_authentication(provider.clone());
        registry.register_qr_authentication(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_playlist(provider.clone());
        registry.register_album(provider.clone());
        registry.register_artist(provider.clone());
        registry.register_chart(provider.clone());
        registry.register_link_resolver(provider);
    }
    pub fn set_cookies(
        &self,
        cookies: BTreeMap<String, String>,
        user_id: Option<String>,
        display_name: Option<String>,
    ) -> Result<()> {
        self.sessions.set(
            ID,
            ProviderSession {
                cookies,
                tokens: BTreeMap::new(),
                user_id,
                display_name,
                ..Default::default()
            },
        )
    }
    /// Flag the current session as expired so `login_status` reports it, and
    /// return the classified [`Error::SessionExpired`] for the caller to
    /// propagate — the M2 landing spot for the `retry` classifier's
    /// `expired=[301,512]` verdict (AUTH_REGISTRY_DESIGN.md §1.1, §8 slice 1).
    /// The legacy free-form "login required or session expired" string is
    /// replaced by the variant, whose Display body is that exact text;
    /// `detail` carries the call-site label, the only underlying information
    /// [`TransportError::SessionExpired`] preserves.
    fn mark_session_expired(&self, detail: Option<ErrorDetail>) -> Error {
        if let Some(mut session) = self.sessions.get(ID) {
            session.tokens.insert("login_expired".into(), "1".into());
            let _ = self.sessions.set(ID, session);
        }
        Error::SessionExpired {
            provider: Some(ID.into()),
            detail,
        }
    }
    /// GET one JSON API endpoint through the unified transport layer
    /// (`send_api_json`): transport errors, 429/5xx and the throttled 405 code
    /// all retry under `RetryPolicy::standard()` with the engine's exponential
    /// backoff + jitter + `Retry-After`, replacing the fixed
    /// `[600,1200,2400,4000]` table. The send budget is unchanged (at most 5
    /// sends); the backoff shape change is declared in TRANSPORT_DESIGN.md
    /// §5.5 row 1. Classification stays in `retry` (`classify_http` /
    /// `classify_api`); session-expiry and fatal body codes surface as the
    /// classified M2 errors (`api_code_error`, the label rides in the detail
    /// slot) while the remaining transport texts keep today's
    /// `{label}: {error}` shape.
    async fn get_json(&self, url: &str, label: &str) -> Result<Value> {
        let request = self.request(TransportRequest::get(url));
        match self
            .http
            .send_api_json(
                &request,
                &RetryPolicy::standard(),
                &NETEASE_CODES,
                netease_code_of,
            )
            .await
        {
            Ok(value) => Ok(value),
            Err(TransportError::SessionExpired) => {
                Err(self.mark_session_expired(Some(ErrorDetail::new(label))))
            }
            Err(TransportError::Api(text)) => Err(api_code_error(Some(label), &text)),
            Err(error) => {
                tracing::debug!("NetEase {label} failed: {error}");
                Err(perr(format!("{label}: {error}")))
            }
        }
    }
    async fn track_details(&self, ids: &[i64]) -> Result<Vec<Track>> {
        let mut result = Vec::with_capacity(ids.len());
        for batch in ids.chunks(100) {
            let body = serde_json::to_string(
                &batch.iter().map(|id| json!({"id":id})).collect::<Vec<_>>(),
            )?;
            let url = format!(
                "https://music.163.com/api/v3/song/detail?c={}",
                urlencoding::encode(&body)
            );
            let value = self.get_json(&url, "song details").await?;
            result.extend(
                value["songs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(parse_track),
            );
        }
        Ok(result)
    }
    /// Adds the request-level headers (UA, Referer) and the whitelisted
    /// session cookies to a [`TransportRequest`]. Platform-specific and kept
    /// in the provider: the cookie whitelist
    /// (MUSIC_U/MUSIC_A/__csrf/NMTID/JSESSIONID-WYYY) is netease policy
    /// (TRANSPORT_DESIGN.md §1); the client-level UA comes from the profile.
    fn request(&self, request: TransportRequest) -> TransportRequest {
        let mut request = request.header("user-agent", UA).header("referer", REFERER);
        if let Some(session) = self.sessions.get(ID) {
            let cookie = session
                .cookies
                .iter()
                .filter(|(k, _)| {
                    matches!(
                        k.as_str(),
                        "MUSIC_U" | "MUSIC_A" | "__csrf" | "NMTID" | "JSESSIONID-WYYY"
                    )
                })
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            if !cookie.is_empty() {
                request = request.header("cookie", cookie);
            }
        }
        request
    }
    /// EAPI POST through the unified transport layer. The encrypted body is
    /// resent on retries, declared with `.idempotent(true)` per
    /// TRANSPORT_DESIGN.md §5.2 so the engine's gate allows it at all three
    /// resend sites. Today's fixed `[400,900,1800]` table allowed 4 sends;
    /// `standard()` allows 5 (§5.5 row 2) with exponential backoff + jitter +
    /// `Retry-After`. eapi errors carry no label prefix today; that exact
    /// shape is preserved — session expiry and fatal body codes surface as
    /// the classified M2 errors without a label, the rest keeps the bare
    /// transport text.
    async fn eapi(&self, endpoint: &str, path: &str, payload: &Value) -> Result<Value> {
        let body = eapi_encrypt(path, payload)?;
        let request = self
            .request(TransportRequest::post(endpoint).idempotent(true))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("params={body}"));
        match self
            .http
            .send_api_json(
                &request,
                &RetryPolicy::standard(),
                &NETEASE_CODES,
                netease_code_of,
            )
            .await
        {
            Ok(value) => Ok(value),
            Err(TransportError::SessionExpired) => Err(self.mark_session_expired(None)),
            Err(TransportError::Api(text)) => Err(api_code_error(None, &text)),
            Err(error) => {
                tracing::debug!("NetEase eapi {path} failed: {error}");
                Err(perr(error.to_string()))
            }
        }
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}
/// Recover the platform API code from the shared classifier's fatal text.
/// `retry::classify_api` formats fatal outcomes as `"API code {code}: ..."`
/// (`retry.rs:100-103`) and the transport layer carries only that text
/// ([`TransportError::Api`]), so the typed `code` slots of the M2 variants
/// are fed by parsing this pinned shape. Texts without the prefix (parse
/// failures, transport notes) yield `None` and keep the legacy catch-all.
fn fatal_api_code(text: &str) -> Option<i64> {
    text.strip_prefix("API code ")?
        .split_once(':')?
        .0
        .trim()
        .parse()
        .ok()
}
/// Classify a fatal body-code text into the M2 taxonomy (AUTH_REGISTRY_DESIGN.md
/// §1.1 netease row, §8 slice 1): the throttled code (`NETEASE_CODES.throttled`,
/// 405) becomes [`Error::RateLimited`] — the `retry_after` slot stays `None`
/// because the 405 body code carries no server-sent duration; any other
/// recognizable platform code is an upstream contract violation
/// ([`Error::InvalidResponse`] with the code preserved); unrecognizable texts
/// keep the legacy [`Error::Provider`] catch-all with the exact pre-migration
/// `{label}: {text}` shape. `label` is `Some` for the GET loop (`get_json`)
/// and `None` for eapi, matching today's no-label eapi errors.
fn api_code_error(label: Option<&str>, text: &str) -> Error {
    let code = fatal_api_code(text);
    let detail_text = match label {
        Some(label) => format!("{label}: {text}"),
        None => text.to_owned(),
    };
    if code == NETEASE_CODES.throttled {
        Error::RateLimited {
            provider: Some(ID.into()),
            retry_after: None,
            detail: Some(ErrorDetail::new(detail_text)),
        }
    } else if let Some(code) = code {
        Error::InvalidResponse {
            provider: Some(ID.into()),
            code: Some(code),
            detail: Some(ErrorDetail::new(detail_text)),
        }
    } else {
        perr(detail_text)
    }
}
/// `(code, message)` extractor handed to `Http::send_api_json`; extraction
/// stays provider-side, classification lives in `retry` (TRANSPORT_DESIGN.md
/// §4.1). A body without a code field falls back to the OK code, exactly like
/// the previous `unwrap_or(NETEASE_CODES.ok)` loops (review P-7: falling back
/// to `0` instead would flip missing-code bodies into `Fatal("API code 0")`).
fn netease_code_of(value: &Value) -> (i64, String) {
    (
        value["code"].as_i64().unwrap_or(NETEASE_CODES.ok),
        value["message"]
            .as_str()
            .unwrap_or("unknown error")
            .to_owned(),
    )
}
fn eapi_encrypt(path: &str, payload: &Value) -> Result<String> {
    let text = serde_json::to_string(payload)?;
    let digest = format!(
        "{:x}",
        Md5::digest(format!("nobody{path}use{text}md5forencrypt").as_bytes())
    );
    let input = format!("{path}-36cd479b6b5-{text}-36cd479b6b5-{digest}");
    let pad = 16 - input.len() % 16;
    let mut bytes = input.into_bytes();
    bytes.extend(std::iter::repeat_n(pad as u8, pad));
    let cipher = Aes128::new(GenericArray::from_slice(b"e82ckenh8dichen8"));
    let mut encrypted = Vec::with_capacity(bytes.len());
    for block in bytes.as_chunks::<16>().0 {
        let mut block = GenericArray::clone_from_slice(block);
        cipher.encrypt_block(&mut block);
        encrypted.extend_from_slice(&block);
    }
    Ok(hex::encode_upper(encrypted))
}
fn parse_track(item: &Value) -> Option<Track> {
    let item = item
        .pointer("/baseInfo/simpleSongData")
        .or_else(|| item.get("content"))
        .or_else(|| item.get("item"))
        .unwrap_or(item);
    let id = item["id"]
        .as_i64()
        .or_else(|| item["id"].as_str().and_then(|s| s.parse().ok()))?;
    if id <= 0 {
        return None;
    }
    let title = item["name"].as_str().unwrap_or_default().to_owned();
    let artists = item["ar"]
        .as_array()
        .or_else(|| item["artists"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|a| a["name"].as_str().map(str::to_owned))
        .collect();
    // v3 shapes use "al"/"dt"; the legacy song detail API uses "album"/"duration".
    let album = item["al"]["name"]
        .as_str()
        .or_else(|| item["album"]["name"].as_str())
        .map(str::to_owned);
    let album_id = item["al"]["id"]
        .as_i64()
        .or_else(|| item["album"]["id"].as_i64())
        .map(|v| v.to_string());
    let artwork_url = item["al"]["picUrl"]
        .as_str()
        .or_else(|| item["album"]["picUrl"].as_str())
        .map(str::to_owned);
    let duration = item["dt"]
        .as_u64()
        .or_else(|| item["duration"].as_u64())
        .map(Duration::from_millis);
    let mut qualities = vec![StreamQuality::Low];
    if item["h"]["size"].as_u64().unwrap_or(0) > 0 {
        qualities.push(StreamQuality::High);
    }
    if item["sq"]["size"].as_u64().unwrap_or(0) > 0 {
        qualities.push(StreamQuality::Lossless);
    }
    if item["hr"]["size"].as_u64().unwrap_or(0) > 0 || item["privilege"]["maxBrLevel"] == "hires" {
        qualities.push(StreamQuality::HiRes);
    }
    let mut extra = BTreeMap::new();
    if let Some(level) = item["privilege"]["plLevel"].as_str() {
        extra.insert("pl_level".into(), level.into());
    }
    Some(Track {
        id: TrackId::new(ID, id.to_string()),
        title,
        artists,
        album,
        album_id,
        duration,
        artwork_url,
        isrc: None,
        available_qualities: qualities,
        audio: None,
        extra,
    })
}
fn ladder(q: StreamQuality) -> &'static [(&'static str, StreamQuality, &'static str)] {
    match q {
        StreamQuality::HiRes => &[
            ("hires", StreamQuality::HiRes, "flac"),
            ("lossless", StreamQuality::Lossless, "flac"),
            ("exhigh", StreamQuality::High, "mp3"),
            ("standard", StreamQuality::Low, "mp3"),
        ],
        StreamQuality::Lossless => &[
            ("lossless", StreamQuality::Lossless, "flac"),
            ("exhigh", StreamQuality::High, "mp3"),
            ("standard", StreamQuality::Low, "mp3"),
        ],
        StreamQuality::High => &[
            ("exhigh", StreamQuality::High, "mp3"),
            ("standard", StreamQuality::Low, "mp3"),
        ],
        _ => &[("standard", StreamQuality::Low, "mp3")],
    }
}
impl MusicProvider for NeteaseProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        "网易云音乐"
    }
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            lyrics: true,
            authentication: true,
            playlists: true,
            albums: true,
            artists: true,
            charts: true,
            link_resolve: true,
            ..Default::default()
        }
    }
}
#[async_trait]
impl TrackSearch for NeteaseProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.keyword.trim().is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        let page = q.page.max(1);
        let limit = q.page_size.clamp(1, 100);
        let mut payload = json!({"keyword":q.keyword,"needCorrect":"1","channel":"typing","offset":limit.saturating_mul(page-1),"scene":"normal","limit":limit});
        if page == 1 {
            payload["total"] = Value::Bool(true);
        }
        let json = self
            .eapi(
                "https://interface.music.163.com/eapi/batch",
                SEARCH_PATH,
                &payload,
            )
            .await?;
        let result = json.get("data").or_else(|| json.get("result"));
        let total = result.and_then(|v| v["totalCount"].as_u64());
        let items = result
            .and_then(|v| v["resources"].as_array().or_else(|| v["songs"].as_array()))
            .into_iter()
            .flatten()
            .filter_map(parse_track)
            .collect::<Vec<_>>();
        let has_more = total.is_some_and(|t| u64::from(page) * u64::from(limit) < t);
        Ok(SearchPage {
            items,
            total,
            has_more,
        })
    }
}
#[async_trait]
impl StreamResolver for NeteaseProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to NetEase".into(),
            ));
        }
        let mut last = None;
        for (level, quality, format) in ladder(options.quality) {
            let payload =
                json!({"ids":format!("[{}]",track.id.id),"level":level,"encodeType":format});
            match self
                .eapi(
                    "https://interface3.music.163.com/eapi/song/enhance/player/url/v1",
                    PLAYER_PATH,
                    &payload,
                )
                .await
            {
                Ok(value) => {
                    if let Some(item) = value["data"].as_array().and_then(|v| v.first())
                        && item["freeTrialInfo"].is_null()
                        && let Some(url) = item["url"].as_str().filter(|s| !s.trim().is_empty())
                    {
                        return Ok(StreamInfo {
                            url: url.into(),
                            quality: *quality,
                            format: if *format == "flac" {
                                AudioFormat::Flac
                            } else {
                                AudioFormat::Mp3
                            },
                            duration: track.duration,
                            expires_at_unix: url_expiry(url),
                            content_length: item["size"].as_u64(),
                            checksum: item["md5"].as_str().map(str::to_ascii_lowercase),
                            headers: vec![],
                            fallback_urls: vec![],
                            index_range: None,
                            init_range: None,
                            max_chunk_size: None,
                            size_is_advisory: false,
                            // The endpoint names the digest `md5`; name the
                            // algorithm explicitly instead of leaning on the
                            // `None` = MD5 default (MODEL_ALIGNMENT_DESIGN.md
                            // §2.3). The two reads share one `as_str()`, so
                            // `Some`/`None` can never disagree with
                            // `checksum`.
                            checksum_algorithm: item["md5"]
                                .as_str()
                                .map(|_| ChecksumAlgorithm::Md5),
                            fallback_endpoints: vec![],
                        });
                    }
                }
                Err(e) => last = Some(e),
            }
        }
        let fallback = format!(
            "https://music.163.com/song/media/outer/url?id={}.mp3",
            track.id.id
        );
        // Dead-fallback HEAD probe: exactly one attempt with its own 3s
        // per-request timeout, through `send_once`. `send_raw` would retry it
        // up to four times and stretch 3s to the 15s profile timeout on an
        // already-failed resolve (TRANSPORT_DESIGN.md §7.1, review P-3). Like
        // today, the probe adds no request-level headers (no cookie/referer);
        // the client-level UA still applies through the profile.
        let response = self
            .http
            .send_once(&TransportRequest::head(&fallback).timeout(Duration::from_secs(3)))
            .await
            .map_err(|e| {
                perr(format!(
                    "{}; fallback probe failed: {e}",
                    last.map(|e: Error| e.to_string())
                        .unwrap_or_else(|| "no playable quality".into())
                ))
            })?;
        let kind = response
            .header("content-type")
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("");
        if !response.is_success()
            || !(kind.starts_with("audio/") || kind == "application/octet-stream")
        {
            return Err(perr("no playable stream URL returned by NetEase"));
        }
        Ok(StreamInfo {
            url: fallback,
            quality: StreamQuality::Low,
            format: AudioFormat::Mp3,
            duration: track.duration,
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
#[async_trait]
impl LyricsProvider for NeteaseProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to NetEase".into(),
            ));
        }
        let payload =
            json!({"id":track.id.id,"cp":false,"tv":0,"lv":0,"rv":0,"kv":0,"yv":0,"ytv":0,"yrv":0});
        let value = self
            .eapi(
                "https://interface3.music.163.com/eapi/song/lyric/v1",
                LYRIC_PATH,
                &payload,
            )
            .await?;
        Ok(parse_lyric_document(&value))
    }
}
#[async_trait]
impl CollectionProvider for NeteaseProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist> {
        let id = numeric_id(id)?;
        let value = self
            .get_json(
                &format!("https://music.163.com/api/v3/playlist/detail?id={id}&n=0&s=0"),
                "playlist detail",
            )
            .await?;
        parse_playlist(&value["playlist"]).ok_or(Error::NotFound)
    }
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let id = numeric_id(id)?;
        let value = self
            .get_json(
                &format!("https://music.163.com/api/v3/playlist/detail?id={id}&n=0&s=0"),
                "playlist detail",
            )
            .await?;
        let playlist = &value["playlist"];
        let ids = parse_track_ids(&playlist["trackIds"]);
        if ids.is_empty() {
            return Ok(playlist_tracks_fallback(playlist));
        }
        self.track_details(&ids).await
    }
}
#[async_trait]
impl PlaylistProvider for NeteaseProvider {
    async fn categories(&self) -> Result<Vec<PlaylistCategory>> {
        let value = self
            .get_json(
                "https://music.163.com/api/playlist/catalogue",
                "playlist categories",
            )
            .await?;
        Ok(parse_categories(&value))
    }
    async fn playlists(&self, category: &str, page: u32, page_size: u32) -> Result<Vec<Playlist>> {
        let limit = page_size.clamp(1, 100);
        let offset = limit.saturating_mul(page.saturating_sub(1));
        let url = format!(
            "https://music.163.com/api/playlist/list?cat={}&order=hot&limit={limit}&offset={offset}",
            urlencoding::encode(if category.trim().is_empty() {
                "全部"
            } else {
                category.trim()
            })
        );
        let value = self.get_json(&url, "hot playlists").await?;
        Ok(value["playlists"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_playlist)
            .collect())
    }
    async fn search_playlists(&self, keyword: &str, page: u32) -> Result<Vec<Playlist>> {
        if keyword.trim().is_empty() {
            return Err(Error::InvalidInput(
                "playlist search keyword is empty".into(),
            ));
        }
        let offset = 30u32.saturating_mul(page.saturating_sub(1));
        let url = format!(
            "https://music.163.com/api/search/get/web?s={}&type=1000&limit=30&offset={offset}",
            urlencoding::encode(keyword)
        );
        let value = self.get_json(&url, "playlist search").await?;
        Ok(value
            .pointer("/result/playlists")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_playlist)
            .collect())
    }
    async fn user_playlists(&self, page: u32, page_size: u32) -> Result<Vec<Playlist>> {
        let session = self
            .sessions
            .get(ID)
            .ok_or_else(|| perr("please log in first"))?;
        if session.tokens.contains_key("login_expired") {
            return Err(perr("login session expired"));
        }
        let account = self
            .get_json(
                "https://music.163.com/api/nuser/account/get",
                "account profile",
            )
            .await?;
        let uid = account["account"]["id"]
            .as_i64()
            .or_else(|| account["profile"]["userId"].as_i64())
            .filter(|id| *id > 0)
            .ok_or_else(|| perr("cannot determine logged-in user id"))?;
        let limit = page_size.clamp(1, 100);
        let offset = limit.saturating_mul(page.saturating_sub(1));
        let url = format!(
            "https://music.163.com/api/user/playlist?uid={uid}&limit={limit}&offset={offset}&includeVideo=true"
        );
        let value = self.get_json(&url, "user playlists").await?;
        Ok(value["playlist"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_playlist)
            .collect())
    }
}
#[async_trait]
impl AlbumProvider for NeteaseProvider {
    async fn album(&self, id: &str) -> Result<Album> {
        let id = numeric_id(id)?;
        let value = self
            .get_json(
                &format!("https://music.163.com/api/v1/album/{id}"),
                "album detail",
            )
            .await?;
        let mut album = parse_album(&value["album"]).ok_or(Error::NotFound)?;
        album.id = TrackId::new(ID, id.to_string());
        Ok(album)
    }
    async fn album_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let id = numeric_id(id)?;
        let value = self
            .get_json(
                &format!("https://music.163.com/api/v1/album/{id}"),
                "album tracks",
            )
            .await?;
        Ok(value["songs"]
            .as_array()
            .or_else(|| value["album"]["songs"].as_array())
            .into_iter()
            .flatten()
            .filter_map(parse_track)
            .collect())
    }
}
#[async_trait]
impl ArtistProvider for NeteaseProvider {
    async fn artist(&self, id: &str) -> Result<Artist> {
        let id = numeric_id(id)?;
        let value = self
            .get_json(
                &format!("https://music.163.com/api/artist/{id}?limit=1&offset=0"),
                "artist detail",
            )
            .await?;
        let a = &value["artist"];
        let (name, artwork_url) = parse_artist_node(a).ok_or(Error::NotFound)?;
        Ok(Artist {
            id: TrackId::new(ID, id.to_string()),
            name,
            artwork_url,
        })
    }
    async fn artist_top_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let id = numeric_id(id)?;
        let value = self
            .get_json(
                &format!("https://music.163.com/api/artist/{id}?limit=50&offset=0"),
                "artist hot songs",
            )
            .await?;
        Ok(parse_hot_songs(&value))
    }
    async fn artist_albums(&self, id: &str, page: u32, page_size: u32) -> Result<Vec<Album>> {
        let id = numeric_id(id)?;
        let limit = page_size.clamp(1, 100);
        let offset = limit.saturating_mul(page.saturating_sub(1));
        let value = self
            .get_json(
                &format!(
                    "https://music.163.com/api/artist/albums/{id}?limit={limit}&offset={offset}"
                ),
                "artist albums",
            )
            .await?;
        Ok(parse_hot_albums(&value))
    }
}
#[async_trait]
impl ChartProvider for NeteaseProvider {
    async fn charts(&self) -> Result<Vec<Chart>> {
        let value = self
            .get_json("https://music.163.com/api/toplist", "chart catalogue")
            .await?;
        Ok(value["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_chart)
            .collect())
    }
    async fn chart_tracks(&self, id: &str) -> Result<Vec<ChartEntry>> {
        let id = numeric_id(id)?;
        let value = self
            .get_json(
                &format!("https://music.163.com/api/v3/playlist/detail?id={id}&n=0&s=0"),
                "chart tracks",
            )
            .await?;
        let pl = &value["playlist"];
        let ids = parse_track_ids(&pl["trackIds"]);
        let tracks = if ids.is_empty() {
            playlist_tracks_fallback(pl)
        } else {
            self.track_details(&ids).await?
        };
        Ok(chart_entries(tracks))
    }
}
#[async_trait]
impl LinkResolver for NeteaseProvider {
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink> {
        let (kind, id) = parse_link_target(url).ok_or_else(|| {
            Error::InvalidInput("unrecognized NetEase song/playlist/album link".into())
        })?;
        match kind.as_str() {
            "song" => {
                let value = self
                    .get_json(
                        &format!("https://music.163.com/api/song/detail?ids=%5B{id}%5D"),
                        "song link",
                    )
                    .await?;
                let track = value["songs"]
                    .as_array()
                    .and_then(|v| v.first())
                    .and_then(parse_track)
                    .ok_or(Error::NotFound)?;
                Ok(ResolvedLink::Track(track))
            }
            "playlist" => Ok(ResolvedLink::Playlist(self.playlist(&id).await?)),
            "album" => Ok(ResolvedLink::Album(self.album(&id).await?)),
            _ => Err(Error::NotFound),
        }
    }
}
fn numeric_id(id: &str) -> Result<i64> {
    let value = id
        .trim()
        .parse::<i64>()
        .map_err(|_| Error::InvalidInput("resource id must be numeric".into()))?;
    if value <= 0 {
        return Err(Error::InvalidInput("resource id must be positive".into()));
    }
    Ok(value)
}
fn value_string(v: &Value) -> String {
    v.as_str()
        .map(str::to_owned)
        .or_else(|| v.as_i64().map(|n| n.to_string()))
        .unwrap_or_default()
}
fn parse_track_ids(value: &Value) -> Vec<i64> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["id"].as_i64().or_else(|| v.as_i64()))
        .filter(|id| *id > 0)
        .collect()
}
fn parse_playlist(v: &Value) -> Option<Playlist> {
    let id = value_string(&v["id"]);
    let title = v["name"].as_str()?.trim();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(Playlist {
        id: TrackId::new(ID, id),
        title: title.into(),
        description: v["description"].as_str().map(str::to_owned),
        artwork_url: v["coverImgUrl"].as_str().map(str::to_owned),
        track_count: v["trackCount"].as_u64().map(|n| n as u32),
        tracks: vec![],
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
    })
}
fn parse_artist_names(v: &Value) -> Vec<String> {
    match v {
        Value::Array(items) => items
            .iter()
            .filter_map(|a| a["name"].as_str().map(str::to_owned))
            .collect(),
        Value::Object(_) => v["name"].as_str().map(str::to_owned).into_iter().collect(),
        _ => vec![],
    }
}
fn parse_album(v: &Value) -> Option<Album> {
    let id = value_string(&v["id"]);
    let title = v["name"].as_str()?.trim();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    let mut artists = parse_artist_names(&v["artists"]);
    if artists.is_empty() {
        artists = parse_artist_names(&v["artist"]);
    }
    Some(Album {
        id: TrackId::new(ID, id),
        title: title.into(),
        artists,
        artwork_url: v["picUrl"].as_str().map(str::to_owned),
        track_count: v["size"]
            .as_u64()
            .or_else(|| v["songs"].as_array().map(|s| s.len() as u64))
            .map(|n| n as u32),
    })
}
fn parse_chart(v: &Value) -> Option<Chart> {
    let id = value_string(&v["id"]);
    let title = v["name"].as_str()?.trim();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(Chart {
        id: TrackId::new(ID, id),
        title: title.into(),
        description: v["updateFrequency"].as_str().map(str::to_owned),
        updated_description: None,
    })
}
/// Songs embedded in a playlist detail node.
///
/// Favorited playlists only carry roughly the first 20 songs here while the
/// complete list lives in `trackIds` (the server ignores the `n` parameter),
/// so this is a lossy fallback for responses without usable `trackIds`.
fn playlist_tracks_fallback(playlist: &Value) -> Vec<Track> {
    playlist["tracks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_track)
        .collect()
}
/// Artist identity from the `/api/artist/{id}` `artist` node: name plus optional
/// avatar. (musid-dli-specific endpoint; no upstream wy artist parser exists.)
fn parse_artist_node(a: &Value) -> Option<(String, Option<String>)> {
    let name = a["name"].as_str().filter(|s| !s.is_empty())?;
    Some((name.to_owned(), a["picUrl"].as_str().map(str::to_owned)))
}
/// Songs from the `/api/artist/{id}` `hotSongs` array.
fn parse_hot_songs(value: &Value) -> Vec<Track> {
    value["hotSongs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_track)
        .collect()
}
/// Albums from the `/api/artist/albums/{id}` `hotAlbums` array.
fn parse_hot_albums(value: &Value) -> Vec<Album> {
    value["hotAlbums"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_album)
        .collect()
}
/// Ranks tracks 1..N in list order.
fn chart_entries(tracks: Vec<Track>) -> Vec<ChartEntry> {
    tracks
        .into_iter()
        .enumerate()
        .map(|(i, track)| ChartEntry {
            rank: (i + 1) as u32,
            track,
        })
        .collect()
}
fn parse_categories(v: &Value) -> Vec<PlaylistCategory> {
    let groups = v["categories"].as_object();
    let mut result = vec![PlaylistCategory {
        id: "全部".into(),
        name: "全部".into(),
        group: Some("全部".into()),
        count: v["all"]["resourceCount"].as_u64().unwrap_or(0) as u32,
        hot: v["all"]["hot"].as_bool().unwrap_or(true),
        provider: Some(ID.to_owned()),
        extra: Default::default(),
    }];
    for item in v["sub"].as_array().into_iter().flatten() {
        let name = item["name"].as_str().unwrap_or_default().trim();
        if name.is_empty() {
            continue;
        }
        let group = item["category"]
            .as_i64()
            .map(|n| n.to_string())
            .and_then(|key| {
                groups
                    .and_then(|g| g.get(&key))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        result.push(PlaylistCategory {
            id: name.into(),
            name: name.into(),
            group,
            count: item["resourceCount"].as_u64().unwrap_or(0) as u32,
            hot: item["hot"].as_bool().unwrap_or(false),
            provider: Some(ID.to_owned()),
            extra: Default::default(),
        });
    }
    result
}
fn parse_lyric_document(value: &Value) -> Option<LyricDocument> {
    let original = value
        .pointer("/lrc/lyric")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if original.trim().is_empty() {
        return None;
    }
    let translation = value
        .pointer("/tlyric/lyric")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let romanization = value
        .pointer("/romalrc/lyric")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some(LyricDocument {
        format: LyricFormat::Lrc,
        lines: parse_lrc(&original),
        original,
        translation,
        romanization,
    })
}
fn parse_link_target(url: &str) -> Option<(String, String)> {
    let lower = url.to_ascii_lowercase();
    // Only trust music.163.com hosts (plus subdomains) so arbitrary URLs handed
    // to the resolver are rejected before any request is made.
    if let Some(rest) = lower.split_once("://").map(|(_, rest)| rest) {
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = match host.rsplit_once(':') {
            Some((h, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => h,
            _ => host,
        };
        if host != "music.163.com" && !host.ends_with(".music.163.com") {
            return None;
        }
    }
    let fragment = lower
        .split_once('#')
        .map(|(_, f)| f.trim_start_matches('!'))
        .unwrap_or("");
    for candidate in [fragment, lower.as_str()] {
        let Some(kind) = ["song", "playlist", "album"]
            .iter()
            .find(|kind| candidate.contains(&format!("/{kind}")))
        else {
            continue;
        };
        let id = candidate
            .split("id=")
            .nth(1)
            .and_then(|s| s.split(['&', '/', '?']).next())
            .or_else(|| {
                candidate
                    .split(&format!("/{kind}/"))
                    .nth(1)
                    .and_then(|s| s.split(['&', '/', '?']).next())
            });
        if let Some(id) = id.filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit())) {
            return Some(((*kind).into(), id.into()));
        }
    }
    None
}

#[async_trait]
impl QrAuthenticationProvider for NeteaseProvider {
    async fn create_qr_login(&self) -> Result<QrLoginSession> {
        const HOSTS: [&str; 3] = [
            "interface.music.163.com",
            "interface3.music.163.com",
            "music.163.com",
        ];
        let mut last_error = None;
        for host in HOSTS {
            let url = format!("https://{host}/api/login/qrcode/unikey");
            // Host failover, not a time retry: each hop is one `send_once`
            // attempt (TRANSPORT_DESIGN.md §4.1, review P-2); the loop's host
            // rotation stays the only retry, exactly as today.
            let request = self
                .request(TransportRequest::post(&url))
                .header("content-type", "application/x-www-form-urlencoded")
                .body("type=3");
            match self.http.send_once(&request).await {
                Ok(response) => match response.json::<Value>() {
                    Ok(value) if value["code"].as_i64() == Some(200) => {
                        let key = value["unikey"]
                            .as_str()
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .ok_or_else(|| perr("QR login key missing"))?;
                        return Ok(QrLoginSession {
                            key: key.into(),
                            url: format!("https://music.163.com/login?codekey={key}"),
                            expires_in: 300,
                            // The unikey API returns only this H5 login link
                            // (the QR content the host renders) — no image
                            // bytes are served, so image_png stays None per
                            // the actual return (M3 QR provider slice,
                            // MODEL_ALIGNMENT_DESIGN.md §6).
                            image_png: None,
                            kind: QrLoginKind::Standard,
                        });
                    }
                    Ok(value) => {
                        last_error = Some(perr(format!(
                            "QR key API: {}",
                            value["message"].as_str().unwrap_or("unexpected response")
                        )))
                    }
                    Err(error) => last_error = Some(perr(error.to_string())),
                },
                Err(error) => last_error = Some(perr(error.to_string())),
            }
        }
        Err(last_error.unwrap_or_else(|| perr("all QR login hosts failed")))
    }
    async fn poll_qr_login(&self, key: &str) -> Result<QrLoginPoll> {
        let key = key.trim();
        if key.is_empty() {
            return Err(Error::InvalidInput("QR login key is empty".into()));
        }
        const HOSTS: [&str; 3] = [
            "interface.music.163.com",
            "interface3.music.163.com",
            "music.163.com",
        ];
        let mut last_error = None;
        for host in HOSTS {
            let url = format!("https://{host}/api/login/qrcode/client/login");
            // Host failover, not a time retry: one `send_once` attempt per
            // host (TRANSPORT_DESIGN.md §4.1, review P-2), rotation unchanged.
            let request = self
                .request(TransportRequest::post(&url))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(format!("key={}&type=3", urlencoding::encode(key)));
            match self.http.send_once(&request).await {
                Ok(response) => {
                    // Lossy-mangled set-cookie values are already skipped by
                    // `TransportResponse::all` (today's `to_str()` drop
                    // semantics, §5.5 row 9).
                    let set_cookies = response.all("set-cookie");
                    match response.json::<Value>() {
                        Ok(value) => {
                            let code = value["code"].as_i64().unwrap_or_default();
                            let message = value["message"].as_str().unwrap_or_default().to_owned();
                            let status = qr_status(code, &message);
                            let mut user_id = None;
                            let mut display_name = None;
                            if status == QrLoginStatus::Success {
                                let mut cookies = BTreeMap::new();
                                for value in set_cookies {
                                    if let Some((name, value)) =
                                        value.split(';').next().unwrap_or_default().split_once('=')
                                    {
                                        cookies.insert(name.trim().into(), value.trim().into());
                                    }
                                }
                                if let Some(raw) = value["cookie"].as_str() {
                                    for pair in raw.split(';') {
                                        if let Some((name, value)) = pair.split_once('=') {
                                            cookies.insert(name.trim().into(), value.trim().into());
                                        }
                                    }
                                }
                                if cookies.contains_key("MUSIC_U") {
                                    self.set_cookies(cookies, None, None)?;
                                }
                                // Best-effort account profile fetch: one
                                // single-shot GET, no retry, like today.
                                if let Ok(account) = self
                                    .http
                                    .send_once(&self.request(TransportRequest::get(
                                        "https://music.163.com/api/nuser/account/get",
                                    )))
                                    .await
                                    && let Ok(account) = account.json::<Value>()
                                {
                                    user_id = account["account"]["id"]
                                        .as_i64()
                                        .map(|v| v.to_string())
                                        .or_else(|| {
                                            account["profile"]["userId"]
                                                .as_i64()
                                                .map(|v| v.to_string())
                                        });
                                    display_name =
                                        account["profile"]["nickname"].as_str().map(str::to_owned);
                                    if let Some(mut session) = self.sessions.get(ID) {
                                        session.user_id = user_id.clone();
                                        session.display_name = display_name.clone();
                                        let _ = self.sessions.set(ID, session);
                                    }
                                }
                            }
                            return Ok(QrLoginPoll {
                                status,
                                message,
                                user_id,
                                display_name,
                            });
                        }
                        Err(error) => last_error = Some(perr(error.to_string())),
                    }
                }
                Err(error) => last_error = Some(perr(error.to_string())),
            }
        }
        Err(last_error.unwrap_or_else(|| perr("all QR login hosts failed")))
    }
}

#[async_trait]
impl AuthenticationProvider for NeteaseProvider {
    /// Migrated onto the unified [`AuthState`] machine (§8 slice 1 wiring):
    /// the state comes from `Self::auth_state` and the legacy three-state
    /// shape is its documented fold ([`AuthState::to_login_status`]). The
    /// account identity keeps today's independent user_id/display_name
    /// plumbing in the `Authenticated` arm (the QR success path writes both
    /// together, but a partial profile still reports the field it has,
    /// exactly like the pre-migration code).
    async fn login_status(&self) -> Result<LoginStatus> {
        let state = self.auth_state();
        Ok(match state {
            AuthState::Authenticated => {
                let session = self.sessions.get(ID);
                LoginStatus::LoggedIn {
                    user_id: session.as_ref().and_then(|s| s.user_id.clone()),
                    display_name: session.as_ref().and_then(|s| s.display_name.clone()),
                }
            }
            state => state.to_login_status(None),
        })
    }
    async fn logout(&self) -> Result<()> {
        self.sessions.clear(ID);
        Ok(())
    }
}
fn qr_status(code: i64, message: &str) -> QrLoginStatus {
    match code {
        800 => QrLoginStatus::Expired,
        801 => QrLoginStatus::Waiting,
        802 => QrLoginStatus::Scanned,
        803 => QrLoginStatus::Success,
        500 | 502 | 503 => QrLoginStatus::ServerError,
        _ if ["验证", "频繁", "风险"].iter().any(|v| message.contains(v)) => {
            QrLoginStatus::RiskControl
        }
        _ => QrLoginStatus::Failed,
    }
}

fn url_expiry(url: &str) -> Option<u64> {
    url.split_once('?')?.1.split('&').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        if k == "ex" || k == "expires" {
            v.parse().ok()
        } else {
            None
        }
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, MockTransport, TransportResponse};
    use std::sync::Arc;
    /// Loads a reconstructed real-response fixture from tests/fixtures/netease
    /// (see the README there for the derivation of these shapes).
    macro_rules! fixture {
        ($name:expr) => {
            serde_json::from_str::<Value>(include_str!(concat!(
                "../tests/fixtures/netease/",
                $name
            )))
            .expect("netease fixture must be valid JSON")
        };
    }
    #[test]
    fn eapi_ciphertext_is_upper_hex_and_block_aligned() {
        let v = eapi_encrypt(SEARCH_PATH, &json!({"keyword":"晴天"})).unwrap();
        assert!(!v.is_empty());
        assert_eq!(v.len() % 32, 0);
        assert!(
            v.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
        );
    }
    #[test]
    fn parses_new_and_legacy_search_shapes() {
        let v = json!({"baseInfo":{"simpleSongData":{"id":123,"name":"晴天","ar":[{"name":"周杰伦"}],"al":{"id":9,"name":"叶惠美","picUrl":"https://img"},"dt":240000,"h":{"size":100}}}});
        let t = parse_track(&v).unwrap();
        assert_eq!(t.id.id, "123");
        assert_eq!(t.artists, vec!["周杰伦"]);
        assert!(t.available_qualities.contains(&StreamQuality::High));
        let legacy =
            json!({"id":456,"name":"七里香","ar":[{"name":"周杰伦"}],"al":{"name":"七里香"}});
        assert_eq!(parse_track(&legacy).unwrap().id.id, "456");
        // Legacy /api/song/detail uses artists/album/duration instead of ar/al/dt.
        let legacy_detail = json!({
            "id":789,
            "name":"老接口",
            "artists":[{"name":"周杰伦"},{"name":"费玉清"}],
            "album":{"id":42,"name":"老专辑","picUrl":"https://p"},
            "duration":258000
        });
        let t = parse_track(&legacy_detail).unwrap();
        assert_eq!(t.artists, vec!["周杰伦", "费玉清"]);
        assert_eq!(t.album.as_deref(), Some("老专辑"));
        assert_eq!(t.album_id.as_deref(), Some("42"));
        assert_eq!(t.artwork_url.as_deref(), Some("https://p"));
        assert_eq!(t.duration.map(|d| d.as_millis()), Some(258000));
    }
    #[test]
    fn quality_ladder_only_steps_down() {
        assert_eq!(ladder(StreamQuality::High).last().unwrap().0, "standard");
        assert!(
            !ladder(StreamQuality::Low)
                .iter()
                .any(|(l, _, _)| *l == "lossless")
        );
    }
    #[tokio::test]
    async fn login_status_distinguishes_logged_out_and_expired() {
        let provider = NeteaseProvider::new();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        let mut cookies = BTreeMap::new();
        cookies.insert("MUSIC_U".into(), "secret-cookie".into());
        provider
            .set_cookies(cookies, Some("42".into()), Some("tester".into()))
            .unwrap();
        assert!(matches!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedIn { .. }
        ));
        let mut session = provider.session_store().get(ID).unwrap();
        session.tokens.insert("login_expired".into(), "1".into());
        provider.session_store().set(ID, session).unwrap();
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);
    }
    #[test]
    fn registration_exposes_only_implemented_capabilities() {
        let provider = NeteaseProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.lyrics);
        assert!(descriptor.capabilities.authentication);
        assert!(registry.qr_authentication_provider(ID).is_some());
        assert!(descriptor.capabilities.playlists);
        assert!(descriptor.capabilities.albums);
        assert!(descriptor.capabilities.artists);
        assert!(descriptor.capabilities.charts);
        assert!(descriptor.capabilities.link_resolve);
        assert!(registry.playlist_provider(ID).is_some());
        assert!(registry.collection_provider(ID).is_some());
        assert!(registry.album_provider(ID).is_some());
        assert!(registry.artist_provider(ID).is_some());
        assert!(registry.chart_provider(ID).is_some());
        assert!(registry.link_provider(ID).is_some());
    }
    #[test]
    fn parses_playlist_track_ids_and_playlist_metadata() {
        let value = json!({"id":123,"name":"精选","trackCount":42,"coverImgUrl":"https://img","description":"desc"});
        let p = parse_playlist(&value).unwrap();
        assert_eq!(p.id.id, "123");
        assert_eq!(p.track_count, Some(42));
        assert_eq!(
            parse_track_ids(&json!([{"id":1},2,{"id":0},{"id":"x"}])),
            vec![1, 2]
        );
    }
    #[test]
    fn parses_netease_share_links() {
        assert_eq!(
            parse_link_target("https://music.163.com/#/song?id=123"),
            Some(("song".into(), "123".into()))
        );
        assert_eq!(
            parse_link_target("https://music.163.com/#/playlist?id=456"),
            Some(("playlist".into(), "456".into()))
        );
        assert_eq!(
            parse_link_target("https://music.163.com/album?id=789"),
            Some(("album".into(), "789".into()))
        );
        assert_eq!(
            parse_link_target("https://music.163.com/#/song?id=abc"),
            None
        );
        // Vectors carried over from the upstream inline tests
        // (voicefox source/src/wy/parse.rs:230-269).
        assert_eq!(
            parse_link_target("https://music.163.com/#/song?id=123456"),
            Some(("song".into(), "123456".into()))
        );
        assert_eq!(
            parse_link_target("https://music.163.com/album?id=789&userid=1"),
            Some(("album".into(), "789".into()))
        );
        assert_eq!(
            parse_link_target("https://music.163.com/song/111"),
            Some(("song".into(), "111".into()))
        );
        assert_eq!(
            parse_link_target("http://music.163.com/playlist/222/"),
            Some(("playlist".into(), "222".into()))
        );
        assert_eq!(
            parse_link_target("https://music.163.com/#/discover/toplist"),
            None
        );
        assert_eq!(parse_link_target("周杰伦 晴天"), None);
    }
    #[test]
    fn rejects_non_netease_link_hosts() {
        assert_eq!(parse_link_target("https://evil.com/#/song?id=1"), None);
        assert_eq!(
            parse_link_target("https://music.163.com.evil.com/#/song?id=1"),
            None
        );
        assert!(parse_link_target("https://music.163.com/#/song?id=1").is_some());
        assert!(parse_link_target("https://y.music.163.com/m/song?id=2").is_some());
    }
    #[test]
    fn maps_netease_qr_status_and_risk_control() {
        assert_eq!(qr_status(800, ""), QrLoginStatus::Expired);
        assert_eq!(qr_status(801, ""), QrLoginStatus::Waiting);
        assert_eq!(qr_status(802, ""), QrLoginStatus::Scanned);
        assert_eq!(qr_status(803, ""), QrLoginStatus::Success);
        assert_eq!(qr_status(503, ""), QrLoginStatus::ServerError);
        assert_eq!(
            qr_status(405, "操作频繁，请稍候再试"),
            QrLoginStatus::RiskControl
        );
        assert_eq!(qr_status(882, "参数错误"), QrLoginStatus::Failed);
    }
    #[test]
    fn parses_url_expiry() {
        assert_eq!(
            url_expiry("https://cdn.test/a?foo=x&ex=1790000000"),
            Some(1790000000)
        );
        assert_eq!(url_expiry("https://cdn.test/a?foo=x"), None);
    }
    #[test]
    fn parses_album_detail_and_hot_album_items() {
        let album = parse_album(&json!({
            "id": 100,
            "name": "叶惠美",
            "artists": [{"name":"周杰伦"}],
            "picUrl": "https://img",
            "size": 11
        }))
        .unwrap();
        assert_eq!(album.id.id, "100");
        assert_eq!(album.artists, vec!["周杰伦"]);
        assert_eq!(album.track_count, Some(11));
        assert_eq!(album.artwork_url.as_deref(), Some("https://img"));
        // hotAlbums items carry a single artist object instead of an array.
        let from_songs = parse_album(&json!({
            "id": 101,
            "name": "EP",
            "artist": {"name": "独唱者"},
            "songs": [{}, {}, {}]
        }))
        .unwrap();
        assert_eq!(from_songs.artists, vec!["独唱者"]);
        assert_eq!(from_songs.track_count, Some(3));
        // Numeric-string ids (as returned by some catalogue endpoints) parse too.
        assert_eq!(
            parse_album(&json!({"id":"103","name":"A"})).unwrap().id.id,
            "103"
        );
        assert!(parse_album(&json!({"id":104,"name":"  "})).is_none());
        assert!(parse_album(&json!({"name":"无ID"})).is_none());
    }
    #[test]
    fn parses_chart_catalogue_items() {
        let chart = parse_chart(&json!({
            "id": 19723756,
            "name": "飙升榜",
            "updateFrequency": "每天更新"
        }))
        .unwrap();
        assert_eq!(chart.id.id, "19723756");
        assert_eq!(chart.title, "飙升榜");
        assert_eq!(chart.description.as_deref(), Some("每天更新"));
        assert!(parse_chart(&json!({"name":"无ID"})).is_none());
        assert!(parse_chart(&json!({"id":1})).is_none());
    }
    #[test]
    fn parses_playlist_catalogue_categories() {
        let value = json!({
            "categories": {"0":"语种","1":"风格"},
            "all": {"resourceCount": 100, "hot": true},
            "sub": [
                {"name":"华语","category":0,"resourceCount":5,"hot":false},
                {"name":"  ","category":1},
                {"name":"摇滚","category":1,"resourceCount":7,"hot":true}
            ]
        });
        let cats = parse_categories(&value);
        assert_eq!(cats.len(), 3);
        assert_eq!(cats[0].id, "全部");
        assert_eq!(cats[0].count, 100);
        assert!(cats[0].hot);
        assert_eq!(cats[1].name, "华语");
        assert_eq!(cats[1].group.as_deref(), Some("语种"));
        assert_eq!(cats[2].name, "摇滚");
        assert_eq!(cats[2].group.as_deref(), Some("风格"));
        assert!(cats[2].hot);
    }
    #[test]
    fn parses_lyric_document_variants() {
        let value = json!({
            "lrc": {"lyric": "[01:00.00]第一句\n[02:00.00]第二句"},
            "tlyric": {"lyric": "[01:00.00]first line"},
            "romalrc": {"lyric": ""}
        });
        let doc = parse_lyric_document(&value).unwrap();
        assert_eq!(doc.format, LyricFormat::Lrc);
        assert_eq!(doc.lines.len(), 2);
        assert_eq!(doc.lines[0].start_ms, 60_000);
        assert_eq!(doc.translation.as_deref(), Some("[01:00.00]first line"));
        assert_eq!(doc.romanization, None);
        assert!(parse_lyric_document(&json!({"lrc":{"lyric":"  "}})).is_none());
    }
    #[test]
    fn numeric_id_validation_rejects_non_positive() {
        assert_eq!(numeric_id(" 186016 ").unwrap(), 186016);
        assert!(matches!(numeric_id("abc"), Err(Error::InvalidInput(_))));
        assert!(numeric_id("0").is_err());
        assert!(numeric_id("-1").is_err());
        assert!(numeric_id("").is_err());
    }
    #[tokio::test]
    async fn session_expiry_marking_requires_an_existing_session() {
        let provider = NeteaseProvider::new();
        // No session yet: must be a no-op and not panic.
        let _ = provider.mark_session_expired(None);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        let mut cookies = BTreeMap::new();
        cookies.insert("MUSIC_U".into(), "cookie".into());
        provider.set_cookies(cookies, None, None).unwrap();
        let error = provider.mark_session_expired(None);
        assert_eq!(error.category(), crate::ErrorCategory::SessionExpired);
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);
        provider.logout().await.unwrap();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
    }

    #[tokio::test]
    async fn auth_state_derivation_drives_login_status() {
        let provider = NeteaseProvider::new();
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );

        // A session without the login cookie stays LoggedOut.
        let mut cookies = BTreeMap::new();
        cookies.insert("__csrf".into(), "c1".into());
        provider.set_cookies(cookies, None, None).unwrap();
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );

        // Login cookie: Authenticated, folded back through the state machine.
        let mut cookies = BTreeMap::new();
        cookies.insert("MUSIC_U".into(), "u1".into());
        provider
            .set_cookies(cookies, Some("42".into()), Some("tester".into()))
            .unwrap();
        assert_eq!(provider.auth_state(), AuthState::Authenticated);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedIn {
                user_id: Some("42".into()),
                display_name: Some("tester".into()),
            }
        );

        // Partial profile: the identity fields stay independent (legacy shape).
        let mut cookies = BTreeMap::new();
        cookies.insert("MUSIC_U".into(), "u2".into());
        provider
            .set_cookies(cookies, Some("43".into()), None)
            .unwrap();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedIn {
                user_id: Some("43".into()),
                display_name: None,
            }
        );

        // The expiry marker moves the state to Expired; both views agree.
        let mut session = provider.session_store().get(ID).unwrap();
        session.tokens.insert("login_expired".into(), "1".into());
        provider.session_store().set(ID, session).unwrap();
        assert_eq!(provider.auth_state(), AuthState::Expired);
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);
    }

    #[test]
    fn fatal_api_code_parses_the_classifier_text() {
        assert_eq!(fatal_api_code("API code 881: 参数异常"), Some(881));
        assert_eq!(fatal_api_code("API code 405: throttled"), Some(405));
        assert_eq!(fatal_api_code("HTTP 502"), None);
        assert_eq!(fatal_api_code("API code nope: x"), None);
        assert_eq!(fatal_api_code(""), None);
    }

    #[test]
    fn api_code_error_maps_throttled_405_to_rate_limited() {
        // The exhaustion text comes from `retry::classify_api`
        // ("API code {code}: throttled"). End-to-end exhaustion would need
        // five scripted sends with real backoff sleeps, so the mapping is
        // pinned at this pure seam: retry.rs already pins 405-exhaustion ->
        // Fatal("API code 405: throttled") and the e2e 881 tests pin the
        // Api-arm wiring this function sits in.
        let error = api_code_error(Some("song details"), "API code 405: throttled");
        assert_eq!(error.category(), crate::ErrorCategory::RateLimited);
        assert_eq!(error.provider_id(), Some("netease"));
        assert!(error.is_retryable());
        assert_eq!(
            error.detail(),
            Some("song details: API code 405: throttled")
        );
        assert_eq!(
            error.to_string(),
            "rate limited: song details: API code 405: throttled"
        );
        assert!(
            matches!(
                error,
                Error::RateLimited {
                    retry_after: None,
                    ..
                }
            ),
            "a 405 body code carries no server-sent duration; the Retry-After slot stays empty"
        );

        // eapi keeps today's no-label shape.
        let error = api_code_error(None, "API code 405: throttled");
        assert_eq!(error.to_string(), "rate limited: API code 405: throttled");
        assert_eq!(error.detail(), Some("API code 405: throttled"));
    }

    #[test]
    fn api_code_error_keeps_legacy_catch_all_for_unparsed_texts() {
        // The one realistic Api text without an "API code" prefix (the
        // engine's non-idempotent retry note) and any future classifier texts
        // keep the exact legacy `perr` shape.
        let error = api_code_error(
            Some("song details"),
            "retryable body verdict on a non-idempotent request; not resent",
        );
        assert_eq!(
            error.to_string(),
            "provider netease failed: song details: retryable body verdict on a non-idempotent request; not resent"
        );
        assert_eq!(error.category(), crate::ErrorCategory::Provider);
        let error = api_code_error(None, "not an api code text");
        assert_eq!(
            error.to_string(),
            "provider netease failed: not an api code text"
        );
        assert_eq!(error.category(), crate::ErrorCategory::Provider);
    }

    // --- fixture tests: reconstructed real-response shapes, see
    // tests/fixtures/netease/README.md for the derivation of each file.

    #[test]
    fn playlist_detail_fixture_pins_truncated_tracks_shape() {
        let value = fixture!("playlist_detail_v3.json");
        assert_eq!(value["code"], 200);
        let playlist = parse_playlist(&value["playlist"]).expect("playlist metadata");
        assert_eq!(playlist.id.id, "123");
        assert_eq!(playlist.title, "精选歌单");
        assert_eq!(playlist.track_count, Some(145));
        assert!(playlist.artwork_url.is_some());
        assert_eq!(
            playlist.description.as_deref(),
            Some("完整列表在 trackIds 的示例歌单")
        );
        // Favorited playlists return only the first ~20 tracks inline; the
        // complete list lives in trackIds and the server ignores the n parameter.
        let ids = parse_track_ids(&value["playlist"]["trackIds"]);
        assert_eq!(ids.len(), 145);
        assert_eq!(ids.first(), Some(&101));
        assert_eq!(ids.last(), Some(&245));
        let fallback = playlist_tracks_fallback(&value["playlist"]);
        assert_eq!(fallback.len(), 20);
        assert_ne!(
            fallback.len(),
            usize::try_from(playlist.track_count.unwrap_or(0)).unwrap_or(0)
        );
        assert_eq!(fallback[0].id.id, "101");
        assert!(
            fallback[0]
                .available_qualities
                .contains(&StreamQuality::HiRes)
        );
    }

    #[test]
    fn hot_playlist_fixture_skips_unparseable_entries() {
        let value = fixture!("hot_playlists.json");
        assert_eq!(value["code"], 200);
        let items: Vec<_> = value["playlists"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_playlist)
            .collect();
        // Six entries; the blank-name one is dropped.
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].title, "精选歌单");
        assert_eq!(items[0].track_count, Some(42));
        assert_eq!(items[1].description, None);
    }

    #[test]
    fn playlist_search_fixture_reads_result_playlists() {
        let value = fixture!("playlist_search.json");
        assert_eq!(value["code"], 200);
        let items: Vec<_> = value
            .pointer("/result/playlists")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_playlist)
            .collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id.id, "201");
    }

    #[test]
    fn playlist_catalogue_fixture_maps_group_names() {
        let value = fixture!("playlist_catalogue.json");
        assert_eq!(value["code"], 200);
        let cats = parse_categories(&value);
        assert_eq!(cats.len(), 3);
        assert_eq!(cats[0].id, "全部");
        assert_eq!(cats[0].count, 100);
        assert!(cats[0].hot);
        assert_eq!(cats[1].name, "华语");
        assert_eq!(cats[1].group.as_deref(), Some("语种"));
        // The blank-name sub category is skipped.
        assert_eq!(cats[2].name, "摇滚");
        assert_eq!(cats[2].group.as_deref(), Some("风格"));
        assert!(cats[2].hot);
    }

    #[test]
    fn album_detail_fixture_parses_album_node_and_songs() {
        let value = fixture!("album_detail.json");
        assert_eq!(value["code"], 200);
        let album = parse_album(&value["album"]).expect("album metadata");
        assert_eq!(album.title, "叶惠美");
        assert_eq!(album.artists, vec!["周杰伦"]);
        assert_eq!(album.track_count, Some(11));
        assert_eq!(
            album.artwork_url.as_deref(),
            Some("https://p1.music.126.net/sample/album.jpg")
        );
        // album_tracks reads the top-level songs array (album.songs is the fallback).
        let songs: Vec<_> = value["songs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_track)
            .collect();
        assert_eq!(songs.len(), 11);
        assert_eq!(songs[0].title, "晴天");
        assert_eq!(songs[0].album.as_deref(), Some("叶惠美"));
        assert!(songs[0].available_qualities.contains(&StreamQuality::High));
        assert!(!songs[0].available_qualities.contains(&StreamQuality::HiRes));
        assert!(songs[1].available_qualities.contains(&StreamQuality::HiRes));
    }

    #[test]
    fn artist_detail_fixture_parses_identity_and_hot_songs() {
        let value = fixture!("artist_detail.json");
        assert_eq!(value["code"], 200);
        let (name, artwork) = parse_artist_node(&value["artist"]).expect("artist identity");
        assert_eq!(name, "周杰伦");
        assert_eq!(
            artwork.as_deref(),
            Some("https://p1.music.126.net/sample/artist.jpg")
        );
        assert_eq!(parse_artist_node(&json!({"name": ""})), None);
        assert_eq!(parse_artist_node(&json!({})), None);
        let songs = parse_hot_songs(&value);
        assert_eq!(songs.len(), 50);
        assert_eq!(songs[0].id.id, "123");
        assert!(songs[0].available_qualities.contains(&StreamQuality::High));
        assert!(!songs[0].available_qualities.contains(&StreamQuality::HiRes));
        // Hot song #10 in the fixture carries hr plus a hires privilege level.
        assert!(
            songs[10]
                .available_qualities
                .contains(&StreamQuality::HiRes)
        );
    }

    #[test]
    fn artist_albums_fixture_reads_hot_albums_variants() {
        let value = fixture!("artist_albums.json");
        assert_eq!(value["code"], 200);
        let albums = parse_hot_albums(&value);
        assert_eq!(albums.len(), 5);
        // A positive size wins over the songs preview length.
        assert_eq!(albums[0].track_count, Some(11));
        assert_eq!(albums[0].artists, vec!["周杰伦"]);
        // Without size the count falls back to the preview length; the single
        // artist object (not an array) still maps to the artist name.
        assert_eq!(albums[1].track_count, Some(3));
        assert_eq!(albums[1].artists, vec!["独唱者"]);
        assert_eq!(albums[3].artists, vec!["合唱团"]);
        assert_eq!(albums[3].track_count, Some(2));
    }

    #[test]
    fn toplist_fixture_parses_chart_catalogue() {
        let value = fixture!("toplist.json");
        assert_eq!(value["code"], 200);
        let charts: Vec<_> = value["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_chart)
            .collect();
        // Five entries; the id-less one is dropped.
        assert_eq!(charts.len(), 4);
        assert_eq!(charts[0].id.id, "19723756");
        assert_eq!(charts[0].title, "飙升榜");
        assert_eq!(charts[0].description.as_deref(), Some("每天更新"));
    }

    #[test]
    fn song_link_detail_fixture_parses_legacy_field_names() {
        let value = fixture!("song_detail_legacy.json");
        assert_eq!(value["code"], 200);
        // resolve_link reads songs[0] of the /api/song/detail response, whose
        // legacy shape uses artists/album/duration instead of ar/al/dt.
        let track = value["songs"]
            .as_array()
            .and_then(|songs| songs.first())
            .and_then(parse_track)
            .expect("legacy song");
        assert_eq!(track.id.id, "789");
        assert_eq!(track.artists, vec!["周杰伦", "费玉清"]);
        assert_eq!(track.album.as_deref(), Some("老专辑"));
        assert_eq!(track.album_id.as_deref(), Some("42"));
        assert_eq!(track.artwork_url.as_deref(), Some("https://p"));
        assert_eq!(track.duration.map(|d| d.as_millis()), Some(258000));
        assert!(track.available_qualities.contains(&StreamQuality::High));
        assert!(track.available_qualities.contains(&StreamQuality::Lossless));
        assert!(track.available_qualities.contains(&StreamQuality::HiRes));
        assert_eq!(
            track.extra.get("pl_level").map(String::as_str),
            Some("lossless")
        );
    }

    #[test]
    fn chart_tracks_fixture_ranks_entries_from_one() {
        let value = fixture!("playlist_detail_v3.json");
        // chart_tracks assigns ranks 1..N in list order.
        let entries = chart_entries(playlist_tracks_fallback(&value["playlist"]));
        assert_eq!(entries.len(), 20);
        assert_eq!(entries[0].rank, 1);
        assert_eq!(entries.last().unwrap().rank, 20);
        assert_eq!(entries[0].track.id.id, "101");
    }

    #[tokio::test]
    async fn fixture_stream_response_pins_md5_checksum_algorithm() {
        // The eapi `song/enhance/player/url/v1` data block is the one
        // production StreamInfo site carrying a real digest, so the M3
        // `checksum_algorithm` backfill is pinned over the actual resolve
        // parse path (fixture shape: tests/fixtures/netease/README.md), not
        // a hand-built struct.
        let body = fixture!("song_enhance_player_url_v1.json").to_string();
        let mock = Arc::new(MockTransport::script([Ok(mock_json(200, &body))]));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let info = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .unwrap();
        // Default resolve quality is High, whose ladder head is the
        // `exhigh`/mp3 tier the fixture answers; format falls out of
        // `encodeType != "flac"`.
        assert_eq!(info.quality, StreamQuality::High);
        assert_eq!(info.format, AudioFormat::Mp3);
        assert_eq!(
            info.content_length,
            Some(8523134),
            "size rides content_length"
        );
        assert_eq!(
            info.checksum.as_deref(),
            Some("0123456789abcdef0123456789abcdef"),
            "the parse path lowercases the digest"
        );
        assert_eq!(
            info.checksum_algorithm,
            Some(ChecksumAlgorithm::Md5),
            "M3: the md5 field names its algorithm explicitly, not via the None default"
        );
        assert_eq!(
            info.md5_checksum(),
            Some("0123456789abcdef0123456789abcdef"),
            "the engine MD5 gate consumes the named checksum"
        );
        assert!(!info.size_is_advisory, "eapi size is byte-exact (§2.2)");
        assert_eq!(
            info.expires_at_unix, None,
            "the CDN URL carries no ex/expires query param, url_expiry reads None"
        );
        assert!(info.fallback_urls.is_empty());
        assert!(info.fallback_endpoints.is_empty());
    }

    // --- unified transport migration tests (mock transport, no network):
    // the provider runs against Http::from_shared_mock and the recorded
    // request log pins the migrated request faces (TRANSPORT_DESIGN.md §7.1).

    fn mock_json(status: u16, body: &str) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: body.as_bytes().to_vec(),
        }
    }

    fn sample_track() -> Track {
        Track {
            id: TrackId::new(ID, "186016"),
            title: "晴天".into(),
            artists: vec!["周杰伦".into()],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn get_json_runs_through_send_api_json_with_cookie_whitelist() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert_eq!(request.method, HttpMethod::Get);
            assert_eq!(request.url, "https://music.163.com/api/x");
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "user-agent" && v.contains("NeteaseMusicDesktop"))
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "referer" && v == REFERER)
            );
            Ok(mock_json(200, r#"{"code":200,"playlist":{"id":1}}"#))
        }));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let mut cookies = BTreeMap::new();
        for (name, value) in [
            ("MUSIC_U", "u1"),
            ("MUSIC_A", "a1"),
            ("__csrf", "c1"),
            ("NMTID", "n1"),
            ("JSESSIONID-WYYY", "j1"),
            ("SOME_TRACKER", "dropped"),
        ] {
            cookies.insert(name.into(), value.into());
        }
        provider.set_cookies(cookies, None, None).unwrap();
        let value = provider
            .get_json("https://music.163.com/api/x", "label")
            .await
            .unwrap();
        assert_eq!(value["playlist"]["id"], 1);
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        let cookie = recorded[0]
            .headers
            .iter()
            .find(|(k, _)| k == "cookie")
            .map(|(_, v)| v.as_str())
            .expect("whitelisted cookies must be sent");
        for pair in [
            "MUSIC_U=u1",
            "MUSIC_A=a1",
            "__csrf=c1",
            "NMTID=n1",
            "JSESSIONID-WYYY=j1",
        ] {
            assert!(
                cookie.contains(pair),
                "cookie must contain {pair}: {cookie}"
            );
        }
        assert!(
            !cookie.contains("SOME_TRACKER"),
            "non-whitelisted cookies stay in the store, never on the wire"
        );
    }

    #[tokio::test]
    async fn get_json_retries_throttled_code_then_succeeds() {
        let mock = Arc::new(MockTransport::script([
            Ok(mock_json(200, r#"{"code":405,"message":"throttled"}"#)),
            Ok(mock_json(200, r#"{"code":200,"songs":[]}"#)),
        ]));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let value = provider
            .get_json("https://music.163.com/api/x", "label")
            .await
            .unwrap();
        assert_eq!(value["songs"].as_array().map(Vec::len), Some(0));
        assert_eq!(mock.recorded().len(), 2, "405 retried once then succeeded");
    }

    #[tokio::test]
    async fn get_json_fatal_api_code_becomes_invalid_response_preserving_label() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":881,"message":"参数异常"}"#,
        ))]));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider
            .get_json("https://music.163.com/api/x", "song details")
            .await
            .unwrap_err();
        // Pinned text updated for the M2 migration (AUTH_REGISTRY_DESIGN.md
        // §1.2/§8 slice 1): the M2 Display drops the "provider netease
        // failed:" prefix; the label survives in the detail slot, and the
        // classifier's fatal text is preserved verbatim as the source-chain
        // detail.
        assert_eq!(
            error.to_string(),
            "invalid response (API code 881): song details: API code 881: 参数异常"
        );
        assert_eq!(error.category(), crate::ErrorCategory::InvalidResponse);
        assert_eq!(error.provider_id(), Some("netease"));
        assert!(!error.is_retryable());
        assert_eq!(error.detail(), Some("song details: API code 881: 参数异常"));
        assert!(
            matches!(
                error,
                Error::InvalidResponse {
                    code: Some(881),
                    ..
                }
            ),
            "the typed code slot carries the platform code"
        );
        assert_eq!(mock.recorded().len(), 1, "fatal codes never retry");
    }

    #[tokio::test]
    async fn get_json_marks_session_expired_on_301() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":301,"message":"need login"}"#,
        ))]));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let mut cookies = BTreeMap::new();
        cookies.insert("MUSIC_U".into(), "u1".into());
        provider.set_cookies(cookies, None, None).unwrap();
        let error = provider
            .get_json("https://music.163.com/api/x", "song details")
            .await
            .unwrap_err();
        // Pinned text updated for the M2 migration: `Error::SessionExpired`
        // keeps the exact "login required or session expired" body (so the
        // message text survives minus the "provider netease failed:" prefix)
        // and the label rides in the detail slot (AUTH_REGISTRY_DESIGN.md
        // §1.2, §8 slice 1).
        assert_eq!(
            error.to_string(),
            "login required or session expired: song details"
        );
        assert_eq!(error.category(), crate::ErrorCategory::SessionExpired);
        assert_eq!(error.provider_id(), Some("netease"));
        assert!(
            !error.is_retryable(),
            "expiry recovers by re-login, not resend"
        );
        assert_eq!(error.detail(), Some("song details"));
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn eapi_posts_idempotent_body_through_unified_layer() {
        let mock = Arc::new(MockTransport::script([
            Err(TransportError::Transport("connection reset".into())),
            Ok(mock_json(200, r#"{"code":200,"data":{"ok":true}}"#)),
        ]));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let value = provider
            .eapi(
                "https://interface.music.163.com/eapi/batch",
                SEARCH_PATH,
                &json!({"keyword": "晴天"}),
            )
            .await
            .unwrap();
        assert_eq!(value["data"]["ok"], true);
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2, "one transport retry then success");
        for request in &recorded {
            assert_eq!(request.method, HttpMethod::Post);
            assert!(
                request.idempotent,
                "the eapi POST declares its body resend (.idempotent(true))"
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "content-type" && v == "application/x-www-form-urlencoded")
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "user-agent" && v.contains("NeteaseMusicDesktop"))
            );
            assert!(
                request
                    .body
                    .as_deref()
                    .unwrap_or(&[])
                    .starts_with(b"params="),
                "the body carries the encrypted params form field"
            );
        }
    }

    #[tokio::test]
    async fn eapi_session_expired_has_no_label_prefix() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":512,"message":""}"#,
        ))]));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let mut cookies = BTreeMap::new();
        cookies.insert("MUSIC_U".into(), "u1".into());
        provider.set_cookies(cookies, None, None).unwrap();
        let error = provider
            .eapi(
                "https://interface3.music.163.com/eapi/song/enhance/player/url/v1",
                PLAYER_PATH,
                &json!({"ids": "[186016]"}),
            )
            .await
            .unwrap_err();
        // Pinned text updated for the M2 migration: eapi carries no label, so
        // the error is the bare classified variant and its Display body is
        // byte-identical with the legacy "login required or session expired"
        // string minus the "provider netease failed:" prefix (§1.2/§8).
        assert_eq!(error.to_string(), "login required or session expired");
        assert_eq!(error.category(), crate::ErrorCategory::SessionExpired);
        assert_eq!(error.provider_id(), Some("netease"));
        assert_eq!(error.detail(), None, "eapi expiry carries no label");
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);
    }

    #[tokio::test]
    async fn eapi_fatal_api_code_surfaces_invalid_response() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":881,"message":"参数异常"}"#,
        ))]));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider
            .eapi(
                "https://interface.music.163.com/eapi/batch",
                SEARCH_PATH,
                &json!({"keyword": "晴天"}),
            )
            .await
            .unwrap_err();
        // eapi keeps today's no-label shape: the typed code slot carries the
        // platform code and the classifier's fatal text is the detail.
        assert_eq!(
            error.to_string(),
            "invalid response (API code 881): API code 881: 参数异常"
        );
        assert_eq!(error.category(), crate::ErrorCategory::InvalidResponse);
        assert!(
            matches!(
                error,
                Error::InvalidResponse {
                    code: Some(881),
                    ..
                }
            ),
            "the typed code slot carries the platform code"
        );
    }

    #[tokio::test]
    async fn resolve_fallback_head_probe_is_single_shot_with_own_timeout() {
        let mock = Arc::new(MockTransport::new(|request| match request.method {
            HttpMethod::Head => Ok(TransportResponse {
                status: 200,
                headers: vec![(
                    "content-type".to_owned(),
                    "audio/mpeg; charset=binary".to_owned(),
                )],
                url: None,
                body: Vec::new(),
            }),
            // Every ladder eapi attempt fails fast with a fatal API code so
            // the ladder drains without any backoff sleep.
            _ => Ok(mock_json(200, r#"{"code":881,"message":"参数异常"}"#)),
        }));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let info = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .unwrap();
        assert_eq!(
            info.url,
            "https://music.163.com/song/media/outer/url?id=186016.mp3"
        );
        assert_eq!(info.quality, StreamQuality::Low);
        assert_eq!(info.format, AudioFormat::Mp3);
        // The dead-fallback URL carries no digest, so the algorithm slot
        // stays `None` (§2.3: no data, no name; MD5 default is inert).
        assert_eq!(info.checksum, None);
        assert_eq!(info.checksum_algorithm, None);
        let recorded = mock.recorded();
        let head = recorded
            .iter()
            .find(|r| r.method == HttpMethod::Head)
            .expect("the fallback probe must go out as a HEAD");
        assert_eq!(
            recorded
                .iter()
                .filter(|r| r.method == HttpMethod::Head)
                .count(),
            1,
            "exactly one HEAD probe: send_once never retries"
        );
        assert_eq!(
            head.timeout,
            Some(Duration::from_secs(3)),
            "the probe keeps its 3s per-request timeout instead of the 15s profile"
        );
        assert!(head.idempotent);
        assert!(
            head.headers.is_empty(),
            "the probe adds no request-level headers today; keep it bare"
        );
    }

    #[tokio::test]
    async fn resolve_rejects_non_audio_fallback_and_probe_failure_keeps_ladder_error() {
        // Non-audio content type: the probe answer is rejected outright.
        let mock = Arc::new(MockTransport::new(|request| match request.method {
            HttpMethod::Head => Ok(TransportResponse {
                status: 200,
                headers: vec![("content-type".to_owned(), "text/html".to_owned())],
                url: None,
                body: Vec::new(),
            }),
            _ => Ok(mock_json(200, r#"{"code":881,"message":"参数异常"}"#)),
        }));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock));
        let error = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider netease failed: no playable stream URL returned by NetEase"
        );

        // Probe transport failure: the last ladder error prefixes the message.
        let mock = Arc::new(MockTransport::new(|request| match request.method {
            HttpMethod::Head => Err(TransportError::Transport("connection refused".into())),
            _ => Ok(mock_json(200, r#"{"code":881,"message":"参数异常"}"#)),
        }));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock));
        let error = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("fallback probe failed: connection refused"),
            "got: {text}"
        );
        assert!(
            text.contains("API code 881"),
            "the last ladder error must prefix the probe failure: {text}"
        );
    }

    #[tokio::test]
    async fn create_qr_login_fails_over_hosts_one_send_each() {
        let mock = Arc::new(MockTransport::new(|request| {
            let url = request.url.as_str();
            if url.contains("interface.music.163.com") {
                Err(TransportError::Transport("dns failure".into()))
            } else if url.contains("interface3.music.163.com") {
                Ok(mock_json(200, r#"{"code":882,"message":"参数错误"}"#))
            } else {
                Ok(mock_json(200, r#"{"code":200,"unikey":" KEY123 "}"#))
            }
        }));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let session = provider.create_qr_login().await.unwrap();
        assert_eq!(session.key, "KEY123", "the unikey value is trimmed");
        assert_eq!(session.url, "https://music.163.com/login?codekey=KEY123");
        assert_eq!(session.expires_in, 300);
        let recorded = mock.recorded();
        assert_eq!(
            recorded.len(),
            3,
            "one attempt per host: the loop rotation is the retry, no time retry"
        );
        for request in &recorded {
            assert_eq!(request.method, HttpMethod::Post);
            assert!(
                !request.idempotent,
                "QR POSTs keep the method-default non-idempotent flag"
            );
            assert_eq!(request.body.as_deref(), Some(b"type=3".as_slice()));
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "content-type" && v == "application/x-www-form-urlencoded")
            );
        }
        assert_eq!(
            recorded[0].url,
            "https://interface.music.163.com/api/login/qrcode/unikey"
        );
        assert_eq!(
            recorded[2].url,
            "https://music.163.com/api/login/qrcode/unikey"
        );
    }

    #[tokio::test]
    async fn poll_qr_login_success_harvests_set_cookies_and_fetches_account() {
        let mock = Arc::new(MockTransport::new(|request| {
            if request.url.contains("client/login") {
                Ok(TransportResponse {
                    status: 200,
                    headers: vec![
                        (
                            "set-cookie".to_owned(),
                            "MUSIC_U=token1; Path=/; HttpOnly".to_owned(),
                        ),
                        ("set-cookie".to_owned(), "__csrf=c1; Path=/".to_owned()),
                    ],
                    url: None,
                    body: r#"{"code":803,"message":"成功"}"#.as_bytes().to_vec(),
                })
            } else {
                Ok(mock_json(
                    200,
                    r#"{"code":200,"account":{"id":42},"profile":{"nickname":"tester"}}"#,
                ))
            }
        }));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let poll = provider.poll_qr_login(" KEY ").await.unwrap();
        assert_eq!(poll.status, QrLoginStatus::Success);
        assert_eq!(poll.user_id.as_deref(), Some("42"));
        assert_eq!(poll.display_name.as_deref(), Some("tester"));
        let recorded = mock.recorded();
        assert_eq!(
            recorded.len(),
            2,
            "login POST plus one single-shot account GET"
        );
        assert_eq!(recorded[0].method, HttpMethod::Post);
        assert!(
            recorded[0]
                .body
                .as_deref()
                .unwrap_or(&[])
                .starts_with(b"key=KEY&type=3"),
            "the poll body carries the trimmed key"
        );
        assert_eq!(recorded[1].method, HttpMethod::Get);
        assert!(
            recorded[1]
                .headers
                .iter()
                .any(|(k, v)| k == "cookie" && v.contains("MUSIC_U=token1")),
            "the account GET rides the harvested session through the whitelist"
        );
        let session = provider.session_store().get(ID).unwrap();
        assert_eq!(
            session.cookies.get("MUSIC_U").map(String::as_str),
            Some("token1")
        );
        assert_eq!(
            session.cookies.get("__csrf").map(String::as_str),
            Some("c1")
        );
        assert_eq!(session.user_id.as_deref(), Some("42"));
        assert_eq!(session.display_name.as_deref(), Some("tester"));
    }

    #[tokio::test]
    async fn poll_qr_login_fails_over_hosts_and_reports_expired() {
        let mock = Arc::new(MockTransport::new(|request| {
            if request.url.contains("interface.music.163.com") {
                Err(TransportError::Transport("connection refused".into()))
            } else {
                Ok(mock_json(200, r#"{"code":800,"message":"过期"}"#))
            }
        }));
        let provider = NeteaseProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let poll = provider.poll_qr_login("KEY").await.unwrap();
        assert_eq!(poll.status, QrLoginStatus::Expired);
        assert_eq!(poll.message, "过期");
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2, "one send per host, no time retry");
        for request in &recorded {
            assert_eq!(request.method, HttpMethod::Post);
            assert!(!request.idempotent);
        }
    }
}
