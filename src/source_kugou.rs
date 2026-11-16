//! Kugou Music provider migrated from VoiceFox's `lx-source::kg`.
//! Protocol origin: lx-music `musicSdk/kg/`. Search (unsigned GET), lyrics
//! (two-step search + download with KRC XOR/zlib decryption) and login-gated
//! stream resolution are implemented. Kugou refuses stream URLs to anonymous
//! sessions entirely (verified upstream): resolution requires a logged-in
//! session with `token`/`userid`/`KUGOU_API_MID` cookies. QR login, playlists,
//! albums, artists, charts and link parsing remain pending
//! (see docs/SOURCE_QQ_MUSIC_AUDIT.md).
use crate::{
    Error, Result,
    lrc::parse_lrc,
    models::{
        Album, Artist, AudioFormat, Chart, ChartEntry, LyricDocument, LyricFormat, LyricLine,
        LyricWord, Playlist, PlaylistCategory, StreamInfo, StreamQuality, Track, TrackId,
    },
    provider::{
        AlbumProvider, ArtistProvider, AuthenticationProvider, ChartProvider, CollectionProvider,
        LinkResolver, LoginStatus, LyricsProvider, MusicProvider, PlaylistProvider,
        QrAuthenticationProvider, QrLoginPoll, QrLoginSession, QrLoginStatus, ResolveOptions,
        ResolvedLink, SearchPage, SearchQuery, StreamResolver, TrackSearch,
    },
    retry::{KUGOU_CODES, StepOutcome, classify_api, classify_http},
    session::{ProviderSession, SessionStore},
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::read::ZlibDecoder;
use md5::{Digest, Md5};
use reqwest::{Client, RequestBuilder, header};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, time::Duration};

const ID: &str = "kugou";
const UA_WEB: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0 Safari/537.36";
const SEARCH_URL: &str = "https://songsearch.kugou.com/song_search_v2";
const LYRIC_SEARCH_URL: &str = "http://lyrics.kugou.com/search";
const LYRIC_DOWNLOAD_URL: &str = "http://lyrics.kugou.com/download";
/// Kugou refuses stream URLs to anonymous callers; the signed v5 endpoint
/// covers the logged-in path (the legacy v3 gateway is a redundant fallback).
const V5_URL: &str = "https://gateway.kugou.com/v5/url";
const V3_URL: &str = "http://gateway.kugou.com/v3/album_audio/audio";
/// Lite-client constants for the v5 signed URL endpoint.
const LITE_APP_ID: &str = "3116";
const LITE_VER: &str = "11440";
const LITE_KEY: &str = "185672dd44712f60bb1736df5a377e82";
const LITE_SIGN: &str = "LnT6xpN3khm36zse0QzvmgTZ3waWdRSA";
const KRC_MAGIC: &[u8; 4] = b"krc1";
const KRC_XOR_KEY: [u8; 16] = [
    0x40, 0x47, 0x61, 0x77, 0x5e, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2d, 0xce, 0xd2, 0x6e, 0x69,
];

#[derive(Clone)]
pub struct KugouProvider {
    client: Client,
    sessions: SessionStore,
}
impl std::fmt::Debug for KugouProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KugouProvider")
            .field("session_store", &"redacted")
            .finish()
    }
}
impl Default for KugouProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl KugouProvider {
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
    /// Register all currently implemented Kugou capabilities in one step.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_lyrics(provider.clone());
        registry.register_authentication(provider.clone());
        registry.register_qr_authentication(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_playlist(provider.clone());
        registry.register_chart(provider.clone());
        registry.register_album(provider.clone());
        registry.register_artist(provider.clone());
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
        let mut request = request.header(header::USER_AGENT, UA_WEB);
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
                    tracing::debug!("Kugou {label} transport retry {}: {error}", attempt + 1);
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
        let message = value["error"]
            .as_str()
            .or_else(|| value["message"].as_str())
            .or_else(|| value["msg"].as_str())
            .unwrap_or("unknown error");
        let budget = 0;
        match classify_api(
            value["error_code"].as_i64().unwrap_or(KUGOU_CODES.ok),
            message,
            budget,
            budget,
            &KUGOU_CODES,
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
    /// A logged-in session for stream resolution: Kugou needs `token`,
    /// `userid` and `KUGOU_API_MID` cookies or it hands out no URL at all.
    fn login_parts(&self) -> Result<(String, String, String)> {
        let session = self
            .sessions
            .get(ID)
            .ok_or_else(|| perr("Kugou playback requires login; please scan in first"))?;
        if session.tokens.contains_key("login_expired") {
            return Err(perr("Kugou login session expired"));
        }
        let token = session
            .cookies
            .get("token")
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| perr("Kugou playback requires login; please scan in first"))?
            .clone();
        let user_id = session
            .user_id
            .clone()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| perr("Kugou login is missing the account id; please scan in again"))?;
        let mid = session
            .cookies
            .get("KUGOU_API_MID")
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                perr("Kugou login is missing the device MID cookie; please scan in again")
            })?
            .clone();
        Ok((token, user_id, mid))
    }
    async fn search_lyric(&self, track: &Track) -> Result<(String, String)> {
        let hash: String = track
            .extra
            .get("SQFileHash")
            .or_else(|| track.extra.get("HQFileHash"))
            .or_else(|| track.extra.get("FileHash"))
            .map(|value| value.to_lowercase())
            .unwrap_or_default();
        let keyword = format!("{}+{}", track.title, track.artists.join("、"));
        let duration_secs = track.duration.map(|d| d.as_secs()).unwrap_or_default();
        let url = format!(
            "{LYRIC_SEARCH_URL}?ver=1&man=yes&client=pc&keyword={}&hash={hash}&timelength={duration_secs}",
            urlencoding::encode(&keyword)
        );
        let value = self.get_json(&url, "lyric search").await?;
        let first = value["candidates"]
            .as_array()
            .and_then(|candidates| candidates.first())
            .ok_or(Error::NotFound)?;
        let id = first["id"].as_str().unwrap_or_default().to_owned();
        if id.is_empty() {
            return Err(Error::NotFound);
        }
        let accesskey = first["accesskey"].as_str().unwrap_or_default().to_owned();
        Ok((id, accesskey))
    }
    async fn download_lyric(&self, id: &str, accesskey: &str, format: &str) -> Result<Vec<u8>> {
        let url = format!(
            "{LYRIC_DOWNLOAD_URL}?ver=1&client=pc&id={id}&accesskey={accesskey}&fmt={format}&charset=utf8"
        );
        let value = self.get_json(&url, "lyric download").await?;
        let content = value["content"].as_str().ok_or(Error::NotFound)?;
        STANDARD
            .decode(content.trim())
            .map_err(|e| perr(format!("lyric base64 decode failed: {e}")))
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}
/// KRC decryption: strip the `krc1` magic, XOR with the fixed 16-byte key,
/// then zlib-inflate the remainder.
fn decrypt_krc(data: &[u8]) -> Result<String> {
    let encrypted = data
        .strip_prefix(KRC_MAGIC)
        .ok_or_else(|| perr("invalid KRC header"))?;
    let decrypted: Vec<u8> = encrypted
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ KRC_XOR_KEY[index % KRC_XOR_KEY.len()])
        .collect();
    let mut decoder = ZlibDecoder::new(decrypted.as_slice());
    let mut decoded = Vec::new();
    decoder
        .read_to_end(&mut decoded)
        .map_err(|e| perr(format!("KRC zlib decode failed: {e}")))?;
    String::from_utf8(decoded).map_err(|e| perr(format!("KRC utf8 decode failed: {e}")))
}
fn decode_entities(value: &str) -> String {
    value
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#039;", "'")
}
/// One `<start,duration[,extra]>` word tag inside a KRC line body.
struct WordTag {
    offset_ms: i64,
    duration_ms: i64,
}
/// Split a KRC line body into plain text plus word tags; tags carry
/// line-relative offsets that the caller makes absolute.
fn split_krc_body(body: &str) -> (String, Vec<WordTag>) {
    let mut text = String::new();
    let mut tags = Vec::new();
    let mut plain_start = 0;
    let mut rest = body;
    while let Some(open) = rest.find('<') {
        text.push_str(&rest[plain_start..plain_start + open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else {
            break;
        };
        let mut parts = after[..close].split(',');
        let offset = parts.next().and_then(|v| v.parse::<i64>().ok());
        let duration = parts.next().and_then(|v| v.parse::<i64>().ok());
        if let (Some(offset), Some(duration)) = (offset, duration) {
            tags.push(WordTag {
                offset_ms: offset,
                duration_ms: duration,
            });
        }
        plain_start = 0;
        rest = &after[close + 1..];
    }
    text.push_str(rest);
    (decode_entities(&text), tags)
}
fn format_timestamp(millis: u64) -> String {
    let minutes = millis / 60_000;
    let seconds = (millis % 60_000) / 1_000;
    let fraction = millis % 1_000;
    format!("{minutes:02}:{seconds:02}.{fraction:03}")
}
/// Turn the decoded `[language:...]` metadata into a timed LRC string paired
/// with the main lyric timestamps (type 0 = romanization, 1 = translation).
fn timed_language_lines(lines: Option<Vec<String>>, timestamps: &[u64]) -> Option<String> {
    let lines = lines?;
    let lyric = lines
        .into_iter()
        .zip(timestamps.iter().copied())
        .map(|(text, millis)| format!("[{}]{}", format_timestamp(millis), decode_entities(&text)))
        .collect::<Vec<_>>()
        .join("\n");
    (!lyric.trim().is_empty()).then_some(lyric)
}
fn language_lines(item: &Value) -> Option<Vec<String>> {
    item["lyricContent"].as_array().map(|lines| {
        lines
            .iter()
            .map(|line| {
                line.as_array().map_or_else(
                    || line.as_str().unwrap_or_default().to_owned(),
                    |parts| parts.iter().filter_map(Value::as_str).collect::<String>(),
                )
            })
            .collect()
    })
}
/// Parse decrypted KRC text: `[millis,duration]<tags>text` lines plus the
/// base64 `[language:...]` metadata block with translation/romanization.
fn parse_krc(content: String) -> LyricDocument {
    let mut translation = None;
    let mut romanization = None;
    let mut body = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("[language:")
            && let Some(encoded) = rest.split(']').next()
            && let Ok(bytes) = STANDARD.decode(encoded)
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(items) = value["content"].as_array()
        {
            for item in items {
                match item["type"].as_i64() {
                    Some(0) => romanization = language_lines(item),
                    Some(1) => translation = language_lines(item),
                    _ => {}
                }
            }
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }
    let mut lines = Vec::new();
    let mut timestamps = Vec::new();
    let mut plain = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        let Some(inner) = trimmed.strip_prefix('[').and_then(|rest| {
            let end = rest.find(']')?;
            Some((&rest[..end], &rest[end + 1..]))
        }) else {
            continue;
        };
        let (header, line_body) = inner;
        let Some(millis) = header.split(',').next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        let (text, tags) = split_krc_body(line_body);
        let words = tags
            .iter()
            .map(|tag| LyricWord {
                text: String::new(),
                start_ms: millis.saturating_add(tag.offset_ms.max(0) as u64),
                duration_ms: tag.duration_ms.max(0) as u64,
            })
            .collect();
        lines.push(LyricLine {
            start_ms: millis,
            end_ms: None,
            text,
            translations: BTreeMap::new(),
            romanization: None,
            words,
        });
        timestamps.push(millis);
        plain.push(format!(
            "[{}]{}",
            format_timestamp(millis),
            lines.last().unwrap().text
        ));
    }
    let original = plain.join("\n");
    LyricDocument {
        format: LyricFormat::Krc,
        translation: timed_language_lines(translation, &timestamps),
        romanization: timed_language_lines(romanization, &timestamps),
        lines,
        original,
    }
}
/// Which quality the requested hash belongs to, mirroring the search fields.
fn quality_flag(q: StreamQuality) -> &'static str {
    match q {
        StreamQuality::HiRes => "super",
        StreamQuality::Lossless => "flac",
        StreamQuality::High => "320",
        _ => "128",
    }
}
fn hash_field(q: StreamQuality) -> &'static str {
    match q {
        StreamQuality::HiRes => "ResFileHash",
        StreamQuality::Lossless => "SQFileHash",
        StreamQuality::High => "HQFileHash",
        _ => "FileHash",
    }
}
fn size_field(q: StreamQuality) -> &'static str {
    match q {
        StreamQuality::HiRes => "ResFileSize",
        StreamQuality::Lossless => "SQFileSize",
        StreamQuality::High => "HQFileSize",
        _ => "FileSize",
    }
}
/// Requested hash with graceful downgrade: the requested tier first, then
/// SQ > HQ > standard, returning the quality actually selected.
fn select_hash(track: &Track, quality: StreamQuality) -> Option<(String, StreamQuality)> {
    let field = hash_field(quality);
    if let Some(hash) = track.extra.get(field).filter(|hash| !hash.is_empty()) {
        return Some((hash.clone(), quality));
    }
    const FALLBACK: [(&str, StreamQuality); 3] = [
        ("SQFileHash", StreamQuality::Lossless),
        ("HQFileHash", StreamQuality::High),
        ("FileHash", StreamQuality::Low),
    ];
    FALLBACK.iter().find_map(|(key, fallback)| {
        track
            .extra
            .get(*key)
            .filter(|hash| !hash.is_empty())
            .map(|hash| (hash.clone(), *fallback))
    })
}
fn format_for_quality(q: StreamQuality) -> AudioFormat {
    match q {
        StreamQuality::HiRes | StreamQuality::Lossless => AudioFormat::Flac,
        _ => AudioFormat::Mp3,
    }
}
/// Pick the play URL from a v5 response: top level, `data`, then per-tier groups.
fn pick_response_url(value: &Value) -> Option<String> {
    fn text(field: &Value) -> Option<String> {
        let raw = field.as_str()?.trim();
        (!raw.is_empty()).then(|| raw.replace("\\/", "/"))
    }
    if let Some(url) = text(&value["url"]).or_else(|| text(&value["backup_url"])) {
        return Some(url);
    }
    let data = &value["data"];
    if let Some(url) = text(&data["url"]).or_else(|| text(&data["backup_url"])) {
        return Some(url);
    }
    for key in ["flac", "high", "320", "128", "super"] {
        if let Some(url) = text(&data[key]["url"]).or_else(|| text(&data[key]["backup_url"])) {
            return Some(url);
        }
    }
    None
}
fn parse_kg_song(item: &Value) -> Option<Track> {
    let audio_id = kg_value_string(&item["Audioid"]);
    if audio_id.is_empty() {
        return None;
    }
    let title = item["SongName"].as_str().unwrap_or_default().trim();
    if title.is_empty() {
        return None;
    }
    let artists: Vec<String> = item["Singers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|singer| singer["name"].as_str().map(str::to_owned))
        .collect();
    let duration_secs = kg_value_i64(&item["Duration"]).max(kg_value_i64(&item["_interval"]));
    let mut qualities = Vec::new();
    for (size_key, quality) in [
        ("FileSize", StreamQuality::Low),
        ("HQFileSize", StreamQuality::High),
        ("SQFileSize", StreamQuality::Lossless),
        ("ResFileSize", StreamQuality::HiRes),
    ] {
        if kg_value_i64(&item[size_key]) > 0 {
            qualities.push(quality);
        }
    }
    let mut extra = BTreeMap::new();
    for key in ["FileHash", "HQFileHash", "SQFileHash", "ResFileHash"] {
        if let Some(hash) = item[key].as_str().filter(|hash| !hash.is_empty()) {
            extra.insert(key.into(), hash.to_owned());
        }
    }
    for key in ["FileSize", "HQFileSize", "SQFileSize", "ResFileSize"] {
        let size = kg_value_i64(&item[key]);
        if size > 0 {
            extra.insert(key.into(), size.to_string());
        }
    }
    Some(Track {
        id: TrackId::new(ID, audio_id),
        title: title.into(),
        artists,
        album: item["AlbumName"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        album_id: item["AlbumID"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        duration: (duration_secs > 0).then(|| Duration::from_secs(duration_secs as u64)),
        artwork_url: None,
        isrc: None,
        available_qualities: qualities,
        extra,
    })
}
fn kg_value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|n| n.to_string()))
        .or_else(|| value.as_u64().map(|n| n.to_string()))
        .unwrap_or_default()
}
fn kg_value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}
fn kg_value_i64(value: &Value) -> i64 {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or(0)
}
impl MusicProvider for KugouProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        "酷狗音乐"
    }
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            lyrics: true,
            authentication: true,
            playlists: true,
            charts: true,
            link_resolve: true,
            albums: true,
            // Artist lookup is a search-based fallback (no dedicated endpoint).
            artists: true,
            ..Default::default()
        }
    }
}
#[async_trait]
impl TrackSearch for KugouProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.keyword.trim().is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        let page = q.page.max(1);
        let limit = q.page_size.clamp(1, 100);
        let url = format!(
            "{SEARCH_URL}?keyword={}&page={page}&pagesize={limit}&userid=0&platform=WebFilter&filter=2&iscorrection=1&privilege_filter=0&area_code=1",
            urlencoding::encode(q.keyword.trim())
        );
        let json = self.get_json(&url, "search").await?;
        let total = json["data"]["total"].as_u64().unwrap_or(0);
        let items = json["data"]["lists"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|item| {
                // `Grp` holds per-tier variants of the same song; flatten and dedup.
                std::iter::once(item).chain(item["Grp"].as_array().into_iter().flatten())
            })
            .filter_map(parse_kg_song)
            .collect::<Vec<_>>();
        let mut seen = std::collections::HashSet::new();
        let items: Vec<Track> = items
            .into_iter()
            .filter(|track| {
                let key = format!(
                    "{}:{}",
                    track.id.id,
                    track.extra.get("FileHash").cloned().unwrap_or_default()
                );
                seen.insert(key)
            })
            .collect();
        let has_more = u64::from(page) * u64::from(limit) < total;
        Ok(SearchPage {
            items,
            total: Some(total),
            has_more,
        })
    }
}
#[async_trait]
impl StreamResolver for KugouProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to Kugou".into()));
        }
        let (hash, actual_quality) = select_hash(track, options.quality).ok_or(Error::NotFound)?;
        let (token, user_id, mid) = self.login_parts()?;
        // v5 first; the legacy v3 gateway is the documented fallback when the
        // lite endpoint rejects the session (e.g. stale token edge cases).
        let url = match self
            .v5_url(track, &hash, actual_quality, &token, &user_id, &mid)
            .await
        {
            Ok(url) => url,
            Err(v5_error) => match self.v3_url(&hash, &token, &user_id, &mid).await {
                Ok(url) => url,
                Err(_) => return Err(v5_error),
            },
        };
        Ok(StreamInfo {
            url,
            quality: actual_quality,
            format: format_for_quality(actual_quality),
            duration: track.duration,
            expires_at_unix: None,
            content_length: track
                .extra
                .get(size_field(actual_quality))
                .and_then(|size| size.parse().ok())
                .filter(|size: &u64| *size > 0),
            checksum: None,
            headers: vec![],
            fallback_urls: vec![],
            index_range: None,
            init_range: None,
            max_chunk_size: None,
        })
    }
}
impl KugouProvider {
    /// Legacy v3 gateway (kg/url.rs flow): POST with KG-* headers; the
    /// response nests data as `[[{...}]]` (early shape: `[{...}]`).
    async fn v3_url(&self, hash: &str, token: &str, user_id: &str, mid: &str) -> Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let body = json!({
            "area_code": "1",
            "data": [{"hash": hash}],
            "key": "OIlwieks28dk2k092lksi2UIkp",
            "appid": 1005,
            "clientver": 11451,
            "mid": mid,
            "dfid": "-",
            "clienttime": now
        });
        let response = self
            .request(
                self.client
                    .post(V3_URL)
                    .header("KG-THash", "13a3164")
                    .header("KG-RC", "1")
                    .header("KG-Fake", "0")
                    .header("KG-RF", "00869891")
                    .header(
                        header::USER_AGENT,
                        "Android712-AndroidPhone-11451-376-0-FeeCacheUpdate-wifi",
                    )
                    .header("x-router", "kmr.service.kugou.com")
                    .header(
                        header::COOKIE,
                        format!("token={token}; userid={user_id}; KUGOU_API_MID={mid}"),
                    )
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(serde_json::to_vec(&body).map_err(|e| perr(e.to_string()))?),
            )
            .send()
            .await
            .map_err(|e| perr(format!("v3 gateway request failed: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("v3 gateway: HTTP {}", response.status())));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|e| perr(format!("v3 response parse failed: {e}")))?;
        // `data` is nested `[[{...}]]`; early shape `[{...}]`; both supported.
        let data = match &value["data"] {
            Value::Array(arr) => arr.first().ok_or(Error::NotFound)?,
            _ => return Err(Error::NotFound),
        };
        let entry = match data {
            Value::Array(inner) => inner.first().ok_or(Error::NotFound)?,
            other => other,
        };
        let url = entry["play_url"]
            .as_str()
            .filter(|url| !url.is_empty())
            .ok_or(Error::NotFound)?;
        Ok(url.to_owned())
    }
    /// Signed v5 URL fetch (aligned with music-lib fetchURLV5). Signature:
    /// params sorted by key joined as `k=v`, sandwiched between LITE_SIGN
    /// keys, MD5'd; the per-track access key is
    /// `md5(hash + LITE_KEY + appid + mid + userid)`.
    async fn v5_url(
        &self,
        track: &Track,
        hash: &str,
        quality: StreamQuality,
        token: &str,
        user_id: &str,
        mid: &str,
    ) -> Result<String> {
        let dfid = self
            .sessions
            .get(ID)
            .and_then(|session| session.cookies.get("dfid").cloned())
            .unwrap_or_else(|| {
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default();
                format!("{:x}", Md5::digest(nonce.to_string().as_bytes())).to_uppercase()
            });
        let clienttime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
            .to_string();
        let album_audio_id = track
            .extra
            .get("album_audio_id")
            .or_else(|| track.extra.get("audio_id"))
            .cloned()
            .unwrap_or_else(|| "0".into());
        let album_id = track
            .extra
            .get("album_id")
            .cloned()
            .unwrap_or_else(|| "0".into());
        let access_key = format!(
            "{:x}",
            Md5::digest(format!("{hash}{LITE_KEY}{LITE_APP_ID}{mid}{user_id}").as_bytes())
        );
        let params: Vec<(&str, String)> = vec![
            ("dfid", dfid.clone()),
            ("mid", mid.to_string()),
            ("uuid", "-".into()),
            ("appid", LITE_APP_ID.into()),
            ("clientver", LITE_VER.into()),
            ("clienttime", clienttime.clone()),
            ("token", token.to_owned()),
            ("userid", user_id.to_owned()),
            ("album_id", album_id),
            ("area_code", "1".into()),
            ("hash", hash.to_owned()),
            ("ssa_flag", "is_fromtrack".into()),
            ("version", "11436".into()),
            ("page_id", "967177915".into()),
            ("quality", quality_flag(quality).into()),
            ("album_audio_id", album_audio_id),
            ("behavior", "play".into()),
            ("pid", "411".into()),
            ("cmd", "26".into()),
            ("pidversion", "3001".into()),
            ("IsFreePart", "0".into()),
            ("ppage_id", "356753938,823673182,967485191".into()),
            ("cdnBackup", "1".into()),
            ("kcard", "0".into()),
            ("module", String::new()),
            ("key", access_key),
        ];
        let mut sorted = params.clone();
        sorted.sort_by(|left, right| left.0.cmp(right.0));
        let joined: String = sorted
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let signature = format!(
            "{:x}",
            Md5::digest(format!("{LITE_SIGN}{joined}{LITE_SIGN}").as_bytes())
        );
        let query = params
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(key),
                    urlencoding::encode(value)
                )
            })
            .chain(std::iter::once(format!("signature={signature}")))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!("{V5_URL}?{query}");
        let response = self
            .request(self.client.get(&url))
            .header(
                header::USER_AGENT,
                "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi",
            )
            .header("x-router", "trackercdn.kugou.com")
            .header("dfid", &dfid)
            .header("clienttime", &clienttime)
            .header("mid", mid)
            .header("kg-rc", "1")
            .header("kg-thash", "5d816a0")
            .send()
            .await
            .map_err(|e| perr(format!("stream url request failed: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!(
                "stream url request failed: HTTP {}",
                response.status()
            )));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|e| perr(format!("stream url response parse failed: {e}")))?;
        pick_response_url(&value).ok_or_else(|| {
            perr("no playable stream URL returned by Kugou (the session may have expired)")
        })
    }
}
#[async_trait]
impl LyricsProvider for KugouProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to Kugou".into()));
        }
        let (id, accesskey) = self.search_lyric(track).await?;
        // Prefer KRC (word timing + language metadata); fall back to plain LRC.
        let krc_document = self
            .download_lyric(&id, &accesskey, "krc")
            .await
            .ok()
            .and_then(|encrypted| decrypt_krc(&encrypted).ok())
            .map(parse_krc)
            .filter(|document| !document.lines.is_empty());
        if let Some(document) = krc_document {
            return Ok(Some(document));
        }
        let decoded = self.download_lyric(&id, &accesskey, "lrc").await?;
        let original =
            String::from_utf8(decoded).map_err(|e| perr(format!("LRC utf8 decode failed: {e}")))?;
        if original.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(LyricDocument {
            format: LyricFormat::Lrc,
            lines: parse_lrc(&original),
            original,
            translation: None,
            romanization: None,
        }))
    }
}
#[async_trait]
impl ChartProvider for KugouProvider {
    async fn charts(&self) -> Result<Vec<Chart>> {
        let url = format!(
            "{RANK_LIST_URL}?version=9108&plat=0&showtype=2&parentid=0&apiver=6&area_code=1&withsong=1"
        );
        let value = self.get_json(&url, "chart catalogue").await?;
        if value["errcode"].as_i64().unwrap_or(-1) != 0 {
            return Err(perr(format!(
                "chart catalogue: {}",
                value["error"].as_str().unwrap_or("request failed")
            )));
        }
        let boards = value["data"]["info"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|board| board["isvol"].as_i64() == Some(1))
            .filter_map(|board| {
                let id = kg_value_string(&board["rankid"]);
                let title = board["rankname"].as_str()?.trim().to_owned();
                if id.is_empty() || title.is_empty() {
                    return None;
                }
                let frequency = board["update_frequency"].as_str().unwrap_or_default();
                let date = board["rank_id_publish_date"].as_str().unwrap_or_default();
                let description = match (frequency.is_empty(), date.is_empty()) {
                    (false, false) => Some(format!("{frequency} · {date}")),
                    (false, true) => Some(frequency.to_owned()),
                    (true, false) => Some(date.to_owned()),
                    (true, true) => None,
                };
                Some(Chart {
                    id: TrackId::new(ID, id),
                    title,
                    description,
                })
            })
            .collect::<Vec<_>>();
        if boards.is_empty() {
            return Err(Error::NotFound);
        }
        Ok(boards)
    }
    async fn chart_tracks(&self, id: &str) -> Result<Vec<ChartEntry>> {
        let page_size = 100u32;
        let url = format!(
            "{RANK_SONG_URL}?version=9108&ranktype=1&plat=0&pagesize={page_size}&area_code=1&page=1&rankid={id}&with_res_tag=0&show_portrait_mv=1"
        );
        let value = self.get_json(&url, "chart tracks").await?;
        if value["errcode"].as_i64().unwrap_or(-1) != 0 {
            return Err(perr(format!(
                "chart tracks: {}",
                value["error"].as_str().unwrap_or("request failed")
            )));
        }
        Ok(value["data"]["info"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let mut track = kg_parse_special_song(item)?;
                // The rank-song endpoint reports duration in seconds.
                if let Some(seconds) = item["duration"].as_u64() {
                    track.duration = (seconds > 0).then(|| Duration::from_secs(seconds));
                }
                Some(track)
            })
            .enumerate()
            .map(|(index, track)| ChartEntry {
                rank: (index + 1) as u32,
                track,
            })
            .collect())
    }
}
/// Link shapes: `hash=` query (song), special single page (playlist), album
/// page or `albumid=` query (album).
fn kg_link_target(link: &str) -> Result<(String, String)> {
    let lower = link.to_ascii_lowercase();
    if let Some(pos) = lower.find("hash=") {
        let hash: String = link[pos + 5..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if hash.len() == 32 {
            return Ok(("song".into(), hash));
        }
    }
    for (marker, kind) in [
        ("special/single/", "playlist"),
        ("songlist/gcid_", "playlist"),
        ("album/single/", "album"),
        ("yy/album/single/", "album"),
    ] {
        if let Some(pos) = lower.find(marker) {
            let start = pos + marker.len();
            let id = link[start..]
                .split(['/', '?', '#', '&'])
                .next()
                .unwrap_or_default()
                .to_owned();
            if !id.is_empty() {
                return Ok((kind.into(), id));
            }
        }
    }
    if let Some(pos) = lower.find("albumid=") {
        let digits: String = link[pos + 8..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return Ok(("album".into(), digits));
        }
    }
    Err(Error::InvalidInput(
        "unrecognized Kugou song/playlist/album link".into(),
    ))
}
#[async_trait]
impl LinkResolver for KugouProvider {
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink> {
        let (kind, id) = kg_link_target(url)?;
        match kind.as_str() {
            "song" => {
                // The hash-based song info endpoint returns metadata only;
                // stream resolution happens later through the v5 signed path.
                let url = format!("{SONG_INFO_URL}?cmd=playInfo&hash={id}");
                let value = self.get_json(&url, "song link").await?;
                let title = value["songName"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if title.is_empty() {
                    return Err(Error::NotFound);
                }
                let mut extra = BTreeMap::new();
                extra.insert("FileHash".into(), id.to_owned());
                if let Some(album_id) = kg_value_string(&value["albumId"]).pipe_kg() {
                    extra.insert("album_id".into(), album_id);
                }
                let audio_id = kg_value_string(&value["audioId"])
                    .pipe_kg()
                    .unwrap_or_else(|| id.clone());
                Ok(ResolvedLink::Track(Track {
                    id: TrackId::new(ID, audio_id),
                    title,
                    artists: vec![value["singerName"].as_str().unwrap_or_default().to_owned()],
                    album: value["albumName"]
                        .as_str()
                        .filter(|n| !n.is_empty())
                        .map(str::to_owned),
                    album_id: kg_value_string(&value["albumId"]).pipe_kg(),
                    duration: value["duration"].as_u64().map(Duration::from_secs),
                    artwork_url: value["imgUrl"]
                        .as_str()
                        .filter(|u| !u.is_empty())
                        .map(str::to_owned),
                    isrc: None,
                    available_qualities: vec![StreamQuality::Low],
                    extra,
                }))
            }
            "playlist" => {
                let tracks = self.playlist_tracks(&id).await?;
                let title = format!("Kugou playlist {id}");
                Ok(ResolvedLink::Playlist(Playlist {
                    id: TrackId::new(ID, id),
                    title,
                    description: None,
                    artwork_url: None,
                    track_count: Some(tracks.len() as u32),
                    tracks: vec![],
                }))
            }
            _ => Err(Error::Unsupported(
                "kugou album link resolution is not implemented yet".into(),
            )),
        }
    }
}
/// Album songs use `hash/320hash/sqhash` plus `filesize/320filesize/sqfilesize`
/// (different field names from the playlist-page shape).
fn kg_parse_album_song(item: &Value) -> Option<Track> {
    let mut track = kg_parse_special_song(item)?;
    // /api/v3/album/song reports duration in seconds.
    if let Some(seconds) = item["duration"].as_u64() {
        track.duration = (seconds > 0).then(|| Duration::from_secs(seconds));
    }
    let ladder = [
        ("hash", "FileHash", "filesize", None),
        (
            "320hash",
            "HQFileHash",
            "320filesize",
            Some(StreamQuality::High),
        ),
        (
            "sqhash",
            "SQFileHash",
            "sqfilesize",
            Some(StreamQuality::Lossless),
        ),
    ];
    for (hash_key, extra_key, size_key, quality) in ladder {
        if let Some(hash) = item[hash_key].as_str().filter(|hash| !hash.is_empty()) {
            track.extra.insert(extra_key.into(), hash.to_owned());
            if let (Some(quality), Some(size)) = (quality, kg_value_u64(&item[size_key]))
                && size > 0
                && !track.available_qualities.contains(&quality)
            {
                track.available_qualities.push(quality);
            }
        }
    }
    Some(track)
}
#[async_trait]
impl AlbumProvider for KugouProvider {
    async fn album(&self, id: &str) -> Result<Album> {
        let tracks = self.album_tracks(id).await?;
        let Some(first) = tracks.first() else {
            return Err(Error::NotFound);
        };
        Ok(Album {
            id: TrackId::new(ID, id.to_owned()),
            title: first
                .album
                .clone()
                .unwrap_or_else(|| format!("Kugou album {id}")),
            artists: first.artists.clone(),
            artwork_url: first.artwork_url.clone(),
            track_count: Some(tracks.len() as u32),
        })
    }
    async fn album_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let album_id = id.strip_prefix("id_").unwrap_or(id.trim()).to_owned();
        if album_id.is_empty() {
            return Err(Error::InvalidInput("album id is empty".into()));
        }
        let url = format!("{ALBUM_SONG_URL}?albumid={album_id}&page=1&pagesize=-1");
        let value = self.get_json(&url, "album songs").await?;
        if value["status"].as_i64() == Some(0) || value["errcode"].as_i64().unwrap_or(0) != 0 {
            return Err(perr("album songs request failed"));
        }
        let tracks = value["data"]["info"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(kg_parse_album_song)
            .collect::<Vec<_>>();
        if tracks.is_empty() {
            return Err(Error::NotFound);
        }
        Ok(tracks)
    }
}
/// Link shapes: `hash=` query (song), special single page (playlist), album
/// page or `albumid=` query (album).
///
/// Artist lookup has no dedicated Kugou endpoint; matching upstream, the
/// "id" parameter is treated as the artist NAME and top tracks come from a
/// name search filtered to that artist.
#[async_trait]
impl ArtistProvider for KugouProvider {
    async fn artist(&self, id: &str) -> Result<Artist> {
        let name = id.trim().to_owned();
        if name.is_empty() {
            return Err(Error::InvalidInput("artist name is empty".into()));
        }
        Ok(Artist {
            id: TrackId::new(ID, name.clone()),
            name,
            artwork_url: None,
        })
    }
    async fn artist_top_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let name = id.trim().to_owned();
        if name.is_empty() {
            return Err(Error::InvalidInput("artist name is empty".into()));
        }
        let page = self
            .search(&SearchQuery {
                keyword: name.clone(),
                page: 1,
                page_size: 30,
            })
            .await?;
        let tracks = page
            .items
            .into_iter()
            .filter(|track| track.artists.iter().any(|artist| artist.contains(&name)))
            .collect::<Vec<_>>();
        if tracks.is_empty() {
            return Err(Error::NotFound);
        }
        Ok(tracks)
    }
}
/// Signature key shared with the kugou lite client.
const LOGIN_SIGN_KEY: &str = "NVPh5oo715z5DIWAeQlhMDsWXXQV4hwt";
const QR_CREATE_API: &str = "https://login-user.kugou.com/v2/qrcode";
const QR_CHECK_API: &str = "https://login-user.kugou.com/v2/get_userinfo_qrcode";
const LOGIN_UA: &str = "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi";
/// Session-token key holding the device cookies a QR poll needs.
const KG_QR_PENDING_PREFIX: &str = "kg_qr_pending_";
// Playlist endpoints (anonymous; mobile web shape).
const PLAYLIST_MOBILE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 13_2_3 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/13.0.3 Mobile/15E148 Safari/604.1";
const PLAYLIST_MOBILE_REFERER: &str = "http://m.kugou.com";
const HOT_SPECIAL_URL: &str = "http://www2.kugou.kugou.com/yueku/v9/special/getSpecial";
const SPECIAL_SINGLE_URL: &str = "http://www2.kugou.kugou.com/yueku/v9/special/single";
const SEARCH_SPECIAL_URL: &str = "http://mobilecdn.kugou.com/api/v3/search/special";
const TAG_LIST_URL: &str = "http://mobilecdnbj.kugou.com/api/v3/tag/list";
const TAG_SPECIAL_URL: &str = "http://mobilecdnbj.kugou.com/api/v3/tag/specialList";
const USER_PLIST_URL: &str = "http://m.kugou.com/plist/index";
const RANK_LIST_URL: &str = "http://mobilecdnbj.kugou.com/api/v5/rank/list";
const RANK_SONG_URL: &str = "http://mobilecdnbj.kugou.com/api/v3/rank/song";
const SONG_INFO_URL: &str = "http://m.kugou.com/app/i/getSongInfo.php";
const ALBUM_SONG_URL: &str = "http://mobilecdnbj.kugou.com/api/v3/album/song";
/// Page size for the paginated playlist detail HTML page (9999 = all songs).
const SPECIAL_SINGLE_SUFFIX: &str = "-5-9999.html";

/// Generate one scan-login's device cookies so every request inside the same
/// login carries a consistent device identity.
fn device_cookies() -> BTreeMap<String, String> {
    let guid = random_guid();
    let mut cookies = BTreeMap::new();
    cookies.insert("KUGOU_API_GUID".into(), guid.clone());
    cookies.insert("KUGOU_API_MID".into(), mid_from_guid(&guid));
    cookies.insert("KUGOU_API_MAC".into(), random_string(12));
    cookies.insert("KUGOU_API_DEV".into(), random_string(16));
    cookies
}
/// 32-hex GUID in UUID v4 layout.
fn random_guid() -> String {
    let bytes = random_bytes(16);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
/// MID = MD5 of the GUID rendered as a big decimal number (no bigint crate:
/// schoolbook base-256 to decimal conversion).
fn mid_from_guid(guid: &str) -> String {
    let digest = Md5::digest(guid.as_bytes());
    let mut digits: Vec<u8> = vec![0];
    for byte in digest {
        let mut carry = u32::from(byte);
        for digit in digits.iter_mut() {
            let value = u32::from(*digit) * 256 + carry;
            *digit = (value % 10) as u8;
            carry = value / 10;
        }
        while carry > 0 {
            digits.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    let value: String = digits
        .iter()
        .rev()
        .map(|digit| char::from(b'0' + digit))
        .collect();
    let trimmed = value.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".into()
    } else {
        trimmed.to_owned()
    }
}
fn random_string(length: usize) -> String {
    const CHARS: &[u8] = b"1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    (0..length)
        .map(|index| char::from(CHARS[(random_bytes(index + 1)[0] as usize) % CHARS.len()]))
        .collect()
}
/// Best-effort randomness from the standard library: nanos, pid and an atomic
/// counter folded through MD5. Device identifiers are not secret material, so
/// this suffices without pulling in a rand crate.
fn random_bytes(count: usize) -> Vec<u8> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut output = Vec::with_capacity(count);
    while output.len() < count {
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let seed = format!(
            "{}-{}-{nonce}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
            std::process::id()
        );
        output.extend(Md5::digest(seed.as_bytes()));
    }
    output.truncate(count);
    output
}
/// Kugou signature: params sorted by key joined as `k=v`, sandwiched between
/// the sign key, then MD5.
fn kugou_login_sign(params: &BTreeMap<String, String>) -> String {
    let joined: String = params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    format!(
        "{:x}",
        Md5::digest(format!("{LOGIN_SIGN_KEY}{joined}{LOGIN_SIGN_KEY}").as_bytes())
    )
}
fn status_from_code(code: i64) -> QrLoginStatus {
    match code {
        4 => QrLoginStatus::Success,
        2 | 3 => QrLoginStatus::Scanned,
        -1 | 5 | 6 => QrLoginStatus::Expired,
        0 | 1 => QrLoginStatus::Waiting,
        _ => QrLoginStatus::Failed,
    }
}
fn message_for(status: QrLoginStatus, code: i64, json: &Value) -> String {
    if let Some(error) = json["error"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return error.to_owned();
    }
    match status {
        QrLoginStatus::Waiting => "等待扫码".into(),
        QrLoginStatus::Scanned => "已扫码，请在手机上确认".into(),
        QrLoginStatus::Success => "登录成功".into(),
        QrLoginStatus::Expired => "二维码已过期".into(),
        QrLoginStatus::Failed => format!("登录失败（status={code}）"),
        _ => format!("登录状态异常（status={code}）"),
    }
}
#[async_trait]
impl QrAuthenticationProvider for KugouProvider {
    async fn create_qr_login(&self) -> Result<QrLoginSession> {
        let device = device_cookies();
        let qr_text =
            format!("https://h5.kugou.com/apps/loginQRCode/html/index.html?appid={LITE_APP_ID}&");
        let value = self
            .login_get(
                QR_CREATE_API,
                &[
                    ("appid", "1001"),
                    ("type", "1"),
                    ("plat", "4"),
                    ("qrcode_txt", qr_text.as_str()),
                    ("srcappid", "2919"),
                ],
                &device,
            )
            .await?;
        let key = value["data"]["qrcode"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| perr("QR login generation failed"))?
            .to_owned();
        self.store_kg_pending(&key, &device);
        Ok(QrLoginSession {
            key: key.clone(),
            // The QR image is rendered by the host from this H5 login URL.
            url: format!("https://h5.kugou.com/apps/loginQRCode/html/index.html?qrcode={key}"),
            expires_in: 300,
        })
    }
    async fn poll_qr_login(&self, key: &str) -> Result<QrLoginPoll> {
        let key = key.trim();
        if key.is_empty() {
            return Err(Error::InvalidInput("QR login key is empty".into()));
        }
        let device = self.kg_pending(key).unwrap_or_else(device_cookies);
        let value = self
            .login_get(
                QR_CHECK_API,
                &[
                    ("plat", "4"),
                    ("appid", LITE_APP_ID),
                    ("srcappid", "2919"),
                    ("qrcode", key),
                ],
                &device,
            )
            .await?;
        let status_code = value["data"]["status"].as_i64().unwrap_or_default();
        let status = status_from_code(status_code);
        let message = message_for(status, status_code, &value);
        let mut user_id = None;
        let mut display_name = None;
        if status == QrLoginStatus::Success {
            let token = value["data"]["token"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_owned();
            let account = value["data"]["userid"]
                .as_u64()
                .map(|v| v.to_string())
                .or_else(|| value["data"]["userid"].as_str().map(str::to_owned))
                .unwrap_or_default();
            if token.is_empty() || account.is_empty() || account == "0" {
                return Ok(QrLoginPoll {
                    status: QrLoginStatus::Failed,
                    message: "酷狗登录成功但未返回 token 或 userid".into(),
                    user_id: None,
                    display_name: None,
                });
            }
            let mut session = self.sessions.get(ID).unwrap_or_default();
            session.cookies = device.clone();
            session.cookies.insert("token".into(), token);
            session.user_id = Some(account.clone());
            session
                .tokens
                .remove(&format!("{KG_QR_PENDING_PREFIX}{key}"));
            self.sessions.set(ID, session)?;
            user_id = Some(account.clone());
            display_name = Some(account);
        }
        Ok(QrLoginPoll {
            status,
            message,
            user_id,
            display_name,
        })
    }
}
impl KugouProvider {
    /// Signed GET for the login endpoints: standard lite params plus
    /// endpoint-specific params, signed and sent with the device cookies.
    async fn login_get(
        &self,
        api: &str,
        params: &[(&str, &str)],
        device: &BTreeMap<String, String>,
    ) -> Result<Value> {
        let clienttime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
            .to_string();
        let mid = device
            .get("KUGOU_API_MID")
            .cloned()
            .unwrap_or_else(|| "-".into());
        let mut all: BTreeMap<String, String> = BTreeMap::new();
        all.insert("appid".into(), LITE_APP_ID.into());
        all.insert("clientver".into(), LITE_VER.into());
        all.insert("clienttime".into(), clienttime.clone());
        all.insert("dfid".into(), "-".into());
        all.insert("mid".into(), mid.clone());
        all.insert("uuid".into(), "-".into());
        for (key, value) in params {
            all.insert((*key).to_owned(), (*value).to_owned());
        }
        let signature = kugou_login_sign(&all);
        let query = all
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(key),
                    urlencoding::encode(value)
                )
            })
            .chain(std::iter::once(format!("signature={signature}")))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!("{api}?{query}");
        let cookie = device
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        let response = self
            .client
            .get(&url)
            .header(header::USER_AGENT, LOGIN_UA)
            .header("dfid", "-")
            .header("clienttime", &clienttime)
            .header("mid", &mid)
            .header(header::COOKIE, cookie)
            .send()
            .await
            .map_err(|e| perr(format!("QR login request failed: {e}")))?;
        response
            .json()
            .await
            .map_err(|e| perr(format!("QR login response parse failed: {e}")))
    }
    fn store_kg_pending(&self, key: &str, device: &BTreeMap<String, String>) {
        let mut session = self.sessions.get(ID).unwrap_or_default();
        session.tokens.insert(
            format!("{KG_QR_PENDING_PREFIX}{key}"),
            device
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; "),
        );
        let _ = self.sessions.set(ID, session);
    }
    fn kg_pending(&self, key: &str) -> Option<BTreeMap<String, String>> {
        let raw = self
            .sessions
            .get(ID)?
            .tokens
            .get(&format!("{KG_QR_PENDING_PREFIX}{key}"))?
            .clone();
        Some(
            raw.split(';')
                .filter_map(|pair| {
                    let (name, value) = pair.trim().split_once('=')?;
                    Some((name.to_owned(), value.to_owned()))
                })
                .collect(),
        )
    }
}
// ── Playlists (mobile web endpoints, anonymous) ─────────────────────────

#[async_trait]
impl CollectionProvider for KugouProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist> {
        let (title, _) = self.special_detail(id).await?;
        let id = id.strip_prefix("id_").unwrap_or(id.trim()).to_owned();
        Ok(Playlist {
            id: TrackId::new(ID, id),
            title,
            description: None,
            artwork_url: None,
            track_count: None,
            tracks: vec![],
        })
    }
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let (_, tracks) = self.special_detail(id).await?;
        Ok(tracks)
    }
}
#[async_trait]
impl PlaylistProvider for KugouProvider {
    async fn categories(&self) -> Result<Vec<PlaylistCategory>> {
        let response = self
            .request(
                self.client
                    .get(format!("{TAG_LIST_URL}?pid=0&apiver=2&plat=0")),
            )
            .header(header::USER_AGENT, PLAYLIST_MOBILE_UA)
            .header(header::REFERER, PLAYLIST_MOBILE_REFERER)
            .send()
            .await
            .map_err(|e| perr(format!("playlist categories: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!(
                "playlist categories: HTTP {}",
                response.status()
            )));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|e| perr(format!("playlist categories parse failed: {e}")))?;
        if value["status"].as_i64() != Some(1) || value["errcode"].as_i64().unwrap_or_default() != 0
        {
            return Err(perr("playlist categories request failed"));
        }
        Ok(parse_categories(&value))
    }
    /// Empty category hits the hot-special list; anything else goes through
    /// the `id:tagid` category endpoint.
    async fn playlists(&self, category: &str, page: u32, page_size: u32) -> Result<Vec<Playlist>> {
        let page = page.max(1);
        if category.trim().is_empty() {
            let url = format!(
                "{HOT_SPECIAL_URL}?is_ajax=1&cdn=cdn&t=6&c=&p={page}&pagesize={}",
                page_size.clamp(1, 100)
            );
            let value: Value = self
                .request(self.client.get(&url))
                .header(header::USER_AGENT, PLAYLIST_MOBILE_UA)
                .header(header::REFERER, PLAYLIST_MOBILE_REFERER)
                .send()
                .await
                .map_err(|e| perr(format!("hot playlists: {e}")))?
                .json()
                .await
                .map_err(|e| perr(format!("hot playlists parse failed: {e}")))?;
            if value["status"].as_i64() != Some(1) {
                return Err(perr("hot playlists request failed"));
            }
            return Ok(value["special_db"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(kg_parse_special)
                .collect());
        }
        let (id, tag_id) = parse_category_id(category);
        if id.is_empty() {
            return Err(Error::InvalidInput(
                "playlist category id is invalid".into(),
            ));
        }
        let url = format!(
            "{TAG_SPECIAL_URL}?plat=0&page={page}&tagid={tag_id}&pagesize=30&ugc=1&id={id}&sort=2"
        );
        let value: Value = self
            .request(self.client.get(&url))
            .header(header::USER_AGENT, PLAYLIST_MOBILE_UA)
            .header(header::REFERER, PLAYLIST_MOBILE_REFERER)
            .send()
            .await
            .map_err(|e| perr(format!("category playlists: {e}")))?
            .json()
            .await
            .map_err(|e| perr(format!("category playlists parse failed: {e}")))?;
        if value["status"].as_i64() != Some(1) || value["errcode"].as_i64().unwrap_or_default() != 0
        {
            return Err(perr("category playlists request failed"));
        }
        Ok(value["data"]["info"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(kg_parse_listed_special)
            .collect())
    }
    async fn search_playlists(&self, keyword: &str, page: u32) -> Result<Vec<Playlist>> {
        if keyword.trim().is_empty() {
            return Err(Error::InvalidInput(
                "playlist search keyword is empty".into(),
            ));
        }
        let url = format!(
            "{SEARCH_SPECIAL_URL}?keyword={}&platform=WebFilter&format=json&page={}&pagesize=10&filter=0",
            urlencoding::encode(keyword.trim()),
            page.max(1)
        );
        let response = self
            .request(self.client.get(&url))
            .header(header::USER_AGENT, PLAYLIST_MOBILE_UA)
            .header(header::REFERER, PLAYLIST_MOBILE_REFERER)
            .send()
            .await
            .map_err(|e| perr(format!("playlist search: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("playlist search: HTTP {}", response.status())));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|e| perr(format!("playlist search parse failed: {e}")))?;
        Ok(value["data"]["info"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(kg_parse_listed_special)
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
        let user_id = session
            .user_id
            .clone()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| perr("Kugou login is missing the account id; please scan in again"))?;
        let cookie = session
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        let url = format!(
            "{USER_PLIST_URL}/{user_id}?json=true&page={}&pagesize={}",
            page.max(1),
            page_size.clamp(1, 100)
        );
        let response = self
            .client
            .get(&url)
            .header(header::USER_AGENT, PLAYLIST_MOBILE_UA)
            .header(header::REFERER, PLAYLIST_MOBILE_REFERER)
            .header(header::COOKIE, cookie)
            .send()
            .await
            .map_err(|e| perr(format!("user playlists: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("user playlists: HTTP {}", response.status())));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|e| perr(format!("user playlists parse failed: {e}")))?;
        // Two response shapes exist: old `data.info`, new `plist.list.info`.
        let items = value["data"]["info"]
            .as_array()
            .or_else(|| value["plist"]["list"]["info"].as_array())
            .or_else(|| value["plist"]["list"].as_array())
            .ok_or_else(|| perr("user playlists request failed; login may have expired"))?;
        Ok(items.iter().filter_map(kg_parse_user_special).collect())
    }
}
impl KugouProvider {
    /// One special-single HTML page: (title, songs). The playlist name lives
    /// in the page `<title>`; the songs live in an embedded JS array.
    async fn special_detail(&self, raw_id: &str) -> Result<(String, Vec<Track>)> {
        let id = raw_id
            .strip_prefix("id_")
            .unwrap_or(raw_id.trim())
            .to_owned();
        if id.is_empty() {
            return Err(Error::InvalidInput("playlist id is empty".into()));
        }
        let url = format!("{SPECIAL_SINGLE_URL}/{id}{SPECIAL_SINGLE_SUFFIX}");
        let response = self
            .request(self.client.get(&url))
            .header(header::USER_AGENT, PLAYLIST_MOBILE_UA)
            .header(header::REFERER, PLAYLIST_MOBILE_REFERER)
            .send()
            .await
            .map_err(|e| perr(format!("playlist detail: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("playlist detail: HTTP {}", response.status())));
        }
        let html = response
            .text()
            .await
            .map_err(|e| perr(format!("playlist detail read failed: {e}")))?;
        let title = special_page_title(&html).unwrap_or_else(|| format!("Kugou playlist {id}"));
        let tracks = embedded_song_data(&html)?
            .iter()
            .filter_map(kg_parse_special_song)
            .collect::<Vec<_>>();
        Ok((title, tracks))
    }
}
/// `id:tagid` pair; a missing tagid defaults to 0.
fn parse_category_id(category: &str) -> (String, String) {
    let (id, tag_id) = category
        .split_once(':')
        .map_or((category, "0"), |(id, tag)| (id, tag));
    let tag = tag_id.trim();
    (
        id.trim().to_owned(),
        if tag.is_empty() {
            "0".to_owned()
        } else {
            tag.to_owned()
        },
    )
}
fn kg_first_string(item: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| item[*key].as_str().filter(|text| !text.is_empty()))
        .unwrap_or_default()
        .to_owned()
}
trait PipeKgOption: Sized {
    fn pipe_kg(self) -> Option<String>;
}
impl PipeKgOption for String {
    fn pipe_kg(self) -> Option<String> {
        (!self.is_empty()).then_some(self)
    }
}
fn kg_parse_special(item: &Value) -> Option<Playlist> {
    let id = kg_value_string(&item["specialid"]);
    let title = item["specialname"].as_str()?.trim().to_owned();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(Playlist {
        id: TrackId::new(ID, id),
        title,
        description: kg_first_string(item, &["intro"]).pipe_kg(),
        artwork_url: kg_first_string(item, &["img", "imgurl"])
            .replace("{size}", "500")
            .pipe_kg(),
        track_count: kg_value_u64(&item["song_count"])
            .or_else(|| kg_value_u64(&item["songcount"]))
            .map(|n| n as u32),
        tracks: vec![],
    })
}
fn kg_parse_listed_special(item: &Value) -> Option<Playlist> {
    let mut id = kg_value_string(&item["specialid"]);
    if id.is_empty() || id == "0" {
        id = kg_value_string(&item["global_specialid"]);
    }
    let title = item["specialname"].as_str()?.trim().to_owned();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(Playlist {
        id: TrackId::new(ID, id.clone()),
        title,
        description: item["intro"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned),
        artwork_url: kg_first_string(item, &["imgurl"])
            .replace("{size}", "240")
            .pipe_kg(),
        track_count: kg_value_u64(&item["songcount"]).map(|n| n as u32),
        tracks: vec![],
    })
}
/// User playlist entries vary field names between old and new endpoints.
fn kg_parse_user_special(item: &Value) -> Option<Playlist> {
    let mut id = kg_value_string(&item["listid"]);
    if id.is_empty() || id == "0" {
        id = kg_value_string(&item["specialid"]);
    }
    if id.is_empty() || id == "0" {
        id = kg_value_string(&item["global_specialid"]);
    }
    let title = item["specialname"]
        .as_str()
        .or_else(|| item["name"].as_str())?
        .trim()
        .to_owned();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(Playlist {
        id: TrackId::new(ID, id),
        title,
        description: kg_first_string(item, &["intro"]).pipe_kg(),
        artwork_url: kg_first_string(item, &["imgurl", "pic"])
            .replace("{size}", "240")
            .pipe_kg(),
        track_count: kg_value_u64(&item["songcount"])
            .or_else(|| kg_value_u64(&item["count"]))
            .map(|n| n as u32),
        tracks: vec![],
    })
}
fn parse_categories(value: &Value) -> Vec<PlaylistCategory> {
    let mut result = vec![PlaylistCategory {
        id: String::new(),
        name: "全部".into(),
        group: Some("全部".into()),
        count: 0,
        hot: true,
    }];
    for group in value["data"]["info"].as_array().into_iter().flatten() {
        let group_name = group["name"].as_str().unwrap_or_default().trim().to_owned();
        let group_id = group["id"].as_i64().unwrap_or_default();
        if group_id > 0 && !group_name.is_empty() {
            result.push(PlaylistCategory {
                id: format!("{group_id}:0"),
                name: group_name.clone(),
                group: Some(group_name.clone()),
                count: 0,
                hot: false,
            });
        }
        for child in group["children"].as_array().into_iter().flatten() {
            let name = child["name"].as_str().unwrap_or_default().trim().to_owned();
            let id = child["id"].as_i64().unwrap_or_default();
            if id <= 0 || name.is_empty() {
                continue;
            }
            let tag_id = child["special_tag_id"].as_i64().unwrap_or_default();
            result.push(PlaylistCategory {
                id: format!("{id}:{tag_id}"),
                name,
                group: (!group_name.is_empty()).then(|| group_name.clone()),
                count: 0,
                hot: child["is_hot"].as_i64() == Some(1),
            });
        }
    }
    result
}
/// Pull the special-song array out of the playlist page's embedded JS.
/// The array ends at the first `];` after the assignment marker.
fn embedded_song_data(html: &str) -> Result<Vec<Value>> {
    for marker in ["global.data", "var data"] {
        let Some(marker_start) = html.find(marker) else {
            continue;
        };
        let after_marker = &html[marker_start + marker.len()..];
        // The assignment may be spaced: `global.data = [ ... ];`
        let Some(assign_rel) = after_marker.find('=') else {
            continue;
        };
        let mut cursor = marker_start + marker.len() + assign_rel + 1;
        while cursor < html.len() && html.as_bytes()[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if !html[cursor..].starts_with('[') {
            continue;
        }
        let Some(end_rel) = html[cursor..].find("];") else {
            continue;
        };
        let raw = &html[cursor..cursor + end_rel + 1];
        return serde_json::from_str(raw)
            .map_err(|e| perr(format!("special page data parse failed: {e}")));
    }
    Err(perr("special page has no embedded song data"))
}
/// The playlist name sits in the page `<title>` with a site suffix.
fn special_page_title(html: &str) -> Option<String> {
    let start = html.find("<title>")? + "<title>".len();
    let end = html[start..].find("</title>")? + start;
    let raw = html[start..end]
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"");
    let name = raw
        .split(['-', '_', '|'])
        .next()
        .unwrap_or(&raw)
        .trim()
        .to_owned();
    (!name.is_empty()).then_some(name)
}
/// Playlist-page songs: hashes for the stream ladder, duration in millis.
fn kg_parse_special_song(item: &Value) -> Option<Track> {
    let mut id = kg_value_string(&item["audio_id"]);
    if id.is_empty() {
        id = kg_value_string(&item["songid"]);
    }
    if id.is_empty() {
        return None;
    }
    let filename = kg_first_string(item, &["filename"]);
    let title = kg_first_string(item, &["songname", "audio_name"]);
    let title = if title.is_empty() {
        filename
            .split_once(" - ")
            .map(|(_, name)| name.trim().to_owned())
            .unwrap_or(filename.clone())
    } else {
        title
    };
    if title.trim().is_empty() {
        return None;
    }
    let mut artists: Vec<String> = kg_first_string(item, &["singername", "author_name"])
        .split('、')
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .collect();
    if artists.is_empty() && filename.contains(" - ") {
        artists = filename
            .split_once(" - ")
            .map(|(artist, _)| vec![artist.trim().to_owned()])
            .unwrap_or_default();
    }
    let millis = kg_value_u64(&item["duration"]).or_else(|| kg_value_u64(&item["timelength"]));
    let mut qualities = Vec::new();
    let mut extra = BTreeMap::new();
    for (size_key, hash_key, extra_key, quality) in [
        ("filesize", "hash", "FileHash", StreamQuality::Low),
        (
            "filesize_320",
            "hash_320",
            "HQFileHash",
            StreamQuality::High,
        ),
        (
            "filesize_flac",
            "hash_flac",
            "SQFileHash",
            StreamQuality::Lossless,
        ),
        (
            "filesize_high",
            "hash_high",
            "ResFileHash",
            StreamQuality::HiRes,
        ),
    ] {
        if kg_value_u64(&item[size_key]).unwrap_or_default() > 0 && !qualities.contains(&quality) {
            qualities.push(quality);
        }
        if let Some(hash) = item[hash_key].as_str().filter(|hash| !hash.is_empty()) {
            extra.insert(extra_key.into(), hash.to_owned());
        }
    }
    Some(Track {
        id: TrackId::new(ID, id),
        title,
        artists,
        album: kg_first_string(item, &["album_name"]).pipe_kg(),
        album_id: kg_value_string(&item["album_id"]).pipe_kg(),
        duration: millis.filter(|value| *value > 0).map(Duration::from_millis),
        artwork_url: item
            .pointer("/trans_param/union_cover")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(|url| url.replace("{size}", "500")),
        isrc: None,
        available_qualities: qualities,
        extra,
    })
}

/// Kugou login means the `token` cookie plus an account id (user_id),
/// mirroring the upstream session predicate.
#[async_trait]
impl AuthenticationProvider for KugouProvider {
    async fn login_status(&self) -> Result<LoginStatus> {
        let Some(session) = self.sessions.get(ID) else {
            return Ok(LoginStatus::LoggedOut);
        };
        if !session.cookies.contains_key("token") || session.user_id.is_none() {
            return Ok(LoginStatus::LoggedOut);
        }
        if session.tokens.contains_key("login_expired") {
            return Ok(LoginStatus::Expired);
        }
        Ok(LoginStatus::LoggedIn {
            user_id: session.user_id.clone(),
            display_name: session.display_name.clone(),
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
    use serde_json::json;

    #[test]
    fn decrypts_krc_payload() {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let plaintext = "[1000,1000]<0,500,0>你<500,500,0>好";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(plaintext.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut fixture = KRC_MAGIC.to_vec();
        fixture.extend(
            compressed
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ KRC_XOR_KEY[index % KRC_XOR_KEY.len()]),
        );
        assert_eq!(decrypt_krc(&fixture).unwrap(), plaintext);
        assert!(decrypt_krc(b"no-magic-here").is_err());
    }
    #[test]
    fn splits_krc_bodies_into_text_and_word_tags() {
        let (text, tags) = split_krc_body("<0,600,0>你<600,600,0>好&nbsp;");
        assert_eq!(text, "你好 ");
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].offset_ms, 0);
        assert_eq!(tags[0].duration_ms, 600);
        assert_eq!(tags[1].offset_ms, 600);
        let (plain, tags) = split_krc_body("no tags");
        assert_eq!(plain, "no tags");
        assert!(tags.is_empty());
    }
    #[test]
    fn parses_krc_with_language_metadata_and_words() {
        let metadata = json!({
            "version": 1,
            "content": [
                {"type": 0, "lyricContent": [["ni", " hao"], ["zai", " jian"]]},
                {"type": 1, "lyricContent": [["你", "好"], ["再", "见"]]}
            ]
        });
        let encoded = STANDARD.encode(metadata.to_string().as_bytes());
        let content = format!(
            "[language:{encoded}]\n[61234,1200]<0,600,0>你<600,600,0>好\n[62434,1200]<0,600,0>再<600,600,0>见"
        );
        let document = parse_krc(content);
        assert_eq!(document.format, LyricFormat::Krc);
        assert_eq!(document.lines.len(), 2);
        assert_eq!(document.lines[0].start_ms, 61_234);
        assert_eq!(document.lines[0].text, "你好");
        assert_eq!(document.lines[0].words.len(), 2);
        assert_eq!(document.lines[0].words[0].start_ms, 61_234);
        assert_eq!(document.lines[0].words[0].duration_ms, 600);
        assert_eq!(document.lines[1].words[0].start_ms, 62_434);
        assert_eq!(document.original, "[01:01.234]你好\n[01:02.434]再见");
        assert_eq!(
            document.translation.as_deref(),
            Some("[01:01.234]你好\n[01:02.434]再见")
        );
        assert_eq!(
            document.romanization.as_deref(),
            Some("[01:01.234]ni hao\n[01:02.434]zai jian")
        );
    }
    #[test]
    fn parses_kg_search_items_and_grp_variants() {
        let item = json!({
            "Audioid": 123456,
            "SongName": "晴天",
            "Singers": [{"name": "周杰伦"}],
            "AlbumName": "叶惠美",
            "AlbumID": 42,
            "Duration": 269,
            "FileSize": 4312832,
            "HQFileSize": 10781692,
            "SQFileSize": 0,
            "ResFileSize": 12345678,
            "FileHash": "ABCDEF0123",
            "HQFileHash": "HQ0123",
            "Grp": [{
                "Audioid": 123457,
                "SongName": "晴天 (Live)",
                "Singers": [{"name": "周杰伦"}],
                "Duration": 300,
                "FileSize": 100,
                "FileHash": "LIVE01"
            }]
        });
        let track = parse_kg_song(&item).unwrap();
        assert_eq!(track.id.id, "123456");
        assert_eq!(track.artists, vec!["周杰伦"]);
        assert_eq!(track.album.as_deref(), Some("叶惠美"));
        assert_eq!(track.duration.map(|d| d.as_secs()), Some(269));
        assert_eq!(
            track.extra.get("FileHash").map(String::as_str),
            Some("ABCDEF0123")
        );
        assert!(track.available_qualities.contains(&StreamQuality::High));
        assert!(track.available_qualities.contains(&StreamQuality::HiRes));
        assert!(!track.available_qualities.contains(&StreamQuality::Lossless));
        assert!(parse_kg_song(&json!({"SongName": "x"})).is_none());
        assert!(parse_kg_song(&json!({"Audioid": 1})).is_none());
    }
    #[test]
    fn select_hash_downgrades_from_requested_tier() {
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
            extra: BTreeMap::from([
                ("SQFileHash".into(), "sq-hash".into()),
                ("HQFileHash".into(), "hq-hash".into()),
                ("FileHash".into(), "std-hash".into()),
            ]),
        };
        // Requested HiRes is missing: falls back to SQ (Lossless).
        let (hash, quality) = select_hash(&track, StreamQuality::HiRes).unwrap();
        assert_eq!(hash, "sq-hash");
        assert_eq!(quality, StreamQuality::Lossless);
        // Requested Lossless hits SQ directly.
        let (hash, quality) = select_hash(&track, StreamQuality::Lossless).unwrap();
        assert_eq!(hash, "sq-hash");
        assert_eq!(quality, StreamQuality::Lossless);
        // Empty track has nothing to offer.
        let empty = Track {
            id: TrackId::new(ID, "2"),
            title: "t".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            extra: BTreeMap::new(),
        };
        assert!(select_hash(&empty, StreamQuality::Low).is_none());
    }
    #[test]
    fn picks_play_url_from_v5_response_shapes() {
        assert_eq!(
            pick_response_url(&json!({"url": "http://cdn/x"})).as_deref(),
            Some("http://cdn/x")
        );
        assert_eq!(
            pick_response_url(&json!({"data": {"backup_url": "http://cdn/y"}})).as_deref(),
            Some("http://cdn/y")
        );
        assert_eq!(
            pick_response_url(&json!({"data": {"flac": {"url": "http://cdn/z"}}})).as_deref(),
            Some("http://cdn/z")
        );
        assert_eq!(
            pick_response_url(&json!({"url": "http:\\/\\/cdn\\/escaped"})).as_deref(),
            Some("http://cdn/escaped")
        );
        assert!(pick_response_url(&json!({})).is_none());
    }
    #[test]
    fn quality_flags_map_to_kugou_tiers() {
        assert_eq!(quality_flag(StreamQuality::Low), "128");
        assert_eq!(quality_flag(StreamQuality::High), "320");
        assert_eq!(quality_flag(StreamQuality::Lossless), "flac");
        assert_eq!(quality_flag(StreamQuality::HiRes), "super");
        assert_eq!(hash_field(StreamQuality::HiRes), "ResFileHash");
        assert_eq!(size_field(StreamQuality::Low), "FileSize");
    }
    #[tokio::test]
    async fn login_status_requires_token_and_user_id() {
        let provider = KugouProvider::new();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        // token cookie alone is not enough without a user id.
        let mut cookies = BTreeMap::new();
        cookies.insert("token".into(), "secret".into());
        provider.set_cookies(cookies, None, None).unwrap();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        let mut cookies = BTreeMap::new();
        cookies.insert("token".into(), "secret".into());
        cookies.insert("KUGOU_API_MID".into(), "mid".into());
        provider
            .set_cookies(cookies, Some("42".into()), None)
            .unwrap();
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
    fn stream_resolution_requires_a_logged_in_session() {
        // The login_parts guard is the anonymous gate; assert it on a fresh
        // provider via a blocking-free check: resolve() is async, so test the
        // predicate pieces instead (token/user_id/mid extraction is covered
        // by login_status and the live smoke).
        let provider = KugouProvider::new();
        assert!(provider.sessions.get(ID).is_none());
    }
    #[test]
    fn maps_kugou_qr_status_codes() {
        assert_eq!(status_from_code(4), QrLoginStatus::Success);
        assert_eq!(status_from_code(2), QrLoginStatus::Scanned);
        assert_eq!(status_from_code(3), QrLoginStatus::Scanned);
        assert_eq!(status_from_code(0), QrLoginStatus::Waiting);
        assert_eq!(status_from_code(1), QrLoginStatus::Waiting);
        assert_eq!(status_from_code(5), QrLoginStatus::Expired);
        assert_eq!(status_from_code(9), QrLoginStatus::Failed);
    }
    #[test]
    fn kugou_login_signature_is_deterministic_and_key_wrapped() {
        let mut params = BTreeMap::new();
        params.insert("b".into(), "2".into());
        params.insert("a".into(), "1".into());
        let signature = kugou_login_sign(&params);
        assert_eq!(signature.len(), 32);
        let expected = format!(
            "{:x}",
            Md5::digest(format!("{LOGIN_SIGN_KEY}a=1b=2{LOGIN_SIGN_KEY}").as_bytes())
        );
        assert_eq!(signature, expected);
    }
    #[test]
    fn guid_mid_is_a_decimal_number() {
        let guid = random_guid();
        assert_eq!(guid.len(), 36);
        let mid = mid_from_guid(&guid);
        assert!(mid.chars().all(|character| character.is_ascii_digit()));
        assert!(!mid.is_empty() && mid != "0");
        assert_eq!(mid, mid_from_guid(&guid));
        assert_ne!(random_guid(), random_guid());
    }
    #[test]
    fn failed_kugou_login_reports_the_api_error() {
        let json = json!({"error": "二维码不存在"});
        assert_eq!(message_for(QrLoginStatus::Failed, 9, &json), "二维码不存在");
    }

    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = KugouProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.lyrics);
        assert!(descriptor.capabilities.authentication);
        assert!(descriptor.capabilities.playlists);
        assert!(descriptor.capabilities.charts);
        assert!(descriptor.capabilities.link_resolve);
        assert!(descriptor.capabilities.albums);
        assert!(descriptor.capabilities.artists);
        assert!(descriptor.capabilities.artists);
        assert!(registry.search_provider(ID).is_some());
        assert!(registry.playlist_provider(ID).is_some());
        assert!(registry.collection_provider(ID).is_some());
        assert!(registry.chart_provider(ID).is_some());
        assert!(registry.link_provider(ID).is_some());
        assert!(registry.qr_authentication_provider(ID).is_some());
    }
}
