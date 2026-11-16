//! Bilibili provider migrated from VoiceFox's `lx-source::bili`.
//! Video-music search (BV/av references expand through the view endpoint),
//! WBI-signed DASH audio stream resolution, QR scan login and login-gated
//! favorites are implemented. Bilibili serves no lyric API (lyrics stay
//! Unsupported), and audio quality depends on the caller's login state:
//! anonymously the higher tiers may fall back to low bitrate.
use crate::{
    Error, Result,
    models::{
        AudioFormat, LyricDocument, Playlist, PlaylistCategory, StreamInfo, StreamQuality, Track,
        TrackId,
    },
    provider::{
        AuthenticationProvider, CollectionProvider, LoginStatus, LyricsProvider, MusicProvider,
        PlaylistProvider, QrAuthenticationProvider, QrLoginPoll, QrLoginSession, QrLoginStatus,
        ResolveOptions, SearchPage, SearchQuery, StreamResolver, TrackSearch,
    },
    retry::{StepOutcome, classify_http},
    session::{ProviderSession, SessionStore},
};
use async_trait::async_trait;
use md5::{Digest, Md5};
use reqwest::{Client, RequestBuilder, header};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    sync::RwLock,
    time::Duration,
};

const ID: &str = "bilibili";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:141.0) Gecko/20100101 Firefox/141.0";
const BILI_REFERER: &str = "https://www.bilibili.com/";
const SEARCH_ENDPOINT: &str = "https://api.bilibili.com/x/web-interface/wbi/search/type";
const VIEW_ENDPOINT: &str = "https://api.bilibili.com/x/web-interface/view";
const PLAY_ENDPOINT: &str = "https://api.bilibili.com/x/player/wbi/playurl";
const NAV_ENDPOINT: &str = "https://api.bilibili.com/x/web-interface/nav";
const FINGER_ENDPOINT: &str = "https://api.bilibili.com/x/frontend/finger/spi";
const QR_GENERATE_URL: &str = "https://passport.bilibili.com/x/passport-login/web/qrcode/generate";
const QR_POLL_URL: &str = "https://passport.bilibili.com/x/passport-login/web/qrcode/poll";
const FAV_FOLDER_LIST: &str = "https://api.bilibili.com/x/v3/fav/folder/created/list-all";
const FAV_RESOURCE_LIST: &str = "https://api.bilibili.com/x/v3/fav/resource/list";
/// The 64-entry permutation that scrambles img_key + sub_key into the WBI
/// mixin key; only the first 32 scrambled chars participate.
const WBI_MIXIN_KEY_TABLE: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];
/// Only interface-relevant cookies ride along; tracking cookies stay out.
const BILI_COOKIE_NAMES: &[&str] = &["SESSDATA", "bili_jct", "buvid3", "buvid4", "DedeUserID"];

#[derive(Clone)]
pub struct BilibiliProvider {
    client: Client,
    sessions: SessionStore,
    /// WBI keys are fetched from the nav endpoint and cached until -403/-412.
    wbi_keys: std::sync::Arc<RwLock<Option<(String, String)>>>,
}

impl std::fmt::Debug for BilibiliProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BilibiliProvider")
            .field("session_store", &"redacted")
            .finish()
    }
}
impl Default for BilibiliProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl BilibiliProvider {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            sessions: SessionStore::default(),
            wbi_keys: std::sync::Arc::new(RwLock::new(None)),
        }
    }
    pub fn with_session_store(mut self, sessions: SessionStore) -> Self {
        self.sessions = sessions;
        self
    }
    pub fn session_store(&self) -> SessionStore {
        self.sessions.clone()
    }
    /// Register all currently implemented Bilibili capabilities in one step.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_authentication(provider.clone());
        registry.register_qr_authentication(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_playlist(provider);
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
    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        let mut request = request
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::REFERER, BILI_REFERER);
        if let Some(session) = self.sessions.get(ID) {
            // Only interface cookies ride along; tracking cookies bloat the
            // header without affecting the API.
            let cookie = BILI_COOKIE_NAMES
                .iter()
                .filter_map(|name| {
                    session
                        .cookies
                        .get(*name)
                        .map(|value| format!("{name}={value}"))
                })
                .collect::<Vec<_>>()
                .join("; ");
            if !cookie.is_empty() {
                request = request.header(header::COOKIE, cookie);
            }
        }
        request
    }
    async fn request_json(&self, url: &str, label: &str) -> Result<Value> {
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
                    tracing::debug!("Bili {label} transport retry {}: {error}", attempt + 1);
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
            return response
                .json()
                .await
                .map_err(|e| perr(format!("{label}: {e}")));
        }
        Err(perr(format!("{label}: retry budget exhausted")))
    }
    /// Plain (unsigned) JSON GET.
    async fn get_json(
        &self,
        endpoint: &str,
        params: &[(&str, String)],
        label: &str,
    ) -> Result<Value> {
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
            endpoint.to_owned()
        } else {
            format!("{endpoint}?{query}")
        };
        self.request_json(&url, label).await
    }
    async fn wbi_keys(&self) -> Result<(String, String)> {
        if let Some(keys) = self
            .wbi_keys
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(keys);
        }
        let json = self.request_json(NAV_ENDPOINT, "wbi keys").await?;
        // The nav endpoint answers code -101 (not logged in) for anonymous
        // callers, but data.wbi_img carries the keys either way; only a
        // missing key block is fatal.
        let img_key = key_from_url(json["data"]["wbi_img"]["img_url"].as_str());
        let sub_key = key_from_url(json["data"]["wbi_img"]["sub_url"].as_str());
        if img_key.is_empty() || sub_key.is_empty() {
            return Err(perr("WBI keys are empty"));
        }
        let keys = (img_key, sub_key);
        *self.wbi_keys.write().unwrap_or_else(|e| e.into_inner()) = Some(keys.clone());
        Ok(keys)
    }
    /// Anonymous callers need a buvid fingerprint cookie before WBI-signed
    /// calls are accepted; the finger endpoint hands one out.
    async fn ensure_buvid(&self) -> Result<()> {
        if self
            .sessions
            .get(ID)
            .is_some_and(|session| session.cookies.contains_key("buvid3"))
        {
            return Ok(());
        }
        let json = self.get_json(FINGER_ENDPOINT, &[], "buvid").await?;
        let b3 = json["data"]["b_3"].as_str().map(str::to_owned);
        let b4 = json["data"]["b_4"].as_str().map(str::to_owned);
        if b3.is_none() && b4.is_none() {
            return Ok(());
        }
        let mut session = self.sessions.get(ID).unwrap_or_default();
        if let Some(value) = b3 {
            session.cookies.insert("buvid3".into(), value);
        }
        if let Some(value) = b4 {
            session.cookies.insert("buvid4".into(), value);
        }
        let _ = self.sessions.set(ID, session);
        Ok(())
    }
    /// WBI-signed JSON GET; -403/-412 trigger one key refresh and retry.
    async fn signed_get(
        &self,
        endpoint: &str,
        params: &[(&str, String)],
        label: &str,
    ) -> Result<Value> {
        self.ensure_buvid().await?;
        let json = self.signed_get_inner(endpoint, params, label).await?;
        if !matches!(json["code"].as_i64(), Some(-403) | Some(-412)) {
            return Ok(json);
        }
        *self.wbi_keys.write().unwrap_or_else(|e| e.into_inner()) = None;
        self.signed_get_inner(endpoint, params, label).await
    }
    async fn signed_get_inner(
        &self,
        endpoint: &str,
        params: &[(&str, String)],
        label: &str,
    ) -> Result<Value> {
        let keys = self.wbi_keys().await?;
        let mut all: BTreeMap<String, String> = params
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        all.insert(
            "wts".into(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string(),
        );
        let query = encode_wbi_query(&all, &keys);
        self.request_json(&format!("{endpoint}?{query}"), label)
            .await
    }
    async fn resolve_cid(&self, bvid: &str) -> Result<String> {
        let json = self
            .get_json(VIEW_ENDPOINT, &[("bvid", bvid.to_owned())], "view")
            .await?;
        if json["code"].as_i64() != Some(0) {
            return Err(perr(api_error(&json, "video info fetch failed")));
        }
        json["data"]["pages"]
            .as_array()
            .and_then(|pages| pages.first())
            .and_then(|page| page["cid"].as_u64())
            .map(|cid| cid.to_string())
            .ok_or(Error::NotFound)
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}
fn key_from_url(value: Option<&str>) -> String {
    value
        .and_then(|value| value.rsplit('/').next())
        .and_then(|value| value.split('.').next())
        .unwrap_or_default()
        .to_owned()
}
/// WBI query encoding: params sorted (BTreeMap), the reserved characters
/// `!'()*` stripped from values, then md5(query + mixin_key) as w_rid.
fn encode_wbi_query(params: &BTreeMap<String, String>, keys: &(String, String)) -> String {
    let mixed = format!("{}{}", keys.0, keys.1);
    let mixin_key: String = WBI_MIXIN_KEY_TABLE
        .iter()
        .filter_map(|index| mixed.chars().nth(*index))
        .take(32)
        .collect();
    let encoded = params
        .iter()
        .map(|(key, value)| {
            let value: String = value
                .chars()
                .filter(|character| !matches!(character, '!' | '\'' | '(' | ')' | '*'))
                .collect();
            format!(
                "{}={}",
                urlencoding::encode(key),
                urlencoding::encode(&value)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let digest = Md5::digest(format!("{encoded}{mixin_key}").as_bytes());
    format!("{encoded}&w_rid={digest:x}")
}
fn api_error(json: &Value, fallback: &str) -> String {
    let code = json["code"].as_i64().unwrap_or(-1);
    if matches!(code, -412 | -352) {
        return format!("Bilibili risk control blocked the request (code={code})");
    }
    let message = json["message"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback);
    format!("{message} (code={code})")
}
fn bili_value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
        .unwrap_or_default()
}
/// Strip HTML tags (search titles carry <em> highlights) and decode entities.
fn strip_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_tag = false;
    for character in value.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}
/// `mm:ss` / `hh:mm:ss` durations from the search endpoint.
fn parse_bili_duration(value: &str) -> Duration {
    let mut parts = value.split(':').rev();
    let seconds = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let minutes = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let hours = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    Duration::from_secs(hours * 3600 + minutes * 60 + seconds)
}
fn normalize_cover(value: Option<&str>) -> Option<String> {
    value.filter(|value| !value.is_empty()).map(|value| {
        if value.starts_with("//") {
            format!("https:{value}")
        } else {
            value.to_owned()
        }
    })
}
/// Find a BV reference: `BV` (case-insensitive) followed by 10 alphanumerics.
fn find_bvid(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("bv") {
        let start = from + pos;
        let candidate: String = input[start + 2..]
            .chars()
            .take(10)
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if candidate.len() == 10 {
            return Some(format!("BV{candidate}"));
        }
        from = start + 2;
    }
    None
}
/// Find an `av<digits>` reference.
fn find_aid(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    let pos = lower.find("av")?;
    let digits: String = input[pos + 2..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    (!digits.is_empty() && digits.len() <= 20).then_some(digits)
}
/// The `?p=`/`&p=` page selector on video links.
fn find_page_number(input: &str) -> Option<u32> {
    for marker in ["?p=", "&p=", "&amp;p="] {
        let mut from = 0;
        while let Some(pos) = input[from..].find(marker) {
            let start = from + pos + marker.len();
            let digits: String = input[start..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(page) = digits.parse::<u32>()
                && page > 0
            {
                return Some(page);
            }
            from = start;
        }
    }
    None
}
/// b23.tv short links redirect to a full video URL; the client follows them.
fn short_video_url(input: &str) -> Option<String> {
    for host in ["b23.tv", "www.b23.tv", "bili2233.cn", "www.bili2233.cn"] {
        let marker = format!("{host}/");
        if let Some(pos) = input.find(&marker) {
            let scheme_start = input[..pos]
                .rfind("http")
                .unwrap_or_else(|| pos.saturating_sub(8));
            let tail = &input[scheme_start..];
            let end = tail
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, '，' | '。' | '；' | '！' | '？' | '）' | '】')
                })
                .unwrap_or(tail.len());
            let candidate = tail[..end]
                .trim_end_matches([',', '.', ';', '!', '?', ')', ']'])
                .to_owned();
            if candidate.starts_with("http") {
                return Some(candidate);
            }
        }
    }
    None
}
/// Video-level reference parsed from a search keyword.
enum VideoReference {
    Bvid { bvid: String, page: Option<u32> },
    Aid { aid: String, page: Option<u32> },
}
fn parse_video_reference(input: &str) -> Option<VideoReference> {
    let page = find_page_number(input);
    if let Some(bvid) = find_bvid(input) {
        return Some(VideoReference::Bvid { bvid, page });
    }
    find_aid(input).map(|aid| VideoReference::Aid { aid, page })
}
/// Turn a view-endpoint response into per-page tracks; multi-page videos
/// expand so each part plays as its own track.
fn parse_video_result(json: &Value, requested_page: Option<u32>) -> Result<Vec<Track>> {
    let data = &json["data"];
    let bvid = data["bvid"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| perr("video bvid is empty"))?
        .to_owned();
    let aid = bili_value_string(&data["aid"]);
    let title = data["title"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(&bvid)
        .to_owned();
    let uploader = data["owner"]["name"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("bilibili user")
        .to_owned();
    let cover = normalize_cover(data["pic"].as_str());
    let raw_pages = data["pages"].as_array();
    let multiple_pages = raw_pages.is_some_and(|pages| pages.len() > 1);
    let mut items: Vec<Track> = raw_pages
        .into_iter()
        .flatten()
        .filter_map(|page| {
            let page_number = page["page"].as_u64().unwrap_or(1) as u32;
            if requested_page.is_some_and(|requested| requested != page_number) {
                return None;
            }
            let cid = bili_value_string(&page["cid"]);
            if cid.is_empty() {
                return None;
            }
            let part = page["part"].as_str().unwrap_or_default().trim();
            let part_title = if part.is_empty() {
                format!("P{page_number}")
            } else {
                part.to_owned()
            };
            let name = if multiple_pages {
                part_title.clone()
            } else {
                title.clone()
            };
            let id = if multiple_pages {
                format!("{bvid}-p{page_number}")
            } else {
                bvid.clone()
            };
            let mut extra = BTreeMap::new();
            extra.insert("bvid".into(), bvid.clone());
            extra.insert("cid".into(), cid);
            extra.insert("page".into(), page_number.to_string());
            extra.insert("bili_part_title".into(), part_title);
            if !aid.is_empty() {
                extra.insert("aid".into(), aid.clone());
            }
            Some(Track {
                id: TrackId::new(ID, id),
                title: name,
                artists: vec![uploader.clone()],
                album: Some(title.clone()),
                album_id: Some(bvid.clone()),
                duration: {
                    let seconds = page["duration"].as_u64().unwrap_or_default();
                    (seconds > 0).then(|| Duration::from_secs(seconds))
                },
                artwork_url: cover.clone(),
                isrc: None,
                available_qualities: vec![],
                extra,
            })
        })
        .collect();
    if items.is_empty() && requested_page.is_none() {
        let cid = bili_value_string(&data["cid"]);
        if !cid.is_empty() {
            let mut extra = BTreeMap::new();
            extra.insert("bvid".into(), bvid.clone());
            extra.insert("cid".into(), cid);
            extra.insert("page".into(), "1".into());
            if !aid.is_empty() {
                extra.insert("aid".into(), aid);
            }
            items.push(Track {
                id: TrackId::new(ID, bvid.clone()),
                title: title.clone(),
                artists: vec![uploader],
                album: Some(title),
                album_id: Some(bvid),
                duration: {
                    let seconds = data["duration"].as_u64().unwrap_or_default();
                    (seconds > 0).then(|| Duration::from_secs(seconds))
                },
                artwork_url: cover,
                isrc: None,
                available_qualities: vec![],
                extra,
            });
        }
    }
    if items.is_empty() {
        return Err(match requested_page {
            Some(page) => perr(format!("video does not contain P{page}")),
            None => perr("video page info is empty"),
        });
    }
    Ok(items)
}
fn bili_parse_search_song(item: &Value) -> Option<Track> {
    let bvid = item["bvid"].as_str()?.trim().to_owned();
    if bvid.is_empty() {
        return None;
    }
    let title = strip_html(item["title"].as_str().unwrap_or_default());
    let mut extra = BTreeMap::new();
    extra.insert("bvid".into(), bvid.clone());
    if let Some(cid) = item["cid"].as_u64() {
        extra.insert("cid".into(), cid.to_string());
    }
    if let Some(aid) = item["aid"].as_u64() {
        extra.insert("aid".into(), aid.to_string());
    }
    Some(Track {
        id: TrackId::new(ID, bvid.clone()),
        title: title.clone(),
        artists: vec![
            item["author"]
                .as_str()
                .unwrap_or("bilibili user")
                .to_owned(),
        ],
        album: Some(title),
        album_id: Some(bvid),
        duration: {
            let seconds = parse_bili_duration(item["duration"].as_str().unwrap_or_default());
            (seconds.as_secs() > 0).then_some(seconds)
        },
        artwork_url: normalize_cover(item["pic"].as_str()),
        isrc: None,
        available_qualities: vec![],
        extra,
    })
}
/// DASH audio selection: gather audio/dolby/flac candidates, pick by
/// bandwidth (closest to 128k for Low, max for higher tiers).
struct SelectedAudio {
    url: String,
    quality: StreamQuality,
}
fn append_audio_candidates<'a>(value: &'a Value, candidates: &mut Vec<&'a Value>) {
    if let Some(items) = value.as_array() {
        candidates.extend(items);
    } else if value.is_object() {
        candidates.push(value);
    }
}
fn stream_url(item: &Value) -> Option<String> {
    item["baseUrl"]
        .as_str()
        .or_else(|| item["base_url"].as_str())
        .or_else(|| {
            item["backupUrl"]
                .as_array()
                .or_else(|| item["backup_url"].as_array())
                .and_then(|urls| urls.first())
                .and_then(Value::as_str)
        })
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
}
fn choose_audio(json: &Value, requested: StreamQuality) -> Option<SelectedAudio> {
    let mut candidates = Vec::new();
    append_audio_candidates(&json["data"]["dash"]["audio"], &mut candidates);
    append_audio_candidates(&json["data"]["dash"]["dolby"]["audio"], &mut candidates);
    append_audio_candidates(&json["data"]["dash"]["flac"]["audio"], &mut candidates);
    candidates.retain(|item| stream_url(item).is_some());
    let selected = match requested {
        StreamQuality::Low => candidates.iter().min_by_key(|item| {
            item["bandwidth"]
                .as_u64()
                .unwrap_or_default()
                .abs_diff(128_000)
        }),
        _ => candidates
            .iter()
            .max_by_key(|item| item["bandwidth"].as_u64().unwrap_or_default()),
    }?;
    let bandwidth = selected["bandwidth"].as_u64().unwrap_or_default();
    Some(SelectedAudio {
        url: stream_url(selected)?,
        quality: if bandwidth > 160_000 {
            StreamQuality::High
        } else {
            StreamQuality::Low
        },
    })
}
fn quality_qn(quality: StreamQuality) -> u32 {
    match quality {
        StreamQuality::Low => 32,
        _ => 80,
    }
}
impl MusicProvider for BilibiliProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        "哔哩哔哩"
    }
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            authentication: true,
            playlists: true,
            ..Default::default()
        }
    }
}
#[async_trait]
impl TrackSearch for BilibiliProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        let keyword = q.keyword.trim();
        if keyword.is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        // BV/av references and b23.tv short links resolve through the view
        // endpoint instead of keyword search.
        if let Some(reference) = self.resolve_video_reference(keyword).await? {
            let (params, requested_page) = match reference {
                VideoReference::Bvid { bvid, page } => (vec![("bvid", bvid)], page),
                VideoReference::Aid { aid, page } => (vec![("aid", aid)], page),
            };
            let json = self.get_json(VIEW_ENDPOINT, &params, "view").await?;
            if json["code"].as_i64() != Some(0) {
                return Err(perr(api_error(&json, "video fetch failed")));
            }
            return Ok(SearchPage {
                items: parse_video_result(&json, requested_page)?,
                total: None,
                has_more: false,
            });
        }
        let json = self
            .signed_get(
                SEARCH_ENDPOINT,
                &[
                    ("search_type", "video".into()),
                    ("keyword", keyword.to_owned()),
                    ("page", q.page.max(1).to_string()),
                    ("page_size", q.page_size.clamp(1, 50).to_string()),
                    // tids=3 is the music zone.
                    ("tids", "3".into()),
                ],
                "search",
            )
            .await?;
        if json["code"].as_i64() != Some(0) {
            return Err(perr(api_error(&json, "search failed")));
        }
        let items = json["data"]["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(bili_parse_search_song)
            .collect::<Vec<_>>();
        let total = json["data"]["numResults"]
            .as_u64()
            .unwrap_or(items.len() as u64);
        let page_size = json["data"]["pagesize"]
            .as_u64()
            .filter(|value| *value > 0)
            .unwrap_or(items.len() as u64);
        let has_more = !items.is_empty() && u64::from(q.page.max(1)) * page_size < total;
        Ok(SearchPage {
            items,
            total: Some(total),
            has_more,
        })
    }
}
impl BilibiliProvider {
    async fn resolve_video_reference(&self, input: &str) -> Result<Option<VideoReference>> {
        if let Some(reference) = parse_video_reference(input) {
            return Ok(Some(reference));
        }
        let Some(short_url) = short_video_url(input) else {
            return Ok(None);
        };
        let response = self
            .client
            .get(&short_url)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::REFERER, BILI_REFERER)
            .send()
            .await
            .map_err(|e| perr(format!("short link expansion failed: {e}")))?;
        let final_url = response.url().to_string();
        parse_video_reference(&final_url)
            .map(Some)
            .ok_or_else(|| perr("short link contains no BV/av reference"))
    }
}
#[async_trait]
impl StreamResolver for BilibiliProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to Bilibili".into(),
            ));
        }
        let bvid = track
            .extra
            .get("bvid")
            .cloned()
            .unwrap_or_else(|| track.id.id.clone());
        let cid = match track.extra.get("cid").filter(|value| !value.is_empty()) {
            Some(cid) => cid.clone(),
            None => self.resolve_cid(&bvid).await?,
        };
        let json = self
            .signed_get(
                PLAY_ENDPOINT,
                &[
                    ("bvid", bvid.clone()),
                    ("cid", cid),
                    ("qn", quality_qn(options.quality).to_string()),
                    ("fnval", "16".into()),
                    ("fnver", "0".into()),
                    ("fourk", "1".into()),
                    ("try_look", "1".into()),
                    ("voice_balance", "1".into()),
                ],
                "play url",
            )
            .await?;
        if json["code"].as_i64() != Some(0) {
            return Err(perr(api_error(&json, "play url request failed")));
        }
        let Some(selected) = choose_audio(&json, options.quality) else {
            return Err(Error::NotFound);
        };
        Ok(StreamInfo {
            url: selected.url,
            quality: selected.quality,
            format: AudioFormat::Aac,
            duration: track.duration,
            expires_at_unix: None,
            content_length: None,
            checksum: None,
            // The DASH CDN refuses requests without the site Referer.
            headers: vec![
                ("Referer".into(), BILI_REFERER.into()),
                ("User-Agent".into(), USER_AGENT.into()),
            ],
            fallback_urls: vec![],
            index_range: None,
            init_range: None,
            max_chunk_size: None,
        })
    }
}
#[async_trait]
impl LyricsProvider for BilibiliProvider {
    async fn lyrics(&self, _track: &Track) -> Result<Option<LyricDocument>> {
        // Bilibili exposes no lyric API; the capability stays false.
        Err(Error::Unsupported(ID.into()))
    }
}
/// SESSDATA is the login credential; other cookies are secondary.
#[async_trait]
impl AuthenticationProvider for BilibiliProvider {
    async fn login_status(&self) -> Result<LoginStatus> {
        let Some(session) = self.sessions.get(ID) else {
            return Ok(LoginStatus::LoggedOut);
        };
        if !session.cookies.contains_key("SESSDATA") {
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
#[async_trait]
impl QrAuthenticationProvider for BilibiliProvider {
    async fn create_qr_login(&self) -> Result<QrLoginSession> {
        let json = self.request_json(QR_GENERATE_URL, "QR generate").await?;
        if json["code"].as_i64() != Some(0) {
            return Err(perr(api_error(&json, "QR generation failed")));
        }
        let url = json["data"]["url"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| perr("QR url is empty"))?
            .to_owned();
        let key = json["data"]["qrcode_key"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| perr("QR key is empty"))?
            .to_owned();
        Ok(QrLoginSession {
            key,
            url,
            expires_in: 180,
        })
    }
    async fn poll_qr_login(&self, key: &str) -> Result<QrLoginPoll> {
        let key = key.trim();
        if key.is_empty() {
            return Err(Error::InvalidInput("QR login key is empty".into()));
        }
        let url = format!("{QR_POLL_URL}?qrcode_key={}", urlencoding::encode(key));
        let response = self
            .client
            .get(&url)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::REFERER, BILI_REFERER)
            .send()
            .await
            .map_err(|e| perr(format!("QR poll request failed: {e}")))?;
        // Success cookies arrive in the Set-Cookie headers of this response.
        let headers = response.headers().clone();
        let json: Value = response
            .json()
            .await
            .map_err(|e| perr(format!("QR poll parse failed: {e}")))?;
        // The outer code is HTTP-level; data.code carries the scan state.
        if json["code"].as_i64() != Some(0) {
            return Err(perr(api_error(&json, "QR poll failed")));
        }
        let data = &json["data"];
        let status = match data["code"].as_i64().unwrap_or(-1) {
            0 => QrLoginStatus::Success,
            86101 => QrLoginStatus::Waiting,
            86090 => QrLoginStatus::Scanned,
            86038 => QrLoginStatus::Expired,
            _ => {
                return Err(perr(format!(
                    "QR poll failed (code={})",
                    data["code"].as_i64().unwrap_or(-1)
                )));
            }
        };
        let mut user_id = None;
        let mut display_name = None;
        if status == QrLoginStatus::Success {
            self.capture_login_session(&headers, data)?;
            // Confirm the session through the nav endpoint.
            let nav = self.get_json(NAV_ENDPOINT, &[], "login status").await?;
            if nav["code"].as_i64() != Some(0) || nav["data"]["isLogin"] != Value::Bool(true) {
                return Ok(QrLoginPoll {
                    status: QrLoginStatus::Failed,
                    message: "session check failed; please scan again".into(),
                    user_id: None,
                    display_name: None,
                });
            }
            display_name = nav["data"]["uname"].as_str().map(str::to_owned);
            user_id = Some(bili_value_string(&nav["data"]["mid"]));
            let mut session = self.sessions.get(ID).unwrap_or_default();
            session.user_id = user_id.clone();
            session.display_name = display_name.clone();
            self.sessions.set(ID, session)?;
        }
        Ok(QrLoginPoll {
            status,
            message: match status {
                QrLoginStatus::Waiting => "等待扫码".into(),
                QrLoginStatus::Scanned => "已扫码，请在手机上确认".into(),
                QrLoginStatus::Success => "登录成功".into(),
                QrLoginStatus::Expired => "二维码已过期".into(),
                _ => "登录失败".into(),
            },
            user_id,
            display_name,
        })
    }
}
impl BilibiliProvider {
    /// Success Set-Cookie headers plus the data.url query both carry the
    /// session cookies; SESSDATA must be present or the login is void.
    fn capture_login_session(
        &self,
        headers: &reqwest::header::HeaderMap,
        data: &Value,
    ) -> Result<()> {
        let mut session = self.sessions.get(ID).unwrap_or_default();
        for value in headers.get_all(header::SET_COOKIE) {
            if let Ok(value) = value.to_str()
                && let Some(pair) = value.split(';').next()
                && let Some((name, value)) = pair.split_once('=')
                && !name.trim().is_empty()
            {
                session
                    .cookies
                    .insert(name.trim().into(), value.trim().into());
            }
        }
        if let Some(url) = data["url"].as_str()
            && !url.is_empty()
        {
            for pair in url.split('?').nth(1).unwrap_or_default().split('&') {
                if let Some((name, value)) = pair.split_once('=') {
                    let value = urlencoding::decode(value)
                        .map(|decoded| decoded.into_owned())
                        .unwrap_or_else(|_| value.to_owned());
                    session.cookies.insert(name.into(), value);
                }
            }
        }
        if let Some(value) = data["refresh_token"].as_str()
            && !value.is_empty()
        {
            session.tokens.insert("refresh_token".into(), value.into());
        }
        if !session.cookies.contains_key("SESSDATA") {
            return Err(perr("login succeeded but no session cookie arrived"));
        }
        self.sessions.set(ID, session)?;
        Ok(())
    }
}
#[async_trait]
impl CollectionProvider for BilibiliProvider {
    async fn playlist(&self, _id: &str) -> Result<Playlist> {
        Err(Error::Unsupported(ID.into()))
    }
    /// Favorites listing; the Bilibili API only exposes created folders to
    /// the logged-in owner.
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        self.ensure_buvid().await?;
        let mut all = Vec::new();
        let mut seen = HashSet::new();
        let mut current_page = 1u32;
        const PAGE_SIZE: u32 = 20;
        const MAX_PAGES: u32 = 100;
        loop {
            let json = self
                .get_json(
                    FAV_RESOURCE_LIST,
                    &[
                        ("media_id", id.to_owned()),
                        ("pn", current_page.to_string()),
                        ("ps", PAGE_SIZE.to_string()),
                        ("type", "0".into()),
                    ],
                    "favorites",
                )
                .await?;
            if json["code"].as_i64() != Some(0) {
                return Err(perr(api_error(&json, "favorites fetch failed")));
            }
            let medias = json["data"]["medias"].as_array();
            let page_empty = medias.map(Vec::is_empty).unwrap_or(true);
            if let Some(medias) = medias {
                for item in medias {
                    let Some(bvid) = item["bvid"]
                        .as_str()
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                    else {
                        continue;
                    };
                    let mut extra = BTreeMap::new();
                    extra.insert("bvid".into(), bvid.to_owned());
                    let cid = bili_value_string(&item["cid"]);
                    if !cid.is_empty() {
                        extra.insert("cid".into(), cid);
                    }
                    let track = Track {
                        id: TrackId::new(ID, bvid.to_owned()),
                        title: item["title"].as_str().unwrap_or_default().to_owned(),
                        artists: vec![
                            item["upper"]["name"]
                                .as_str()
                                .unwrap_or("bilibili user")
                                .to_owned(),
                        ],
                        album: None,
                        album_id: None,
                        duration: {
                            let seconds = item["duration"].as_u64().unwrap_or_default();
                            (seconds > 0).then(|| Duration::from_secs(seconds))
                        },
                        artwork_url: normalize_cover(item["cover"].as_str()),
                        isrc: None,
                        available_qualities: vec![],
                        extra,
                    };
                    if seen.insert(track.id.id.clone()) {
                        all.push(track);
                    }
                }
            }
            let has_more = json["data"]["has_more"].as_bool().unwrap_or(false);
            if !has_more || page_empty || current_page >= MAX_PAGES {
                break;
            }
            current_page += 1;
        }
        Ok(all)
    }
}
#[async_trait]
impl PlaylistProvider for BilibiliProvider {
    /// Created favorite folders; login required by the API.
    async fn categories(&self) -> Result<Vec<PlaylistCategory>> {
        Err(Error::Unsupported(ID.into()))
    }
    /// The Bilibili favorites API doubles as the folder list.
    async fn playlists(
        &self,
        _category: &str,
        _page: u32,
        _page_size: u32,
    ) -> Result<Vec<Playlist>> {
        self.ensure_buvid().await?;
        let session = self
            .sessions
            .get(ID)
            .ok_or_else(|| perr("please log in first"))?;
        if !session.cookies.contains_key("SESSDATA") {
            return Err(perr("please log in first"));
        }
        if session.tokens.contains_key("login_expired") {
            return Err(perr("login session expired"));
        }
        let mid = session
            .user_id
            .clone()
            .ok_or_else(|| perr("Bilibili login is missing the account id"))?;
        let json = self
            .get_json(
                FAV_FOLDER_LIST,
                &[("up_mid", mid), ("type", "2".into())],
                "favorite folders",
            )
            .await?;
        if json["code"].as_i64() != Some(0) {
            return Err(perr(api_error(&json, "favorite folders fetch failed")));
        }
        Ok(json["data"]["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|folder| {
                let id = folder["id"].as_u64()?.to_string();
                let title = folder["title"].as_str()?.to_owned();
                Some(Playlist {
                    id: TrackId::new(ID, id.clone()),
                    title,
                    description: None,
                    artwork_url: normalize_url(folder["cover"].as_str()),
                    track_count: folder["media_count"].as_u64().map(|n| n as u32),
                    tracks: vec![],
                })
            })
            .collect())
    }
    async fn search_playlists(&self, _keyword: &str, _page: u32) -> Result<Vec<Playlist>> {
        Err(Error::Unsupported(ID.into()))
    }
    async fn user_playlists(&self, _page: u32, _page_size: u32) -> Result<Vec<Playlist>> {
        // Bilibili favorites are the user playlists; handled via playlists().
        Err(Error::Unsupported(ID.into()))
    }
}
fn normalize_url(value: Option<&str>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty()).map(|value| {
        value
            .strip_prefix("//")
            .map_or_else(|| value.to_owned(), |url| format!("https://{url}"))
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requested_quality_selects_different_audio_streams() {
        let json = json!({
            "data": {"dash": {"audio": [
                {"bandwidth": 64_000, "baseUrl": "https://cdn/64"},
                {"bandwidth": 128_000, "baseUrl": "https://cdn/128"},
                {"bandwidth": 192_000, "baseUrl": "https://cdn/192"}
            ]}}
        });
        assert_eq!(
            choose_audio(&json, StreamQuality::Low).unwrap().url,
            "https://cdn/128"
        );
        assert_eq!(
            choose_audio(&json, StreamQuality::High).unwrap().url,
            "https://cdn/192"
        );
        assert_eq!(
            choose_audio(&json, StreamQuality::Low).unwrap().quality,
            StreamQuality::Low
        );
    }
    #[test]
    fn wbi_query_is_sorted_signed_and_filtered() {
        let keys = ("a".repeat(32), "b".repeat(32));
        let mut params = BTreeMap::new();
        params.insert("keyword".into(), "晴天".into());
        params.insert("page".into(), "1".into());
        params.insert("weird".into(), "va!l(u)e".into());
        let query = encode_wbi_query(&params, &keys);
        assert!(query.starts_with("keyword="));
        assert!(query.contains("&page=1"));
        // Reserved characters are stripped before encoding.
        assert!(query.contains("weird=value"));
        assert!(query.contains("&w_rid="));
    }
    #[test]
    fn parses_video_references() {
        assert_eq!(find_bvid("BV1xx411c7mD"), Some("BV1xx411c7mD".into()));
        assert_eq!(
            find_bvid("https://www.bilibili.com/video/bv1xx411c7md/?p=2"),
            Some("BV1xx411c7md".into())
        );
        assert_eq!(
            find_aid("https://www.bilibili.com/video/av170001?p=3"),
            Some("170001".into())
        );
        assert_eq!(
            find_page_number("https://www.bilibili.com/video/BV1xx411c7mD?p=2"),
            Some(2)
        );
        assert!(find_bvid("周杰伦 晴天").is_none());
    }
    #[test]
    fn detects_b23_short_links() {
        assert_eq!(
            short_video_url("复制链接 https://b23.tv/abc123，打开哔哩哔哩"),
            Some("https://b23.tv/abc123".into())
        );
        assert!(short_video_url("没有链接").is_none());
    }
    #[test]
    fn strips_search_title_html() {
        assert_eq!(strip_html("测试<em>视频</em>"), "测试视频");
        assert_eq!(strip_html("a&amp;b"), "a&b");
    }
    #[test]
    fn video_pages_expand_into_tracks() {
        let json = json!({
            "code": 0,
            "data": {
                "bvid": "BV1xx411c7mD",
                "aid": 170001,
                "title": "测试视频",
                "pic": "//example.com/cover.jpg",
                "owner": {"name": "UP主", "mid": 42},
                "pages": [
                    {"cid": 1001, "page": 1, "part": "第一段", "duration": 60},
                    {"cid": 1002, "page": 2, "part": "第二段", "duration": 90}
                ]
            }
        });
        let tracks = parse_video_result(&json, None).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "第一段");
        assert_eq!(tracks[1].extra["cid"], "1002");
        assert_eq!(tracks[0].album.as_deref(), Some("测试视频"));
        assert_eq!(
            tracks[0].artwork_url.as_deref(),
            Some("https://example.com/cover.jpg")
        );
        // A requested page filters.
        let tracks = parse_video_result(&json, Some(2)).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id.id, "BV1xx411c7mD-p2");
    }
    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = BilibiliProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.authentication);
        assert!(descriptor.capabilities.playlists);
        // No lyric API exists on Bilibili.
        assert!(!descriptor.capabilities.lyrics);
        assert!(!descriptor.capabilities.albums);
        assert!(!descriptor.capabilities.charts);
        assert!(registry.stream_provider(ID).is_some());
        assert!(registry.qr_authentication_provider(ID).is_some());
    }
}
