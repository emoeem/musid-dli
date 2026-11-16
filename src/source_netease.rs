//! NetEase Cloud Music provider migrated from VoiceFox's `lx-source::wy`.
//! Search, stream resolution and lyrics are implemented; QR login and collections remain pending.
use crate::{
    Error, Result,
    lrc::parse_lrc,
    models::{
        Album, Artist, AudioFormat, Chart, ChartEntry, LyricDocument, LyricFormat, Playlist,
        PlaylistCategory, StreamInfo, StreamQuality, Track, TrackId,
    },
    provider::{
        AlbumProvider, ArtistProvider, AuthenticationProvider, ChartProvider, CollectionProvider,
        LinkResolver, LoginStatus, LyricsProvider, MusicProvider, PlaylistProvider,
        QrAuthenticationProvider, QrLoginPoll, QrLoginSession, QrLoginStatus, ResolveOptions,
        ResolvedLink, SearchPage, SearchQuery, StreamResolver, TrackSearch,
    },
    retry::{NETEASE_CODES, StepOutcome, classify_api, classify_http},
    session::{ProviderSession, SessionStore},
};
use aes::{
    Aes128,
    cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use async_trait::async_trait;
use md5::{Digest, Md5};
use reqwest::{Client, RequestBuilder, header};
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
    client: Client,
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
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(UA)
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
            },
        )
    }
    /// Flag the current session as expired so `login_status` reports it.
    fn mark_session_expired(&self) {
        if let Some(mut session) = self.sessions.get(ID) {
            session.tokens.insert("login_expired".into(), "1".into());
            let _ = self.sessions.set(ID, session);
        }
    }
    async fn get_json(&self, url: &str, label: &str) -> Result<Value> {
        const BACKOFF_MS: [u64; 4] = [600, 1_200, 2_400, 4_000];
        for (attempt, delay) in BACKOFF_MS
            .iter()
            .copied()
            .chain(std::iter::once(0))
            .enumerate()
        {
            let response = self.request(self.client.get(url)).send().await;
            let response = match response {
                Ok(response) => response,
                Err(error) if attempt < BACKOFF_MS.len() => {
                    tracing::debug!("NetEase {label} transport retry {}: {error}", attempt + 1);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }
                Err(error) => return Err(perr(format!("{label}: {error}"))),
            };
            match classify_http(response.status().as_u16(), attempt, BACKOFF_MS.len()) {
                StepOutcome::Retry => {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }
                StepOutcome::Fatal(message) => return Err(perr(format!("{label}: {message}"))),
                StepOutcome::SessionExpired | StepOutcome::Ok => {}
            }
            let value: Value = response
                .json()
                .await
                .map_err(|e| perr(format!("{label}: {e}")))?;
            let message = value["message"].as_str().unwrap_or("unknown error");
            match classify_api(
                value["code"].as_i64().unwrap_or(NETEASE_CODES.ok),
                message,
                attempt,
                BACKOFF_MS.len(),
                &NETEASE_CODES,
            ) {
                StepOutcome::Ok => return Ok(value),
                StepOutcome::Retry => {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    continue;
                }
                StepOutcome::SessionExpired => {
                    self.mark_session_expired();
                    return Err(perr(format!("{label}: login required or session expired")));
                }
                StepOutcome::Fatal(m) => return Err(perr(format!("{label}: {m}"))),
            }
        }
        Err(perr(format!("{label}: retry budget exhausted")))
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
    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        let mut request = request
            .header(header::USER_AGENT, UA)
            .header(header::REFERER, REFERER);
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
                request = request.header(header::COOKIE, cookie);
            }
        }
        request
    }
    async fn eapi(&self, endpoint: &str, path: &str, payload: &Value) -> Result<Value> {
        const BACKOFF_MS: [u64; 3] = [400, 900, 1_800];
        let body = eapi_encrypt(path, payload)?;
        for (attempt, backoff_ms) in BACKOFF_MS
            .iter()
            .copied()
            .chain(std::iter::once(0))
            .enumerate()
        {
            let response = self
                .request(self.client.post(endpoint))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(format!("params={body}"))
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) if attempt < BACKOFF_MS.len() => {
                    tracing::debug!(
                        "NetEase transient transport error (attempt {}): {error}",
                        attempt + 1
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    continue;
                }
                Err(error) => return Err(perr(error.to_string())),
            };
            match classify_http(response.status().as_u16(), attempt, BACKOFF_MS.len()) {
                StepOutcome::Retry => {
                    tracing::debug!(
                        "NetEase retryable HTTP status; retrying after backoff (attempt {})",
                        attempt + 1
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    continue;
                }
                StepOutcome::Fatal(message) => return Err(perr(message)),
                StepOutcome::SessionExpired | StepOutcome::Ok => {}
            }
            let value: Value = response.json().await.map_err(|e| perr(e.to_string()))?;
            let message = value["message"].as_str().unwrap_or("unknown error");
            match classify_api(
                value["code"].as_i64().unwrap_or(NETEASE_CODES.ok),
                message,
                attempt,
                BACKOFF_MS.len(),
                &NETEASE_CODES,
            ) {
                StepOutcome::Ok => return Ok(value),
                StepOutcome::Retry => {
                    tracing::debug!(
                        "NetEase API throttled; retrying after backoff (attempt {})",
                        attempt + 1
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    continue;
                }
                StepOutcome::SessionExpired => {
                    self.mark_session_expired();
                    return Err(perr("login required or session expired"));
                }
                StepOutcome::Fatal(m) => return Err(perr(m)),
            }
        }
        Err(perr("retry budget exhausted"))
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
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
        let response = self
            .client
            .head(&fallback)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map_err(|e| {
                perr(format!(
                    "{}; fallback probe failed: {e}",
                    last.map(|e: Error| e.to_string())
                        .unwrap_or_else(|| "no playable quality".into())
                ))
            })?;
        let kind = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("");
        if !response.status().is_success()
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
            return Ok(playlist["tracks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(parse_track)
                .collect());
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
        let name = a["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(Error::NotFound)?;
        Ok(Artist {
            id: TrackId::new(ID, id.to_string()),
            name: name.into(),
            artwork_url: a["picUrl"].as_str().map(str::to_owned),
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
        Ok(value["hotSongs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_track)
            .collect())
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
        Ok(value["hotAlbums"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_album)
            .collect())
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
            pl["tracks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(parse_track)
                .collect()
        } else {
            self.track_details(&ids).await?
        };
        Ok(tracks
            .into_iter()
            .enumerate()
            .map(|(i, track)| ChartEntry {
                rank: (i + 1) as u32,
                track,
            })
            .collect())
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
    })
}
fn parse_categories(v: &Value) -> Vec<PlaylistCategory> {
    let groups = v["categories"].as_object();
    let mut result = vec![PlaylistCategory {
        id: "全部".into(),
        name: "全部".into(),
        group: Some("全部".into()),
        count: v["all"]["resourceCount"].as_u64().unwrap_or(0) as u32,
        hot: v["all"]["hot"].as_bool().unwrap_or(true),
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
            match self
                .request(self.client.post(url))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body("type=3")
                .send()
                .await
            {
                Ok(response) => match response.json::<Value>().await {
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
            match self
                .request(self.client.post(url))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(format!("key={}&type=3", urlencoding::encode(key)))
                .send()
                .await
            {
                Ok(response) => {
                    let headers = response.headers().clone();
                    match response.json::<Value>().await {
                        Ok(value) => {
                            let code = value["code"].as_i64().unwrap_or_default();
                            let message = value["message"].as_str().unwrap_or_default().to_owned();
                            let status = qr_status(code, &message);
                            let mut user_id = None;
                            let mut display_name = None;
                            if status == QrLoginStatus::Success {
                                let mut cookies = BTreeMap::new();
                                for value in headers.get_all(header::SET_COOKIE) {
                                    if let Ok(value) = value.to_str()
                                        && let Some((name, value)) = value
                                            .split(';')
                                            .next()
                                            .unwrap_or_default()
                                            .split_once('=')
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
                                if let Ok(response) = self
                                    .request(
                                        self.client
                                            .get("https://music.163.com/api/nuser/account/get"),
                                    )
                                    .send()
                                    .await
                                    && let Ok(account) = response.json::<Value>().await
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
    async fn login_status(&self) -> Result<LoginStatus> {
        let Some(s) = self.sessions.get(ID) else {
            return Ok(LoginStatus::LoggedOut);
        };
        if !s.cookies.contains_key("MUSIC_U") {
            return Ok(LoginStatus::LoggedOut);
        }
        if s.tokens.contains_key("login_expired") {
            return Ok(LoginStatus::Expired);
        }
        Ok(LoginStatus::LoggedIn {
            user_id: s.user_id,
            display_name: s.display_name,
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
        provider.mark_session_expired();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        let mut cookies = BTreeMap::new();
        cookies.insert("MUSIC_U".into(), "cookie".into());
        provider.set_cookies(cookies, None, None).unwrap();
        provider.mark_session_expired();
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);
        provider.logout().await.unwrap();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
    }
}
