//! QQ Music provider migrated from VoiceFox's `lx-source::tx`.
//! Protocol origin: lx-music `musicSdk/tx/`. Search, song detail, stream URL
//! resolution, lyrics, playlists, albums, charts, link parsing, QQ scan login
//! and cookie-backed session status are implemented. Artist lookup has no
//! upstream endpoint and stays Unsupported; the WeChat login channel and QRC
//! 3DES lyrics decryption remain pending (see docs/SOURCE_QQ_MUSIC_AUDIT.md).
//!
//! Request signing: every `musicu.fcg` call is unsigned except lyrics, which
//! uses `zzc_sign` (SHA1 + index picking + XOR + base64).
use crate::{
    Error, Result,
    lrc::parse_lrc,
    models::{
        Album, AudioFormat, Chart, ChartEntry, LyricDocument, LyricFormat, Playlist,
        PlaylistCategory, StreamInfo, StreamQuality, Track, TrackId,
    },
    provider::{
        AlbumProvider, AuthenticationProvider, ChartProvider, CollectionProvider, LinkResolver,
        LoginStatus, LyricsProvider, MusicProvider, PlaylistProvider, QrAuthenticationProvider,
        QrLoginPoll, QrLoginSession, QrLoginStatus, ResolveOptions, ResolvedLink, SearchPage,
        SearchQuery, StreamResolver, TrackSearch,
    },
    retry::{QQ_CODES, StepOutcome, classify_api, classify_http},
    session::{ProviderSession, SessionStore},
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, RequestBuilder, header, redirect};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{collections::BTreeMap, time::Duration};

const ID: &str = "qqmusic";
const UA_WEB: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0 Safari/537.36";
const UA_ANDROID: &str = "QQMusic 14090508(android 12)";
const REFERER: &str = "https://y.qq.com/";
const MUSICU_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const SEARCH_URL: &str = "https://c.y.qq.com/soso/fcgi-bin/client_search_cp";
const SONG_DETAIL_URL: &str = "https://c.y.qq.com/v8/fcg-bin/fcg_play_single_song.fcg";
const DISS_TAG_CONF_URL: &str = "https://c.y.qq.com/splcloud/fcgi-bin/fcg_get_diss_tag_conf.fcg";
const DISS_BY_TAG_URL: &str = "https://c.y.qq.com/splcloud/fcgi-bin/fcg_get_diss_by_tag.fcg";
const PLAYLIST_SEARCH_URL: &str = "https://c.y.qq.com/soso/fcgi-bin/client_music_search_songlist";
const CREATED_DISS_URL: &str = "https://c.y.qq.com/rsc/fcgi-bin/fcg_user_created_diss";
const ORDER_ASSET_URL: &str = "https://c.y.qq.com/fav/fcgi-bin/fcg_get_profile_order_asset.fcg";
const TOPLIST_URL: &str = "https://c.y.qq.com/v8/fcg-bin/fcg_myqq_toplist.fcg";
/// Stream URL TTL is not reported upstream; resolved URLs are simply re-fetched when stale.
const STREAM_QUERY: &str = "guid={guid}&vkey={vkey}&uin=0&fromtag=120093";
/// Playlist detail pages: the API caps a single page at 1000 songs.
const DETAIL_PAGE_SIZE: u64 = 1000;
const DETAIL_MAX_PAGES: u64 = 10;
/// Album song list page size for full-track pagination.
const ALBUM_PAGE_SIZE: u64 = 100;
const ALBUM_MAX_PAGES: u64 = 20;

// QQ scan-login channel constants, snapshot of the y.qq.com web flow.
const LOGIN_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const LOGIN_REFERER: &str = "https://xui.ptlogin2.qq.com/";
const XLOGIN_URL: &str = "https://xui.ptlogin2.qq.com/cgi-bin/xlogin";
const QR_SHOW_URL: &str = "https://xui.ptlogin2.qq.com/ssl/ptqrshow";
const QR_CHECK_URL: &str = "https://xui.ptlogin2.qq.com/ssl/ptqrlogin";
const OAUTH_JUMP: &str = "https://graph.qq.com/oauth2.0/login_jump";
const OAUTH_CLIENT_ID: &str = "100497308";
const QQ_APPID: &str = "716027609";
/// OAuth redirect hops to walk while collecting login cookies.
const MAX_REDIRECTS: usize = 10;
/// Session-token key holding the warmup cookies a QR poll needs.
const QR_PENDING_KEY: &str = "qr_pending_cookies";
// WeChat scan-login channel (y.qq.com web flow).
const WX_APP_ID: &str = "wx48db31d50e334801";
const WX_QR_CONNECT_URL: &str = "https://open.weixin.qq.com/connect/qrconnect";
const WX_QRCODE_URL: &str = "https://open.weixin.qq.com/connect/qrcode";
const WX_POLL_URL: &str = "https://lp.open.weixin.qq.com/connect/l/qrconnect";
const WX_REDIRECT_URI: &str =
    "https://y.qq.com/portal/wx_redirect.html?login_type=2&surl=https%3A%2F%2Fy.qq.com%2F";
const WX_KEY_PREFIX: &str = "wx:";
const WX_EXPIRES_IN: u64 = 300;
/// WeChat long-poll blocks server-side for ~15s; the request timeout must cover it.
const WX_POLL_TIMEOUT: Duration = Duration::from_secs(35);
/// Session-token prefix marking a WeChat uuid that already showed "scanned".
const WX_SEEN_SCAN_PREFIX: &str = "wx_seen_scan_";
const WX_MUSICU_FALLBACKS: [&str; 2] = [
    "https://szu.y.qq.com/cgi-bin/musicu.fcg",
    "https://shu.y.qq.com/cgi-bin/musicu.fcg",
];

#[derive(Clone)]
pub struct QqMusicProvider {
    client: Client,
    /// Redirects are followed manually during QR login so every hop's
    /// Set-Cookie headers can be harvested.
    login_client: Client,
    sessions: SessionStore,
}
impl std::fmt::Debug for QqMusicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QqMusicProvider")
            .field("session_store", &"redacted")
            .finish()
    }
}
impl Default for QqMusicProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl QqMusicProvider {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| Client::new());
        let login_client = Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            login_client,
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
    /// Register all currently implemented QQ Music capabilities in one step.
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
    fn mark_session_expired(&self) {
        if let Some(mut session) = self.sessions.get(ID) {
            session.tokens.insert("login_expired".into(), "1".into());
            let _ = self.sessions.set(ID, session);
        }
    }
    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        let mut request = request
            .header(header::REFERER, REFERER)
            .header(header::USER_AGENT, UA_WEB);
        if let Some(session) = self.sessions.get(ID) {
            let cookie = session
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            if !cookie.is_empty() {
                request = request.header(header::COOKIE, cookie);
            }
        }
        request
    }
    /// GET with the shared retry policy; returns the final 2xx response.
    async fn raw_get(&self, url: &str, label: &str) -> Result<reqwest::Response> {
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
                    tracing::debug!("QQ {label} transport retry {}: {error}", attempt + 1);
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
            return Ok(response);
        }
        Err(perr(format!("{label}: retry budget exhausted")))
    }
    async fn get_json(&self, url: &str, label: &str) -> Result<Value> {
        let response = self.raw_get(url, label).await?;
        let value: Value = response
            .json()
            .await
            .map_err(|e| perr(format!("{label}: {e}")))?;
        // The API-level classification has no retry semantics for QQ (no
        // throttled code), so attempt == budget is always correct here.
        let budget = 0;
        let message = value["message"]
            .as_str()
            .or_else(|| value["msg"].as_str())
            .unwrap_or("unknown error");
        match classify_api(
            value["code"].as_i64().unwrap_or(QQ_CODES.ok),
            message,
            budget,
            budget,
            &QQ_CODES,
        ) {
            StepOutcome::Ok => Ok(value),
            StepOutcome::Retry => Err(perr(format!("{label}: throttled"))),
            StepOutcome::SessionExpired => {
                self.mark_session_expired();
                Err(perr(format!("{label}: login required or session expired")))
            }
            StepOutcome::Fatal(m) => Err(perr(format!("{label}: {m}"))),
        }
    }
    /// GET endpoints that answer with JSONP or plain JSON; the callback shell
    /// is stripped. API-level `code` checks stay at the call sites because the
    /// legacy fcg endpoints vary in shape.
    async fn get_jsonp(&self, url: &str, label: &str) -> Result<Value> {
        let response = self.raw_get(url, label).await?;
        let text = response
            .text()
            .await
            .map_err(|e| perr(format!("{label}: {e}")))?;
        serde_json::from_str(&strip_jsonp(&text)).map_err(|e| {
            let snippet: String = text.chars().take(120).collect();
            perr(format!("{label}: {e}; body head: {snippet}"))
        })
    }
    async fn musicu(&self, body: &Value, label: &str) -> Result<Value> {
        let text = serde_json::to_string(body)?;
        self.musicu_inner(MUSICU_URL, &text, UA_WEB, label).await
    }
    /// The lyrics endpoint is the only signed QQ call: `?sign=zzc_sign(body)`.
    async fn musicu_signed(&self, body: &Value, label: &str) -> Result<Value> {
        let text = serde_json::to_string(body)?;
        let url = format!("{MUSICU_URL}?sign={}", zzc_sign(&text));
        self.musicu_inner(&url, &text, UA_ANDROID, label).await
    }
    async fn musicu_inner(
        &self,
        url: &str,
        body: &str,
        user_agent: &str,
        label: &str,
    ) -> Result<Value> {
        const BACKOFF_MS: [u64; 3] = [400, 900, 1_800];
        for (attempt, backoff_ms) in BACKOFF_MS
            .iter()
            .copied()
            .chain(std::iter::once(0))
            .enumerate()
        {
            let response = self
                .request(self.client.post(url))
                .header(header::USER_AGENT, user_agent)
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_owned())
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) if attempt < BACKOFF_MS.len() => {
                    tracing::debug!("QQ {label} transport retry {}: {error}", attempt + 1);
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    continue;
                }
                Err(error) => return Err(perr(format!("{label}: {error}"))),
            };
            match classify_http(response.status().as_u16(), attempt, BACKOFF_MS.len()) {
                StepOutcome::Retry => {
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    continue;
                }
                StepOutcome::Fatal(message) => return Err(perr(format!("{label}: {message}"))),
                StepOutcome::SessionExpired | StepOutcome::Ok => {}
            }
            let value: Value = response
                .json()
                .await
                .map_err(|e| perr(format!("{label}: {e}")))?;
            let message = value["message"]
                .as_str()
                .or_else(|| value["msg"].as_str())
                .unwrap_or("unknown error");
            match classify_api(
                value["code"].as_i64().unwrap_or(QQ_CODES.ok),
                message,
                attempt,
                BACKOFF_MS.len(),
                &QQ_CODES,
            ) {
                StepOutcome::Ok => return Ok(value),
                StepOutcome::Retry => {
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
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
    async fn fetch_song(&self, query: &str, label: &str) -> Result<Track> {
        let url = format!("{SONG_DETAIL_URL}?{query}&format=json");
        let value = self.get_json(&url, label).await?;
        value["data"]
            .as_array()
            .and_then(|v| v.first())
            .and_then(parse_song)
            .ok_or(Error::NotFound)
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}
/// lx-music "zzc" signature: SHA1 of the body, digits picked from the hex
/// digest by (0-based) index with out-of-range indexes skipped (index 40 is
/// past the end of a 40-char digest), XOR of digest byte pairs against a
/// fixed table, base64 with `\`, `/`, `+`, `=` stripped, then lowercased.
fn zzc_sign(body: &str) -> String {
    const PICK1: [usize; 8] = [23, 14, 6, 36, 16, 40, 7, 19];
    const PICK2: [usize; 8] = [16, 1, 32, 12, 19, 27, 8, 5];
    const SCRAMBLE: [u8; 20] = [
        89, 39, 179, 150, 218, 82, 58, 252, 177, 52, 186, 123, 120, 64, 242, 133, 143, 161, 121,
        179,
    ];
    let digest = format!("{:x}", Sha1::digest(body.as_bytes()));
    let pick = |indexes: &[usize]| {
        indexes
            .iter()
            .filter_map(|&i| digest.chars().nth(i))
            .collect::<String>()
    };
    let part1 = pick(&PICK1);
    let part2 = pick(&PICK2);
    let scrambled: Vec<u8> = SCRAMBLE
        .iter()
        .enumerate()
        .map(|(i, byte)| byte ^ u8::from_str_radix(&digest[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect();
    let part3: String = STANDARD
        .encode(scrambled)
        .chars()
        .filter(|c| !matches!(c, '\\' | '/' | '+' | '='))
        .collect();
    format!("zzc{part1}{part3}{part2}").to_lowercase()
}
/// Quality-tier filename prefixes: `prefix + media_mid + extension`.
const HIRES_TIER: [(&str, StreamQuality, AudioFormat); 7] = [
    ("AI00", StreamQuality::HiRes, AudioFormat::Flac),
    ("Q001", StreamQuality::HiRes, AudioFormat::Flac),
    ("Q000", StreamQuality::Lossless, AudioFormat::Flac),
    ("F000", StreamQuality::Lossless, AudioFormat::Flac),
    ("O801", StreamQuality::Lossless, AudioFormat::Ogg),
    ("M800", StreamQuality::High, AudioFormat::Mp3),
    ("M500", StreamQuality::Low, AudioFormat::Mp3),
];
const LOSSLESS_TIER: [(&str, StreamQuality, AudioFormat); 6] = [
    ("Q001", StreamQuality::HiRes, AudioFormat::Flac),
    ("Q000", StreamQuality::Lossless, AudioFormat::Flac),
    ("F000", StreamQuality::Lossless, AudioFormat::Flac),
    ("O801", StreamQuality::Lossless, AudioFormat::Ogg),
    ("M800", StreamQuality::High, AudioFormat::Mp3),
    ("M500", StreamQuality::Low, AudioFormat::Mp3),
];
const HIGH_TIER: [(&str, StreamQuality, AudioFormat); 2] = [
    ("M800", StreamQuality::High, AudioFormat::Mp3),
    ("M500", StreamQuality::Low, AudioFormat::Mp3),
];
const LOW_TIER: [(&str, StreamQuality, AudioFormat); 1] =
    [("M500", StreamQuality::Low, AudioFormat::Mp3)];
fn tier(q: StreamQuality) -> &'static [(&'static str, StreamQuality, AudioFormat)] {
    match q {
        StreamQuality::HiRes => &HIRES_TIER,
        StreamQuality::Lossless => &LOSSLESS_TIER,
        StreamQuality::High => &HIGH_TIER,
        _ => &LOW_TIER,
    }
}
fn extension(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Flac => "flac",
        AudioFormat::Ogg => "ogg",
        AudioFormat::Mp3 => "mp3",
        _ => "mp3",
    }
}
/// Track extras whose value mirrors the chosen prefix; sizes are advisory.
fn size_key(prefix: &str) -> &'static str {
    match prefix {
        "M500" => "size_128mp3",
        "M800" => "size_320mp3",
        "Q000" | "F000" => "size_flac",
        "Q001" | "AI00" | "O801" => "size_hires",
        _ => "size_128mp3",
    }
}
fn vkey_request_body(mid: &str, filenames: &[String], guid: u32) -> Value {
    json!({
        "req_0": {
            "module": "vkey.GetVkeyServer",
            "method": "CgiGetVkey",
            "param": {
                "guid": guid,
                "songmid": [mid],
                "songtype": [0],
                "uin": "0",
                "loginflag": 1,
                "platform": "20",
                "filename": filenames,
            },
        },
        "comm": {"uin": 0, "format": "json", "ct": 24, "cv": 0},
    })
}
/// One requested stream variant: filename, mapped quality, format and the
/// track-extra key holding its advisory size.
type StreamCandidate = (String, StreamQuality, AudioFormat, &'static str);
/// A matched entry: candidate plus the response's sip, purl and vkey parts.
type PickedStream<'a> = (&'a StreamCandidate, String, String, String);
/// Match response entries against requested filenames; empty `sip`, `purl` or
/// `vkey` entries are skipped (empty sip is how QQ signals "no playable URL").
fn pick_stream<'a>(value: &Value, candidates: &'a [StreamCandidate]) -> Option<PickedStream<'a>> {
    let sip = value["req_0"]["data"]["sip"]
        .as_array()?
        .first()?
        .as_str()
        .filter(|s| !s.trim().is_empty())?;
    for entry in value["req_0"]["data"]["midurlinfo"].as_array()? {
        let filename = entry["filename"].as_str()?;
        let Some(purl) = entry["purl"].as_str().filter(|p| !p.trim().is_empty()) else {
            continue;
        };
        let Some(vkey) = entry["vkey"].as_str().filter(|v| !v.trim().is_empty()) else {
            continue;
        };
        if let Some(candidate) = candidates.iter().find(|(file, ..)| file == filename) {
            return Some((candidate, sip.to_owned(), purl.to_owned(), vkey.to_owned()));
        }
    }
    None
}
fn parse_song(item: &Value) -> Option<Track> {
    let mid = item["mid"].as_str()?.trim();
    if mid.is_empty() {
        return None;
    }
    let title = item["title"]
        .as_str()
        .or_else(|| item["name"].as_str())
        .unwrap_or_default()
        .trim();
    if title.is_empty() {
        return None;
    }
    let singers = item["singer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["name"].as_str().map(str::to_owned))
        .collect();
    let album_mid = item["album"]["mid"].as_str().map(str::to_owned);
    let artwork_url = album_mid
        .as_deref()
        .map(|m| format!("https://y.gtimg.cn/music/photo_new/T002R500x500M000{m}.jpg"));
    let duration = item["interval"].as_u64().map(Duration::from_secs);
    let file = &item["file"];
    let media_mid = file["media_mid"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(mid)
        .to_owned();
    let mut qualities = vec![StreamQuality::Low];
    if file["size_320mp3"].as_u64().unwrap_or(0) > 0 {
        qualities.push(StreamQuality::High);
    }
    if file["size_flac"].as_u64().unwrap_or(0) > 0 {
        qualities.push(StreamQuality::Lossless);
    }
    if file["size_hires"].as_u64().unwrap_or(0) > 0 {
        qualities.push(StreamQuality::HiRes);
    }
    let mut extra = BTreeMap::new();
    if let Some(song_id) = item["id"].as_i64() {
        extra.insert("songId".into(), song_id.to_string());
    }
    extra.insert("strMediaMid".into(), media_mid);
    for key in ["size_128mp3", "size_320mp3", "size_flac", "size_hires"] {
        if let Some(size) = file[key].as_u64().filter(|s| *s > 0) {
            extra.insert(key.into(), size.to_string());
        }
    }
    Some(Track {
        id: TrackId::new(ID, mid),
        title: title.into(),
        artists: singers,
        album: item["album"]["name"].as_str().map(str::to_owned),
        album_id: album_mid,
        duration,
        artwork_url,
        isrc: None,
        available_qualities: qualities,
        extra,
    })
}
fn decode_b64(value: &Value) -> Option<String> {
    let raw = value.as_str()?;
    if raw.trim().is_empty() {
        return None;
    }
    STANDARD
        .decode(raw)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|text| !text.trim().is_empty())
}
/// Case-preserving link parsing: markers are located in the lowercased copy,
/// values are sliced from the original so song mids keep their casing.
fn param_value(hay_lower: &str, hay: &str, key: &str) -> Option<String> {
    let mut from = 0;
    loop {
        let pos = hay_lower[from..].find(key)? + from;
        let boundary = pos == 0
            || matches!(
                hay_lower.as_bytes().get(pos.wrapping_sub(1)),
                Some(b'?') | Some(b'&') | Some(b'/')
            );
        if boundary {
            let rest = &hay[pos + key.len()..];
            let rest = rest.strip_prefix('=').unwrap_or(rest);
            let end = rest.find(['&', '#']).unwrap_or(rest.len());
            return Some(rest[..end].to_owned()).filter(|v| !v.is_empty());
        }
        from = pos + 1;
    }
}
fn path_value(hay_lower: &str, hay: &str, marker: &str) -> Option<String> {
    let pos = hay_lower.find(marker)?;
    let rest = &hay[pos + marker.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(rest.len());
    Some(rest[..end].to_owned()).filter(|v| !v.is_empty())
}
fn parse_link_target(url: &str) -> Option<(String, String)> {
    let lower = url.to_ascii_lowercase();
    if let Some(rest) = lower.split_once("://").map(|(_, rest)| rest) {
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = match host.rsplit_once(':') {
            Some((h, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => h,
            _ => host,
        };
        if host != "y.qq.com" && !host.ends_with(".qq.com") {
            return None;
        }
    }
    let fragment = url.split_once('#').map(|(_, f)| f).unwrap_or("");
    let fragment_lower = fragment.to_ascii_lowercase();
    for (hay_lower, hay) in [(fragment_lower.as_str(), fragment), (lower.as_str(), url)] {
        if let Some(mid) = path_value(hay_lower, hay, "/songdetail/")
            .or_else(|| param_value(hay_lower, hay, "songmid"))
            .filter(|mid| mid.chars().all(|c| c.is_ascii_alphanumeric()))
        {
            return Some(("song".into(), mid));
        }
        if let Some(mid) = path_value(hay_lower, hay, "/albumdetail/")
            .filter(|mid| mid.chars().all(|c| c.is_ascii_alphanumeric()))
        {
            return Some(("album".into(), mid));
        }
        if let Some(id) = param_value(hay_lower, hay, "songid")
            .filter(|id| id.chars().all(|c| c.is_ascii_digit()))
        {
            return Some(("songid".into(), id));
        }
        for marker in ["/playlist/", "/taoge/"] {
            if let Some(id) = path_value(hay_lower, hay, marker)
                .filter(|id| id.chars().all(|c| c.is_ascii_digit()))
            {
                return Some(("playlist".into(), id));
            }
        }
        if let Some(id) = param_value(hay_lower, hay, "disstid")
            .or_else(|| param_value(hay_lower, hay, "dissid"))
            .filter(|id| id.chars().all(|c| c.is_ascii_digit()))
        {
            return Some(("playlist".into(), id));
        }
        if let Some(id) =
            param_value(hay_lower, hay, "id").filter(|id| id.chars().all(|c| c.is_ascii_digit()))
        {
            return Some(("playlist".into(), id));
        }
    }
    None
}
impl MusicProvider for QqMusicProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        "QQ音乐"
    }
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            lyrics: true,
            authentication: true,
            playlists: true,
            albums: true,
            charts: true,
            link_resolve: true,
            ..Default::default()
        }
    }
}
#[async_trait]
impl TrackSearch for QqMusicProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.keyword.trim().is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        let page = q.page.max(1);
        let limit = q.page_size.clamp(1, 100);
        let url = format!(
            "{SEARCH_URL}?p={page}&n={limit}&w={}&format=json&new_json=1&cr=1&aggr=1&lossless=1",
            urlencoding::encode(q.keyword.trim())
        );
        let json = self.get_json(&url, "search").await?;
        let song = &json["data"]["song"];
        let total = song["totalnum"].as_u64();
        let items = song["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_song)
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
impl StreamResolver for QqMusicProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to QQ Music".into(),
            ));
        }
        let media_mid = track
            .extra
            .get("strMediaMid")
            .cloned()
            .unwrap_or_else(|| track.id.id.clone());
        let candidates = tier(options.quality)
            .iter()
            .map(|(prefix, quality, format)| {
                (
                    format!("{prefix}{media_mid}.{}", extension(*format)),
                    *quality,
                    *format,
                    size_key(prefix),
                )
            })
            .collect::<Vec<_>>();
        let body = vkey_request_body(
            &track.id.id,
            &candidates
                .iter()
                .map(|(file, ..)| file.clone())
                .collect::<Vec<_>>(),
            std::process::id(),
        );
        let value = self.musicu(&body, "stream resolve").await?;
        let Some((candidate, sip, purl, vkey)) = pick_stream(&value, &candidates) else {
            let module_code = value["req_0"]["code"].as_i64();
            return Err(perr(format!(
                "no playable stream URL returned by QQ Music (module code {module_code:?}; VIP or risk-controlled content requires a session cookie)"
            )));
        };
        let query = STREAM_QUERY
            .replace("{guid}", &std::process::id().to_string())
            .replace("{vkey}", &vkey);
        Ok(StreamInfo {
            url: format!("{sip}{purl}?{query}"),
            quality: candidate.1,
            format: candidate.2,
            duration: track.duration,
            expires_at_unix: None,
            content_length: track.extra.get(candidate.3).and_then(|s| s.parse().ok()),
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
impl LyricsProvider for QqMusicProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to QQ Music".into(),
            ));
        }
        let song_id: i64 = track
            .extra
            .get("songId")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Error::InvalidInput("track has no numeric song id".into()))?;
        let body = json!({
            "comm": {
                "ct": "11", "cv": "14090508", "v": "14090508",
                "tmeAppID": "qqmusic", "OpenUDID": "0",
                "tmeLoginType": "0", "uin": "0",
            },
            "req": {
                "module": "music.musichallSong.PlayLyricInfo",
                "method": "GetPlayLyricInfo",
                // crypt/qrc 0 keeps the response as plain base64 LRC; crypt:1
                // returns a hex blob that would need 3DES QRC decryption.
                "param": {"songID": song_id, "crypt": 0, "qrc": 0, "trans": 1, "roma": 1},
            },
        });
        let value = self.musicu_signed(&body, "lyrics").await?;
        let data = &value["req"];
        // Non-zero module code means "no lyrics", not an API failure.
        if data["code"].as_i64().unwrap_or(0) != 0 {
            return Ok(None);
        }
        let Some(original) = decode_b64(&data["data"]["lyric"]) else {
            return Ok(None);
        };
        Ok(Some(LyricDocument {
            format: LyricFormat::Lrc,
            lines: parse_lrc(&original),
            translation: decode_b64(&data["data"]["trans"]),
            romanization: decode_b64(&data["data"]["roma"]),
            original,
        }))
    }
}
#[async_trait]
impl LinkResolver for QqMusicProvider {
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink> {
        let (kind, id) = parse_link_target(url)
            .ok_or_else(|| Error::InvalidInput("unrecognized QQ Music link".into()))?;
        match kind.as_str() {
            "song" => Ok(ResolvedLink::Track(
                self.fetch_song(&format!("songmid={id}"), "song link")
                    .await?,
            )),
            "songid" => Ok(ResolvedLink::Track(
                self.fetch_song(&format!("songid={id}"), "song link")
                    .await?,
            )),
            "playlist" => Ok(ResolvedLink::Playlist(self.playlist(&id).await?)),
            "album" => Ok(ResolvedLink::Album(self.album(&id).await?)),
            _ => Err(Error::NotFound),
        }
    }
}
fn extract_wx_uuid(html: &str) -> Option<String> {
    const MARKERS: [&str; 2] = ["connect/l/qrconnect?uuid=", "/connect/qrcode/"];
    for marker in MARKERS {
        if let Some(rest) = html.split(marker).nth(1) {
            let uuid: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if !uuid.is_empty() {
                return Some(uuid);
            }
        }
    }
    None
}
/// Parse `window.wx_errcode=408;window.wx_code='';`.
fn parse_wx_poll(body: &str) -> Option<(String, String)> {
    let errcode: String = body
        .split("wx_errcode=")
        .nth(1)?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if errcode.is_empty() {
        return None;
    }
    let code = body
        .split("wx_code='")
        .nth(1)
        .and_then(|rest| rest.split('\'').next())
        .unwrap_or_default()
        .to_owned();
    Some((errcode, code))
}
/// WeChat account id inside QQ Music: `str_musicid` is a string, `musicid`
/// may be a number; both shapes have been observed live.
fn wechat_uin(data: &Value) -> Option<String> {
    if let Some(value) = data["str_musicid"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return Some(value.to_owned());
    }
    match &data["musicid"] {
        Value::String(value) if !value.trim().is_empty() => Some(value.trim().to_owned()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}
/// Success criteria: outer code 0, req.code 0, non-empty musickey (W_X_...).
/// Identity cookies are written under both spellings because different QQ
/// Music endpoints read different names.
fn wechat_cookies(response: &Value) -> Option<BTreeMap<String, String>> {
    if response["code"].as_i64() != Some(0) || response["req"]["code"].as_i64() != Some(0) {
        return None;
    }
    let data = &response["req"]["data"];
    let musickey = data["musickey"].as_str()?.trim();
    if musickey.is_empty() {
        return None;
    }
    let mut cookies = BTreeMap::new();
    cookies.insert("login_type".into(), "2".into());
    if let Some(uin) = wechat_uin(data) {
        cookies.insert("uin".into(), uin.clone());
        cookies.insert("wxuin".into(), uin);
    }
    for name in ["qqmusic_key", "qm_keyst"] {
        cookies.insert(name.into(), musickey.to_owned());
    }
    for (cookie, field) in [
        ("wxopenid", "openid"),
        ("wxunionid", "unionid"),
        ("wxaccess_token", "access_token"),
        ("wxrefresh_token", "refresh_token"),
    ] {
        if let Some(value) = data[field]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            cookies.insert(cookie.into(), value.into());
        }
    }
    Some(cookies)
}
impl QqMusicProvider {
    /// Create a WeChat-channel QR login session. The poll key carries the
    /// `wx:` prefix so `poll_qr_login` dispatches to the WeChat flow; the QR
    /// image itself is a JPEG data URL (WeChat codes cannot be re-rendered).
    pub async fn create_wechat_login(&self) -> Result<QrLoginSession> {
        let state_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let connect_url = format!(
            "{WX_QR_CONNECT_URL}?appid={WX_APP_ID}&redirect_uri={}&response_type=code&scope=snsapi_login&state={state_ms}",
            urlencoding::encode(WX_REDIRECT_URI)
        );
        let html = self
            .client
            .get(&connect_url)
            .header(header::USER_AGENT, LOGIN_UA)
            .header(header::REFERER, "https://y.qq.com/")
            .send()
            .await
            .map_err(|e| perr(format!("WeChat QR page request failed: {e}")))?
            .text()
            .await
            .map_err(|e| perr(format!("WeChat QR page read failed: {e}")))?;
        let uuid =
            extract_wx_uuid(&html).ok_or_else(|| perr("WeChat QR page did not contain a uuid"))?;
        let jpeg = self
            .client
            .get(format!("{WX_QRCODE_URL}/{uuid}"))
            .header(header::USER_AGENT, LOGIN_UA)
            .header(header::REFERER, "https://y.qq.com/")
            .send()
            .await
            .map_err(|e| perr(format!("WeChat QR image request failed: {e}")))?
            .bytes()
            .await
            .map_err(|e| perr(format!("WeChat QR image read failed: {e}")))?;
        if jpeg.is_empty() {
            return Err(perr("WeChat QR image is empty"));
        }
        self.set_wx_seen_scan(&uuid, false);
        Ok(QrLoginSession {
            key: format!("{WX_KEY_PREFIX}{uuid}"),
            url: format!("data:image/jpeg;base64,{}", STANDARD.encode(jpeg)),
            expires_in: WX_EXPIRES_IN,
        })
    }
    async fn poll_wechat(&self, uuid: &str) -> Result<QrLoginPoll> {
        let uuid = uuid.trim();
        if uuid.is_empty() {
            return Err(Error::InvalidInput("WeChat QR uuid is empty".into()));
        }
        let seen_scan = self.wx_seen_scan(uuid);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let last = if seen_scan { "&last=404" } else { "" };
        let poll_url = format!("{WX_POLL_URL}?uuid={uuid}&_={nonce}{last}");
        let body = self
            .client
            .get(&poll_url)
            .timeout(WX_POLL_TIMEOUT)
            .header(header::USER_AGENT, LOGIN_UA)
            .header(header::REFERER, "https://y.qq.com/")
            .send()
            .await
            .map_err(|e| perr(format!("WeChat poll request failed: {e}")))?
            .text()
            .await
            .map_err(|e| perr(format!("WeChat poll response read failed: {e}")))?;
        let Some((errcode, wx_code)) = parse_wx_poll(&body) else {
            let excerpt: String = body.trim().chars().take(80).collect();
            return Ok(QrLoginPoll {
                status: QrLoginStatus::Failed,
                message: format!("WeChat poll parse failed: {excerpt}"),
                user_id: None,
                display_name: None,
            });
        };
        // Only the "scanned" state carries last=404 forward; terminal states reset it.
        self.set_wx_seen_scan(uuid, errcode == "404");
        let (status, message, exchange_code) = match errcode.as_str() {
            "408" => (QrLoginStatus::Waiting, "等待扫码".to_owned(), None),
            "404" => (
                QrLoginStatus::Scanned,
                "已扫码，请在微信中确认".to_owned(),
                None,
            ),
            "402" => (
                QrLoginStatus::Expired,
                "微信二维码已过期，请重新生成".to_owned(),
                None,
            ),
            "403" => (QrLoginStatus::Failed, "微信登录被拒绝".to_owned(), None),
            "405" => {
                let code = wx_code.trim().to_owned();
                if code.is_empty() {
                    (
                        QrLoginStatus::Failed,
                        "微信已确认但没有返回 code".to_owned(),
                        None,
                    )
                } else {
                    (QrLoginStatus::Success, "登录成功".to_owned(), Some(code))
                }
            }
            other => (
                QrLoginStatus::Failed,
                format!("微信登录失败（{other}）"),
                None,
            ),
        };
        if let Some(code) = exchange_code {
            let response = self.exchange_wechat(&code).await?;
            let Some(cookies) = wechat_cookies(&response) else {
                let outer = response["code"].as_i64();
                let inner = response["req"]["code"].as_i64();
                let failure = if outer == Some(0) && inner == Some(0) {
                    "微信登录成功但未拿到音乐凭证，请重新扫码".to_owned()
                } else {
                    match inner.or(outer) {
                        Some(code) => format!("微信登录失败（服务端返回 {code}）"),
                        None => "微信登录失败".to_owned(),
                    }
                };
                return Ok(QrLoginPoll {
                    status: QrLoginStatus::Failed,
                    message: failure,
                    user_id: None,
                    display_name: None,
                });
            };
            let user_id = cookies.get("uin").or_else(|| cookies.get("wxuin")).cloned();
            let mut session = self.sessions.get(ID).unwrap_or_default();
            session.cookies = cookies;
            session.user_id = user_id.clone();
            self.sessions.set(ID, session)?;
            return Ok(QrLoginPoll {
                status: QrLoginStatus::Success,
                message: "登录成功".to_owned(),
                user_id,
                // The exchange response carries no observed nickname field.
                display_name: None,
            });
        }
        Ok(QrLoginPoll {
            status,
            message,
            user_id: None,
            display_name: None,
        })
    }
    /// Exchange a confirmed wx_code for musickey cookies. Deliberately only
    /// sends `login_type=2` so existing QQ-channel cookies are not mixed in.
    async fn exchange_wechat(&self, code: &str) -> Result<Value> {
        let body = json!({
            "comm": {
                "tmeAppID": "qqmusic", "tmeLoginType": "1", "g_tk": 5381,
                "platform": "yqq", "ct": 24, "cv": 0,
            },
            "req": {
                "module": "music.login.LoginServer",
                "method": "Login",
                "param": {"strAppid": WX_APP_ID, "code": code},
            },
        });
        let payload = serde_json::to_string(&body)?;
        let mut last_error = None;
        for api in std::iter::once(MUSICU_URL).chain(WX_MUSICU_FALLBACKS) {
            let response = self
                .client
                .post(api)
                .header(header::USER_AGENT, LOGIN_UA)
                .header(header::REFERER, "https://y.qq.com/")
                .header(header::ORIGIN, "https://y.qq.com")
                .header(header::COOKIE, "login_type=2")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(payload.clone())
                .send()
                .await;
            match response {
                Ok(response) => {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    match serde_json::from_str::<Value>(&text) {
                        Ok(value) => return Ok(value),
                        Err(error) => {
                            let excerpt: String = text.trim().chars().take(80).collect();
                            last_error = Some(perr(format!(
                                "{api} returned {status} but JSON parse failed: {error}; body head: {excerpt}"
                            )));
                        }
                    }
                }
                Err(error) => last_error = Some(perr(format!("{api}: {error}"))),
            }
        }
        Err(last_error.unwrap_or_else(|| perr("WeChat credential exchange failed")))
    }
    fn wx_seen_scan(&self, uuid: &str) -> bool {
        self.sessions
            .get(ID)
            .and_then(|session| {
                session
                    .tokens
                    .get(&format!("{WX_SEEN_SCAN_PREFIX}{uuid}"))
                    .map(|value| value == "1")
            })
            .unwrap_or(false)
    }
    fn set_wx_seen_scan(&self, uuid: &str, seen: bool) {
        if let Some(mut session) = self.sessions.get(ID) {
            let key = format!("{WX_SEEN_SCAN_PREFIX}{uuid}");
            if seen {
                session.tokens.insert(key, "1".into());
            } else {
                session.tokens.remove(&key);
            }
            let _ = self.sessions.set(ID, session);
        }
    }
}
/// Cookie names that together prove a logged-in QQ Music session.
const IDENTITY_COOKIES: [&str; 2] = ["uin", "wxuin"];
const KEY_COOKIES: [&str; 2] = ["qqmusic_key", "qm_keyst"];

#[async_trait]
impl CollectionProvider for QqMusicProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist> {
        let disstid = numeric_disstid(id)?;
        let (playlist, _, _) = self.fetch_diss_page(disstid, 0).await?;
        Ok(playlist)
    }
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let disstid = numeric_disstid(id)?;
        let mut all = Vec::new();
        for page in 0..DETAIL_MAX_PAGES {
            let (_, songs, complete) = self
                .fetch_diss_page(disstid, page * DETAIL_PAGE_SIZE)
                .await?;
            let done = songs.is_empty() || complete;
            all.extend(songs);
            if done {
                break;
            }
        }
        Ok(all)
    }
}
fn numeric_disstid(id: &str) -> Result<u64> {
    let value = id
        .trim()
        .parse::<u64>()
        .map_err(|_| Error::InvalidInput("playlist id must be numeric".into()))?;
    if value == 0 {
        return Err(Error::InvalidInput("playlist id must be positive".into()));
    }
    Ok(value)
}
#[async_trait]
impl PlaylistProvider for QqMusicProvider {
    async fn categories(&self) -> Result<Vec<PlaylistCategory>> {
        let url = format!("{DISS_TAG_CONF_URL}?format=json&inCharset=utf8&outCharset=utf-8");
        let value = self.get_jsonp(&url, "playlist categories").await?;
        if value["code"].as_i64().unwrap_or_default() != 0 {
            return Err(perr("playlist categories request failed"));
        }
        Ok(parse_categories(&value))
    }
    async fn playlists(&self, category: &str, page: u32, page_size: u32) -> Result<Vec<Playlist>> {
        let limit = page_size.clamp(1, 100);
        let offset = limit.saturating_mul(page.max(1).saturating_sub(1));
        let category_id = if category.trim().is_empty() {
            "10000000"
        } else {
            category.trim()
        };
        let url = format!(
            "{DISS_BY_TAG_URL}?picmid=1&rnd=0.1&g_tk=5381&loginUin=0&hostUin=0&format=json&inCharset=utf8&outCharset=utf-8&notice=0&platform=yqq.json&needNewCode=0&categoryId={category_id}&sortId=5&sin={offset}&ein={}",
            offset + limit - 1
        );
        let value = self.get_jsonp(&url, "category playlists").await?;
        if value["code"].as_i64().unwrap_or_default() != 0 {
            return Err(perr("category playlists request failed"));
        }
        Ok(value["data"]["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_diss_by_tag_item)
            .collect())
    }
    async fn search_playlists(&self, keyword: &str, page: u32) -> Result<Vec<Playlist>> {
        if keyword.trim().is_empty() {
            return Err(Error::InvalidInput(
                "playlist search keyword is empty".into(),
            ));
        }
        let url = format!(
            "{PLAYLIST_SEARCH_URL}?query={}&page_no={}&num_per_page=20&format=json&remoteplace=txt.yqq.playlist&flag_qc=0",
            urlencoding::encode(keyword.trim()),
            page.max(1).saturating_sub(1)
        );
        let value = self.get_jsonp(&url, "playlist search").await?;
        Ok(value["data"]["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_diss_by_tag_item)
            .collect())
    }
    async fn user_playlists(&self, page: u32, page_size: u32) -> Result<Vec<Playlist>> {
        let uin = self.logged_in_uin()?;
        let limit = page_size.clamp(1, 100);
        let begin = limit.saturating_mul(page.max(1).saturating_sub(1));
        let created_url = format!(
            "{CREATED_DISS_URL}?hostUin=0&hostuin={uin}&sin={begin}&size={limit}&g_tk=5381&loginUin={uin}&format=json&inCharset=utf8&outCharset=utf-8&notice=0&platform=yqq.json&needNewCode=0"
        );
        let created = self
            .get_jsonp(&created_url, "user created playlists")
            .await?;
        if created["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(perr(
                "user created playlists request failed; login may have expired",
            ));
        }
        let collected_url = format!(
            "{ORDER_ASSET_URL}?format=json&inCharset=utf8&outCharset=utf-8&platform=yqq.json&needNewCode=0&loginUin={uin}&hostUin=0&notice=0&g_tk=5381&ct=20&cid=205360956&userid={uin}&reqtype=3&sin={begin}&ein={}",
            begin + limit - 1
        );
        let collected = self
            .get_jsonp(&collected_url, "user collected playlists")
            .await?;
        if collected["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(perr(
                "user collected playlists request failed; login may have expired",
            ));
        }
        let mut result = created_playlists(&created);
        for item in collected["data"]["cdlist"].as_array().into_iter().flatten() {
            if let Some(playlist) = parse_user_playlist(item)
                && !result.iter().any(|existing| existing.id == playlist.id)
            {
                result.push(playlist);
            }
        }
        Ok(result)
    }
}
#[async_trait]
impl AlbumProvider for QqMusicProvider {
    async fn album(&self, id: &str) -> Result<Album> {
        let album_mid = id.trim();
        if album_mid.is_empty() {
            return Err(Error::InvalidInput("album id is empty".into()));
        }
        let value = self.album_song_page(album_mid, 0, 1).await?;
        album_from_songlist(&value["albumSonglist"]["data"], album_mid).ok_or(Error::NotFound)
    }
    async fn album_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let album_mid = id.trim();
        if album_mid.is_empty() {
            return Err(Error::InvalidInput("album id is empty".into()));
        }
        let mut all = Vec::new();
        for page in 0..ALBUM_MAX_PAGES {
            let value = self
                .album_song_page(album_mid, page * ALBUM_PAGE_SIZE, ALBUM_PAGE_SIZE)
                .await?;
            let data = &value["albumSonglist"]["data"];
            let songs = data["songList"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| parse_song(&item["songInfo"]))
                .collect::<Vec<_>>();
            let total = data["totalNum"].as_u64();
            let done = songs.is_empty()
                || (songs.len() as u64) < ALBUM_PAGE_SIZE
                || total.is_some_and(|total| all.len() as u64 >= total);
            all.extend(songs);
            if done {
                break;
            }
        }
        if all.is_empty() {
            return Err(Error::NotFound);
        }
        Ok(all)
    }
}
impl QqMusicProvider {
    async fn album_song_page(&self, album_mid: &str, begin: u64, num: u64) -> Result<Value> {
        let body = json!({
            "comm": {"ct": 24, "cv": 10000},
            "albumSonglist": {
                "method": "GetAlbumSongList",
                "param": {"albumMid": album_mid, "albumID": 0, "begin": begin, "num": num, "order": 2},
                "module": "music.musichallAlbum.AlbumSongList",
            },
        });
        let value = self.musicu(&body, "album songs").await?;
        if value["albumSonglist"]["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(perr("album songs request failed"));
        }
        Ok(value)
    }
}
#[async_trait]
impl ChartProvider for QqMusicProvider {
    async fn charts(&self) -> Result<Vec<Chart>> {
        let url = format!(
            "{TOPLIST_URL}?g_tk=1928093487&inCharset=utf-8&outCharset=utf-8&notice=0&format=json&uin=0&needNewCode=1&platform=h5"
        );
        let value = self.get_jsonp(&url, "chart catalogue").await?;
        if value["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(perr("chart catalogue request failed"));
        }
        let boards = value["data"]["topList"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|board| board["id"].as_i64() != Some(201))
            .filter_map(parse_toplist_board)
            .collect::<Vec<_>>();
        if boards.is_empty() {
            return Err(Error::NotFound);
        }
        Ok(boards)
    }
    async fn chart_tracks(&self, id: &str) -> Result<Vec<ChartEntry>> {
        let topid: u64 = id
            .trim()
            .parse()
            .map_err(|_| Error::InvalidInput("chart id must be numeric".into()))?;
        // An empty period asks for the current chart issue (verified live);
        // pinning a period would require scraping the HTML leaderboard page.
        let body = json!({
            "toplist": {
                "module": "musicToplist.ToplistInfoServer",
                "method": "GetDetail",
                "param": {"topid": topid, "num": 300, "period": ""},
            },
            "comm": {"uin": 0, "format": "json", "ct": 20, "cv": 1859},
        });
        let value = self.musicu(&body, "chart tracks").await?;
        if value["toplist"]["code"].as_i64().unwrap_or(-1) != 0 {
            return Err(perr("chart tracks request failed"));
        }
        Ok(value["toplist"]["data"]["songInfoList"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_song)
            .enumerate()
            .map(|(index, track)| ChartEntry {
                rank: (index + 1) as u32,
                track,
            })
            .collect())
    }
}
fn parse_diss_by_tag_item(item: &Value) -> Option<Playlist> {
    let id = qq_value_string(&item["dissid"]);
    let title = item["dissname"].as_str()?.trim();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(Playlist {
        id: TrackId::new(ID, id),
        title: title.into(),
        description: non_empty(&item["introduction"]),
        artwork_url: non_empty(&item["imgurl"]).map(|url| url.replace("http://", "https://")),
        track_count: qq_value_u64(&item["song_count"])
            .or_else(|| qq_value_u64(&item["song_num"]))
            .map(|n| n as u32),
        tracks: vec![],
    })
}
/// Created and collected user playlists use different field spellings.
fn parse_user_playlist(item: &Value) -> Option<Playlist> {
    let id = {
        let id = qq_value_string(&item["dissid"]);
        if id.is_empty() {
            qq_value_string(&item["tid"])
        } else {
            id
        }
    };
    let title = item["dissname"]
        .as_str()
        .or_else(|| item["diss_name"].as_str())
        .or_else(|| item["title"].as_str())?
        .trim()
        .to_owned();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(Playlist {
        id: TrackId::new(ID, id),
        title,
        description: None,
        artwork_url: item["logo"]
            .as_str()
            .or_else(|| item["picurl"].as_str())
            .or_else(|| item["diss_cover"].as_str())
            .filter(|url| !url.is_empty())
            .map(|url| url.replace("http://", "https://")),
        track_count: qq_value_u64(&item["songnum"])
            .or_else(|| qq_value_u64(&item["song_count"]))
            .or_else(|| qq_value_u64(&item["song_cnt"]))
            .or_else(|| qq_value_u64(&item["song_num"]))
            .map(|n| n as u32),
        tracks: vec![],
    })
}
/// `dirid=201` is the favorites list (fetched separately) and `tid=0` items
/// are QZone background music or local uploads without a global id.
fn created_playlists(created: &Value) -> Vec<Playlist> {
    created["data"]["disslist"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            item["dirid"].as_u64() != Some(201)
                && qq_value_u64(&item["tid"]).unwrap_or_default() != 0
        })
        .filter_map(parse_user_playlist)
        .collect()
}
fn parse_categories(value: &Value) -> Vec<PlaylistCategory> {
    let mut result = vec![PlaylistCategory {
        id: "10000000".into(),
        name: "全部".into(),
        group: Some("全部".into()),
        count: 0,
        hot: true,
    }];
    for group in value["data"]["categories"].as_array().into_iter().flatten() {
        let group_name = group["categoryGroupName"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned();
        for item in group["items"].as_array().into_iter().flatten() {
            if item["usable"].as_i64().unwrap_or_default() == 0 {
                continue;
            }
            let id = item["categoryId"].as_i64().unwrap_or_default();
            let name = item["categoryName"].as_str().unwrap_or_default().trim();
            // 10000000 is the sentinel id for "all" categories.
            if id == 0 || id == 10_000_000 || name.is_empty() {
                continue;
            }
            result.push(PlaylistCategory {
                id: id.to_string(),
                name: name.into(),
                group: (!group_name.is_empty()).then(|| group_name.clone()),
                count: 0,
                hot: false,
            });
        }
    }
    result
}
fn parse_toplist_board(item: &Value) -> Option<Chart> {
    let id = item["id"].as_u64()?.to_string();
    let raw_title = item["topTitle"].as_str()?.trim();
    if raw_title.is_empty() {
        return None;
    }
    let mut title = raw_title
        .strip_prefix("巅峰榜·")
        .unwrap_or(raw_title)
        .to_owned();
    if !title.ends_with('榜') {
        title.push('榜');
    }
    Some(Chart {
        id: TrackId::new(ID, id),
        title,
        description: None,
    })
}
/// Album metadata rides on the first song's `songInfo.album` object plus the
/// list-level `totalNum` count; GetAlbumSongList has no dedicated album object.
fn album_from_songlist(data: &Value, album_mid: &str) -> Option<Album> {
    let first = data["songList"].as_array()?.first()?;
    let album_meta = &first["songInfo"]["album"];
    let title = album_meta["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())?;
    let artists = first["songInfo"]["singer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|singer| singer["name"].as_str().map(str::to_owned))
        .collect();
    Some(Album {
        id: TrackId::new(ID, album_mid.to_owned()),
        title: title.into(),
        artists,
        artwork_url: Some(format!(
            "https://y.gtimg.cn/music/photo_new/T002R500x500M000{album_mid}.jpg"
        )),
        track_count: data["totalNum"].as_u64().map(|n| n as u32),
    })
}
fn strip_jsonp(body: &str) -> String {
    let trimmed = body.trim();
    // Plain JSON passes through untouched: parentheses inside JSON strings
    // would otherwise trigger a bogus shell strip.
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return trimmed.to_owned();
    }
    match (trimmed.find('('), trimmed.rfind(')')) {
        (Some(start), Some(end)) if end > start => trimmed[start + 1..end].trim().to_owned(),
        _ => trimmed.to_owned(),
    }
}
fn non_empty(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}
fn qq_value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
        .unwrap_or_default()
}
fn qq_value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}
/// QQ's `hash33`: `h += (h << 5) + byte`, keep the low 31 bits.
fn hash33(value: &str) -> u32 {
    let mut hash: u32 = 0;
    for byte in value.bytes() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_add(hash)
            .wrapping_add(u32::from(byte));
    }
    hash & 0x7fff_ffff
}
/// Parse `ptuiCB('code','0','url','0','message','nickname')`.
fn parse_ptui_cb(raw: &str) -> Option<(String, String, String, String)> {
    let start = raw.find("ptuiCB(")?;
    let body = &raw[start + "ptuiCB(".len()..];
    let end = body.find(')')?;
    let parts = body[..end]
        .split(',')
        .map(|part| part.trim().trim_matches('\'').to_owned())
        .collect::<Vec<_>>();
    if parts.len() < 5 {
        return None;
    }
    Some((
        parts[0].clone(),
        parts[2].clone(),
        parts[4].clone(),
        parts.get(5).cloned().unwrap_or_default(),
    ))
}
fn status_from_code(code: &str) -> QrLoginStatus {
    match code {
        "0" => QrLoginStatus::Success,
        "65" => QrLoginStatus::Expired,
        "66" => QrLoginStatus::Waiting,
        "67" => QrLoginStatus::Scanned,
        _ => QrLoginStatus::Failed,
    }
}
fn resolve_location(current: &str, next: &str) -> String {
    if next.starts_with("http") {
        return next.to_owned();
    }
    let Some((scheme, rest)) = current.split_once("://") else {
        return next.to_owned();
    };
    let host = rest.split('/').next().unwrap_or_default();
    let path = if next.starts_with('/') {
        next.to_owned()
    } else {
        format!("/{next}")
    };
    format!("{scheme}://{host}{path}")
}
fn collect_cookies(headers: &reqwest::header::HeaderMap, into: &mut BTreeMap<String, String>) {
    for value in headers.get_all(header::SET_COOKIE) {
        if let Ok(value) = value.to_str()
            && let Some(pair) = value.split(';').next()
            && let Some((name, value)) = pair.split_once('=')
            && !name.trim().is_empty()
        {
            into.insert(name.trim().to_owned(), value.trim().to_owned());
        }
    }
}
fn cookie_header(cookies: &BTreeMap<String, String>) -> Option<String> {
    if cookies.is_empty() {
        return None;
    }
    let mut pairs: Vec<String> = cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    pairs.sort();
    Some(pairs.join("; "))
}
fn parse_cookie_header(raw: &str) -> BTreeMap<String, String> {
    raw.split(';')
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            Some((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}
#[async_trait]
impl QrAuthenticationProvider for QqMusicProvider {
    async fn create_qr_login(&self) -> Result<QrLoginSession> {
        let mut cookies = BTreeMap::new();
        // Warmup: the login page drops pt_login_sig etc. that later calls need.
        let warmup_url = format!(
            "{XLOGIN_URL}?appid={QQ_APPID}&daid=383&style=33&login_text=登录&hide_title_bar=1&hide_border=1&target=self&s_url={OAUTH_JUMP}&pt_3rd_aid={OAUTH_CLIENT_ID}&theme=2"
        );
        if let Ok(response) = self
            .login_client
            .get(&warmup_url)
            .header(header::USER_AGENT, LOGIN_UA)
            .header(header::REFERER, LOGIN_REFERER)
            .send()
            .await
        {
            collect_cookies(response.headers(), &mut cookies);
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or_default();
        let show_url = format!(
            "{QR_SHOW_URL}?appid={QQ_APPID}&e=2&l=M&s=3&d=72&v=4&t={timestamp}&daid=383&pt_3rd_aid={OAUTH_CLIENT_ID}&u1={OAUTH_JUMP}"
        );
        let mut request = self
            .login_client
            .get(&show_url)
            .header(header::USER_AGENT, LOGIN_UA)
            .header(header::REFERER, LOGIN_REFERER);
        if let Some(cookie) = cookie_header(&cookies) {
            request = request.header(header::COOKIE, cookie);
        }
        let response = request
            .send()
            .await
            .map_err(|e| perr(format!("QR image request failed: {e}")))?;
        collect_cookies(response.headers(), &mut cookies);
        let png = response
            .bytes()
            .await
            .map_err(|e| perr(format!("QR image download failed: {e}")))?;
        let qrsig = cookies
            .get("qrsig")
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| perr("QR login did not return a qrsig cookie"))?
            .to_owned();
        cookies.remove("qrsig");
        self.store_qr_pending(&cookies)?;
        Ok(QrLoginSession {
            key: qrsig,
            url: format!("data:image/png;base64,{}", STANDARD.encode(png)),
            expires_in: 180,
        })
    }
    async fn poll_qr_login(&self, key: &str) -> Result<QrLoginPoll> {
        let key = key.trim();
        if key.is_empty() {
            return Err(Error::InvalidInput("QR login key is empty".into()));
        }
        if let Some(uuid) = key.strip_prefix(WX_KEY_PREFIX) {
            return self.poll_wechat(uuid).await;
        }
        let qrsig = key;
        let mut cookies = self.qr_pending();
        cookies.insert("qrsig".into(), qrsig.to_owned());
        let action_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let url = format!(
            "{QR_CHECK_URL}?u1={OAUTH_JUMP}&ptqrtoken={}&ptredirect=0&h=1&t=1&g=1&from_ui=1&ptlang=2052&action=0-0-{action_ms}&js_ver=26071711&js_type=1&login_sig=&pt_uistyle=40&aid={QQ_APPID}&daid=383&pt_3rd_aid={OAUTH_CLIENT_ID}",
            hash33(qrsig)
        );
        let mut request = self
            .login_client
            .get(&url)
            .header(header::USER_AGENT, LOGIN_UA)
            .header(header::REFERER, LOGIN_REFERER);
        if let Some(cookie) = cookie_header(&cookies) {
            request = request.header(header::COOKIE, cookie);
        }
        let body = request
            .send()
            .await
            .map_err(|e| perr(format!("QR poll request failed: {e}")))?
            .text()
            .await
            .map_err(|e| perr(format!("QR poll response read failed: {e}")))?;
        let (code, redirect_url, message, nickname) =
            parse_ptui_cb(&body).ok_or_else(|| perr("QR login status parse failed"))?;
        let status = status_from_code(&code);
        let message = if message.trim().is_empty() {
            "登录状态未知".to_owned()
        } else {
            message
        };
        let mut user_id = None;
        let mut display_name = None;
        if status == QrLoginStatus::Success {
            // Walk the OAuth chain manually and harvest every hop's cookies.
            let collected = self.follow_redirects(&redirect_url, cookies).await;
            let has_identity = IDENTITY_COOKIES
                .iter()
                .any(|name| collected.contains_key(*name));
            let has_key = KEY_COOKIES.iter().any(|name| collected.contains_key(*name));
            if !(has_identity && has_key) {
                return Ok(QrLoginPoll {
                    status: QrLoginStatus::Failed,
                    message: "QQ 登录成功但未拿到音乐凭证，请重新扫码".into(),
                    user_id: None,
                    display_name: None,
                });
            }
            user_id = IDENTITY_COOKIES
                .iter()
                .find_map(|name| collected.get(*name).cloned());
            display_name = (!nickname.trim().is_empty()).then_some(nickname.clone());
            let mut session = self.sessions.get(ID).unwrap_or_default();
            session.cookies = collected;
            session.user_id = user_id.clone();
            session.display_name = display_name.clone();
            session.tokens.remove(QR_PENDING_KEY);
            self.sessions.set(ID, session)?;
        }
        Ok(QrLoginPoll {
            status,
            message,
            user_id,
            display_name,
        })
    }
}
impl QqMusicProvider {
    fn qr_pending(&self) -> BTreeMap<String, String> {
        self.sessions
            .get(ID)
            .and_then(|session| session.tokens.get(QR_PENDING_KEY).cloned())
            .map(|raw| parse_cookie_header(&raw))
            .unwrap_or_default()
    }
    fn store_qr_pending(&self, cookies: &BTreeMap<String, String>) -> Result<()> {
        let mut session = self.sessions.get(ID).unwrap_or_default();
        session.tokens.insert(
            QR_PENDING_KEY.into(),
            cookie_header(cookies).unwrap_or_default(),
        );
        self.sessions.set(ID, session)
    }
    async fn follow_redirects(
        &self,
        first: &str,
        cookies: BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        let mut collected = cookies;
        let mut location = first.trim().to_owned();
        for _ in 0..MAX_REDIRECTS {
            if location.is_empty() {
                break;
            }
            let mut request = self
                .login_client
                .get(&location)
                .header(header::USER_AGENT, LOGIN_UA)
                .header(header::REFERER, LOGIN_REFERER);
            if let Some(cookie) = cookie_header(&collected) {
                request = request.header(header::COOKIE, cookie);
            }
            let Ok(response) = request.send().await else {
                break;
            };
            collect_cookies(response.headers(), &mut collected);
            let next = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            if next.is_empty() {
                break;
            }
            location = resolve_location(&location, &next);
        }
        collected
    }
    fn logged_in_uin(&self) -> Result<String> {
        let session = self
            .sessions
            .get(ID)
            .ok_or_else(|| perr("please log in first"))?;
        if session.tokens.contains_key("login_expired") {
            return Err(perr("login session expired"));
        }
        let identity = IDENTITY_COOKIES
            .iter()
            .find_map(|name| session.cookies.get(*name).cloned())
            .ok_or_else(|| perr("please log in first"))?;
        let has_key = KEY_COOKIES
            .iter()
            .any(|name| session.cookies.contains_key(*name));
        if !has_key {
            return Err(perr("please log in first"));
        }
        Ok(identity)
    }
    /// One playlist-detail page: (meta, songs, page-complete flag).
    async fn fetch_diss_page(
        &self,
        disstid: u64,
        song_begin: u64,
    ) -> Result<(Playlist, Vec<Track>, bool)> {
        let body = json!({
            "comm": {"ct": 20, "cv": 1859, "uin": 0, "format": "json"},
            "req": {
                "module": "music.srfDissInfo.aiDissInfo",
                "method": "uniform_get_Dissinfo",
                "param": {
                    "disstid": disstid,
                    "enc_host_uin": "",
                    "tag": 1,
                    "userInfo": 1,
                    "song_begin": song_begin,
                    "song_num": DETAIL_PAGE_SIZE,
                    "onlysonglist": 0,
                },
            },
        });
        let value = self.musicu(&body, "playlist detail").await?;
        let req = &value["req"];
        if req["code"].as_i64().unwrap_or(-1) != 0
            || req["data"]["code"].as_i64().unwrap_or(-1) != 0
        {
            return Err(perr("playlist detail request failed"));
        }
        let data = &req["data"];
        let songs = data["songlist"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_song)
            .collect::<Vec<_>>();
        let complete = songs.len() < DETAIL_PAGE_SIZE as usize;
        let title = data["dirinfo"]["title"]
            .as_str()
            .or_else(|| data["dissname"].as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("QQ 歌单 {disstid}"));
        let playlist = Playlist {
            id: TrackId::new(ID, disstid.to_string()),
            title,
            description: non_empty(&data["desc"]),
            artwork_url: non_empty(&data["dirinfo"]["picurl"]),
            track_count: data["total_song_num"]
                .as_u64()
                .or_else(|| data["dirinfo"]["songnum"].as_u64())
                .map(|n| n as u32),
            tracks: vec![],
        };
        Ok((playlist, songs, complete))
    }
}
#[async_trait]
impl AuthenticationProvider for QqMusicProvider {
    async fn login_status(&self) -> Result<LoginStatus> {
        let Some(session) = self.sessions.get(ID) else {
            return Ok(LoginStatus::LoggedOut);
        };
        let has_identity = IDENTITY_COOKIES
            .iter()
            .any(|name| session.cookies.contains_key(*name));
        let has_key = KEY_COOKIES
            .iter()
            .any(|name| session.cookies.contains_key(*name));
        if !(has_identity && has_key) {
            return Ok(LoginStatus::LoggedOut);
        }
        if session.tokens.contains_key("login_expired") {
            return Ok(LoginStatus::Expired);
        }
        Ok(LoginStatus::LoggedIn {
            user_id: session.user_id,
            display_name: session.display_name,
        })
    }
    async fn logout(&self) -> Result<()> {
        self.sessions.clear(ID);
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zzc_sign_matches_lx_music_golden_vector() {
        assert_eq!(
            zzc_sign(r#"{"test":"hello"}"#),
            "zzc8cbd808brhsjkhrcde4fogzsjczmgmobi8c73809f"
        );
    }
    #[test]
    fn filename_ladder_steps_down_and_formats_correctly() {
        let mid = "003aAYrm3GE0Ac";
        let candidates: Vec<String> = tier(StreamQuality::HiRes)
            .iter()
            .map(|(prefix, _, format)| format!("{prefix}{mid}.{}", extension(*format)))
            .collect();
        assert_eq!(
            candidates,
            vec![
                "AI00003aAYrm3GE0Ac.flac",
                "Q001003aAYrm3GE0Ac.flac",
                "Q000003aAYrm3GE0Ac.flac",
                "F000003aAYrm3GE0Ac.flac",
                "O801003aAYrm3GE0Ac.ogg",
                "M800003aAYrm3GE0Ac.mp3",
                "M500003aAYrm3GE0Ac.mp3",
            ]
        );
        let low: Vec<String> = tier(StreamQuality::Low)
            .iter()
            .map(|(prefix, _, format)| format!("{prefix}{mid}.{}", extension(*format)))
            .collect();
        assert_eq!(low, vec!["M500003aAYrm3GE0Ac.mp3"]);
        let high: Vec<String> = tier(StreamQuality::High)
            .iter()
            .map(|(prefix, _, format)| format!("{prefix}{mid}.{}", extension(*format)))
            .collect();
        assert_eq!(high.len(), 2);
        assert!(high[0].starts_with("M800"));
        assert!(high[1].starts_with("M500"));
    }
    #[test]
    fn size_keys_map_to_track_extras() {
        assert_eq!(size_key("M500"), "size_128mp3");
        assert_eq!(size_key("M800"), "size_320mp3");
        assert_eq!(size_key("F000"), "size_flac");
        assert_eq!(size_key("AI00"), "size_hires");
    }
    #[test]
    fn vkey_request_body_has_expected_shape() {
        let body = vkey_request_body("abc", &["M500abc.mp3".into()], 4242);
        let param = &body["req_0"]["param"];
        assert_eq!(body["req_0"]["module"], "vkey.GetVkeyServer");
        assert_eq!(body["req_0"]["method"], "CgiGetVkey");
        assert_eq!(param["guid"], 4242);
        assert_eq!(param["songmid"][0], "abc");
        assert_eq!(param["uin"], "0");
        assert_eq!(param["filename"][0], "M500abc.mp3");
        assert_eq!(body["comm"]["cv"], 0);
    }
    #[test]
    fn stream_response_picking_skips_empty_entries() {
        let candidates = vec![
            (
                "M800abc.mp3".to_string(),
                StreamQuality::High,
                AudioFormat::Mp3,
                "size_320mp3",
            ),
            (
                "M500abc.mp3".to_string(),
                StreamQuality::Low,
                AudioFormat::Mp3,
                "size_128mp3",
            ),
        ];
        let value = json!({
            "req_0": {"data": {
                "sip": ["http://dl.stream.qqmusic.qq.com/"],
                "midurlinfo": [
                    {"filename": "M800abc.mp3", "purl": "", "vkey": "v1"},
                    {"filename": "M500abc.mp3", "purl": "M500abc.mp3?extra=1", "vkey": "v2"}
                ]
            }}
        });
        let (candidate, sip, purl, vkey) = pick_stream(&value, &candidates).unwrap();
        assert_eq!(candidate.0, "M500abc.mp3");
        assert_eq!(candidate.1, StreamQuality::Low);
        assert_eq!(sip, "http://dl.stream.qqmusic.qq.com/");
        assert_eq!(purl, "M500abc.mp3?extra=1");
        assert_eq!(vkey, "v2");
        // Empty sip means "not playable" even with valid purl entries.
        let blocked = json!({"req_0": {"data": {
            "sip": [""],
            "midurlinfo": [{"filename": "M500abc.mp3", "purl": "x", "vkey": "v"}]
        }}});
        assert!(pick_stream(&blocked, &candidates).is_none());
    }
    #[test]
    fn parses_search_item_into_track() {
        let item = json!({
            "id": 483034200_i64,
            "mid": "003aAYrm3GE0Ac",
            "title": "晴天",
            "singer": [{"name": "周杰伦"}, {"name": "another"}],
            "album": {"mid": "002Neh8l0uciQ1", "name": "叶惠美"},
            "interval": 269,
            "file": {
                "media_mid": "003aAYrm3GE0Ac",
                "size_128mp3": 4312832,
                "size_320mp3": 10781692,
                "size_flac": 0,
                "size_hires": 12345678
            }
        });
        let track = parse_song(&item).unwrap();
        assert_eq!(track.id.id, "003aAYrm3GE0Ac");
        assert_eq!(track.artists, vec!["周杰伦", "another"]);
        assert_eq!(track.album.as_deref(), Some("叶惠美"));
        assert_eq!(track.album_id.as_deref(), Some("002Neh8l0uciQ1"));
        assert_eq!(track.duration.map(|d| d.as_secs()), Some(269));
        assert_eq!(
            track.artwork_url.as_deref(),
            Some("https://y.gtimg.cn/music/photo_new/T002R500x500M000002Neh8l0uciQ1.jpg")
        );
        assert_eq!(
            track.extra.get("songId").map(String::as_str),
            Some("483034200")
        );
        assert_eq!(
            track.extra.get("strMediaMid").map(String::as_str),
            Some("003aAYrm3GE0Ac")
        );
        assert!(track.available_qualities.contains(&StreamQuality::High));
        assert!(track.available_qualities.contains(&StreamQuality::HiRes));
        assert!(!track.available_qualities.contains(&StreamQuality::Lossless));
        assert!(parse_song(&json!({"mid": "", "title": "x"})).is_none());
        assert!(parse_song(&json!({"mid": "abc"})).is_none());
    }
    #[test]
    fn parses_qq_share_links_preserving_mid_case() {
        let url = "https://y.qq.com/n/ryqq/songDetail/003aAYrm3GE0Ac";
        assert_eq!(
            parse_link_target(url),
            Some(("song".into(), "003aAYrm3GE0Ac".into()))
        );
        assert_eq!(
            parse_link_target("https://y.qq.com/n/ryqq/songDetail/ABCdef123?from=tag"),
            Some(("song".into(), "ABCdef123".into()))
        );
        assert_eq!(
            parse_link_target("https://y.qq.com/#/song?songmid=001abcXYZ&share=1"),
            Some(("song".into(), "001abcXYZ".into()))
        );
        assert_eq!(
            parse_link_target("https://y.qq.com/web?songid=483034200"),
            Some(("songid".into(), "483034200".into()))
        );
        assert_eq!(
            parse_link_target("https://y.qq.com/n/ryqq/playlist/7242345568"),
            Some(("playlist".into(), "7242345568".into()))
        );
        assert_eq!(
            parse_link_target("https://y.qq.com/taoge/123456"),
            Some(("playlist".into(), "123456".into()))
        );
        assert_eq!(
            parse_link_target("https://y.qq.com/#/playlist?id=7242345568"),
            Some(("playlist".into(), "7242345568".into()))
        );
        assert_eq!(
            parse_link_target("https://y.qq.com/?disstid=42"),
            Some(("playlist".into(), "42".into()))
        );
        assert_eq!(parse_link_target("https://evil.com/songDetail/abc"), None);
        assert_eq!(parse_link_target("https://y.qq.com/singer/xyz"), None);
    }
    #[tokio::test]
    async fn login_status_requires_identity_and_key_cookies() {
        let provider = QqMusicProvider::new();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        // Key cookie alone is not enough.
        let mut cookies = BTreeMap::new();
        cookies.insert("qqmusic_key".into(), "secret".into());
        provider.set_cookies(cookies, None, None).unwrap();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        // Identity + key (QQ spelling) counts as logged in.
        let mut cookies = BTreeMap::new();
        cookies.insert("uin".into(), "12345".into());
        cookies.insert("qqmusic_key".into(), "secret".into());
        provider.set_cookies(cookies, None, None).unwrap();
        assert!(matches!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedIn { .. }
        ));
        // WeChat spelling also counts.
        let mut cookies = BTreeMap::new();
        cookies.insert("wxuin".into(), "67890".into());
        cookies.insert("qm_keyst".into(), "secret".into());
        provider.set_cookies(cookies, None, None).unwrap();
        assert!(matches!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedIn { .. }
        ));
        provider.mark_session_expired();
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);
        provider.logout().await.unwrap();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
    }
    #[test]
    fn extracts_wx_uuid_from_page_markers() {
        let js = r#"<script>var u="https://open.weixin.qq.com/connect/l/qrconnect?uuid=061AbC-dEf_12";</script>"#;
        assert_eq!(extract_wx_uuid(js).as_deref(), Some("061AbC-dEf_12"));
        let image = r#"<img src="https://open.weixin.qq.com/connect/qrcode/061gVkgZ2dwlGa1u">"#;
        assert_eq!(extract_wx_uuid(image).as_deref(), Some("061gVkgZ2dwlGa1u"));
        assert!(extract_wx_uuid("<html>no uuid here</html>").is_none());
    }
    #[test]
    fn parses_wechat_poll_states() {
        let waiting = parse_wx_poll("window.wx_errcode=408;window.wx_code='';").unwrap();
        assert_eq!(waiting, ("408".into(), String::new()));
        let confirmed = parse_wx_poll("window.wx_errcode=405;window.wx_code='abc123';").unwrap();
        assert_eq!(confirmed, ("405".into(), "abc123".into()));
        assert!(parse_wx_poll("garbage").is_none());
    }
    #[test]
    fn extracts_wechat_cookies_from_exchange_response() {
        let response = json!({
            "code": 0,
            "req": {"code": 0, "data": {
                "musickey": "W_X_key",
                "str_musicid": "123456",
                "openid": "openid-1",
                "unionid": "unionid-1",
                "access_token": "at",
                "refresh_token": "rt"
            }}
        });
        let cookies = wechat_cookies(&response).unwrap();
        assert_eq!(cookies.get("login_type").map(String::as_str), Some("2"));
        assert_eq!(cookies.get("uin").map(String::as_str), Some("123456"));
        assert_eq!(cookies.get("wxuin").map(String::as_str), Some("123456"));
        assert_eq!(
            cookies.get("qqmusic_key").map(String::as_str),
            Some("W_X_key")
        );
        assert_eq!(cookies.get("qm_keyst").map(String::as_str), Some("W_X_key"));
        assert_eq!(
            cookies.get("wxopenid").map(String::as_str),
            Some("openid-1")
        );
        // Numeric musicid variant also extracts.
        let numeric = json!({"code": 0, "req": {"code": 0, "data": {
            "musickey": "W_X_key", "musicid": 654321
        }}});
        let cookies = wechat_cookies(&numeric).unwrap();
        assert_eq!(cookies.get("uin").map(String::as_str), Some("654321"));
        // Missing musickey fails closed.
        assert!(wechat_cookies(&json!({"code": 0, "req": {"code": 0, "data": {}}})).is_none());
        assert!(
            wechat_cookies(
                &json!({"code": 500, "req": {"code": 0, "data": {"musickey": "W_X_k"}}})
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = QqMusicProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.lyrics);
        assert!(descriptor.capabilities.authentication);
        assert!(descriptor.capabilities.playlists);
        assert!(descriptor.capabilities.albums);
        assert!(descriptor.capabilities.charts);
        assert!(descriptor.capabilities.link_resolve);
        // Artist lookup has no upstream endpoint and must not be claimed.
        assert!(!descriptor.capabilities.artists);
        assert!(registry.link_provider(ID).is_some());
        assert!(registry.playlist_provider(ID).is_some());
        assert!(registry.collection_provider(ID).is_some());
        assert!(registry.album_provider(ID).is_some());
        assert!(registry.chart_provider(ID).is_some());
        assert!(registry.qr_authentication_provider(ID).is_some());
    }
    #[test]
    fn hash33_matches_the_rolling_formula() {
        // 97, 98, 99: h=97; h=3104+97+98=3299; h=105568+3299+99=108966.
        assert_eq!(hash33("abc"), 108_966);
        assert_eq!(hash33(""), 0);
    }
    #[test]
    fn parses_ptui_callback() {
        let raw = "ptuiCB('0','0','https://ptlogin2.qq.com/login?u1=x','0','登录成功！', '昵称');";
        let (code, redirect, message, nickname) = parse_ptui_cb(raw).unwrap();
        assert_eq!(code, "0");
        assert!(redirect.starts_with("https://ptlogin2.qq.com/login"));
        assert_eq!(message, "登录成功！");
        assert_eq!(nickname, "昵称");
        let waiting = "ptuiCB('66','0','','0','二维码未失效。','');";
        assert_eq!(parse_ptui_cb(waiting).unwrap().0, "66");
        assert!(parse_ptui_cb("no callback here").is_none());
    }
    #[test]
    fn maps_qr_status_codes() {
        assert_eq!(status_from_code("0"), QrLoginStatus::Success);
        assert_eq!(status_from_code("65"), QrLoginStatus::Expired);
        assert_eq!(status_from_code("66"), QrLoginStatus::Waiting);
        assert_eq!(status_from_code("67"), QrLoginStatus::Scanned);
        assert_eq!(status_from_code("99"), QrLoginStatus::Failed);
    }
    #[test]
    fn resolves_relative_redirects_against_the_current_host() {
        assert_eq!(
            resolve_location("https://a.com/x", "https://b.com/y"),
            "https://b.com/y"
        );
        assert_eq!(resolve_location("https://a.com/x", "/y"), "https://a.com/y");
        assert_eq!(resolve_location("https://a.com/x", "y"), "https://a.com/y");
        assert_eq!(resolve_location("not-a-url", "/y"), "/y");
    }
    #[test]
    fn strips_jsonp_shells() {
        assert_eq!(strip_jsonp("callback({\"a\":1});"), "{\"a\":1}");
        assert_eq!(strip_jsonp("  {\"a\":1}  "), "{\"a\":1}");
        assert_eq!(strip_jsonp("not json"), "not json");
        assert_eq!(strip_jsonp("{\"a\":\"(x)\"}"), "{\"a\":\"(x)\"}");
    }
    #[test]
    fn parses_diss_by_tag_items_with_https_upgrade() {
        let item = json!({
            "dissid": 6800944944_i64,
            "dissname": "劲爆嗨曲",
            "imgurl": "http://img.example/logo.jpg",
            "song_count": 71,
            "listennum": 12345,
            "creator": {"name": "某位用户"},
            "introduction": "简介"
        });
        let playlist = parse_diss_by_tag_item(&item).unwrap();
        assert_eq!(playlist.id.id, "6800944944");
        assert_eq!(
            playlist.artwork_url.as_deref(),
            Some("https://img.example/logo.jpg")
        );
        assert_eq!(playlist.track_count, Some(71));
        assert_eq!(playlist.description.as_deref(), Some("简介"));
    }
    #[test]
    fn parses_user_playlist_dual_field_shapes() {
        // Collected shape (fcg_get_profile_order_asset): dissid, no underscores.
        let collected = parse_user_playlist(&json!({
            "dissid": 6800944944_i64,
            "dissname": "劲爆嗨曲中文dj舞曲",
            "songnum": 71,
            "listennum": 12345,
            "logo": "http://img.example/logo.jpg",
            "nickname": "某位用户"
        }))
        .unwrap();
        assert_eq!(collected.id.id, "6800944944");
        assert_eq!(collected.track_count, Some(71));
        // Created shape (fcg_user_created_diss): tid, underscore names.
        let created = parse_user_playlist(&json!({
            "dirid": 3,
            "tid": 7758023113_i64,
            "diss_name": "我的自建歌单",
            "diss_cover": "http://img.example/cover.jpg",
            "song_cnt": 12,
            "listen_num": 0
        }))
        .unwrap();
        assert_eq!(created.id.id, "7758023113");
        assert_eq!(created.title, "我的自建歌单");
        assert_eq!(created.track_count, Some(12));
    }
    #[test]
    fn created_playlists_skip_favorites_and_system_dirs() {
        let created = json!({
            "code": 0,
            "data": {
                "disslist": [
                    {"dirid": 205, "tid": 0, "diss_name": "QZone背景音乐", "song_cnt": 0},
                    {"dirid": 201, "tid": 7758023113_i64, "diss_name": "我喜欢", "song_cnt": 267},
                    {"dirid": 206, "tid": 0, "diss_name": "本地上传", "song_cnt": 0},
                    {"dirid": 3, "tid": 6800944944_i64, "diss_name": "自建歌单", "song_cnt": 5}
                ]
            }
        });
        let playlists = created_playlists(&created);
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].id.id, "6800944944");
    }
    #[test]
    fn parses_playlist_categories_with_groups() {
        let value = json!({
            "code": 0,
            "data": {
                "categories": [
                    {
                        "categoryGroupName": "语种",
                        "items": [
                            {"categoryId": 3, "categoryName": "华语", "usable": 1},
                            {"categoryId": 0, "categoryName": "坏数据", "usable": 1},
                            {"categoryId": 10_000_000, "categoryName": "全部", "usable": 1},
                            {"categoryId": 4, "categoryName": "下架分类", "usable": 0}
                        ]
                    },
                    {"categoryGroupName": "风格", "items": [{"categoryId": 6, "categoryName": "摇滚", "usable": 1}]}
                ]
            }
        });
        let cats = parse_categories(&value);
        assert_eq!(cats.len(), 3);
        assert_eq!(cats[0].id, "10000000");
        assert_eq!(cats[0].name, "全部");
        assert_eq!(cats[1].name, "华语");
        assert_eq!(cats[1].group.as_deref(), Some("语种"));
        assert_eq!(cats[2].name, "摇滚");
        assert_eq!(cats[2].group.as_deref(), Some("风格"));
    }
    #[test]
    fn parses_toplist_boards_with_prefix_stripping() {
        let board = parse_toplist_board(&json!({"id": 26, "topTitle": "巅峰榜·热歌"})).unwrap();
        assert_eq!(board.id.id, "26");
        assert_eq!(board.title, "热歌榜");
        let plain = parse_toplist_board(&json!({"id": 57, "topTitle": "内地"})).unwrap();
        assert_eq!(plain.title, "内地榜");
        assert!(parse_toplist_board(&json!({"id": 201, "topTitle": "x"})).is_some());
    }
    #[test]
    fn derives_album_metadata_from_song_list() {
        let data = json!({
            "totalNum": 11,
            "songList": [{
                "songInfo": {
                    "mid": "0039MnYb0qxYhV",
                    "singer": [{"name": "周杰伦"}],
                    "album": {"mid": "000MkMni19ClKG", "name": "叶惠美", "time_public": "2003-07-31"}
                }
            }]
        });
        let album = album_from_songlist(&data, "000MkMni19ClKG").unwrap();
        assert_eq!(album.id.id, "000MkMni19ClKG");
        assert_eq!(album.title, "叶惠美");
        assert_eq!(album.artists, vec!["周杰伦"]);
        assert_eq!(album.track_count, Some(11));
        assert_eq!(
            album.artwork_url.as_deref(),
            Some("https://y.gtimg.cn/music/photo_new/T002R500x500M000000MkMni19ClKG.jpg")
        );
        assert!(album_from_songlist(&json!({"songList": []}), "x").is_none());
    }
}
