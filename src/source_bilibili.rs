//! Bilibili provider migrated from VoiceFox's `lx-source::bili`.
//! Video-music search (BV/av references expand through the view endpoint),
//! WBI-signed DASH audio stream resolution, QR scan login, login-gated
//! favorites and the chart module (four hardcoded boards: three unsigned
//! endpoints plus the WBI-signed ranking/v2 board) are implemented.
//! Bilibili serves no lyric API (lyrics stay Unsupported), and audio quality
//! depends on the caller's login state: anonymously the higher tiers may
//! fall back to low bitrate.
//!
//! HTTP now goes through the unified transport layer (`crate::transport`):
//! the `request_json` backoff loop and the raw `reqwest::Client` are merged
//! into `Http::send_raw` under `RetryPolicy::standard()` (TRANSPORT_DESIGN.md
//! §7.5), while the platform-specific concerns stay here — WBI signing,
//! the -403/-412 key-rotation single retry, buvid bootstrap, the cookie-name
//! whitelist, b23.tv short-link expansion and QR Set-Cookie harvesting.
//! Declared error-text drift: a JSON body decode failure now surfaces the
//! raw serde message without reqwest's "error decoding response body: "
//! wrapper.
//!
//! Auth wiring (AUTH_REGISTRY_DESIGN.md §8 slice 5 / §9.4): the API's -101
//! "账号未登录" verdict marks the stored session expired and surfaces the
//! classified `Error::SessionExpired` when SESSDATA rode along (anonymous
//! callers get -101 by design and keep the raw body), a -403/-412 gate code
//! that survives the key-rotation retry classifies as `PermissionDenied` /
//! `RateLimited`, and `login_status` reports through the derived
//! [`AuthState`] (the Expired bit is now reachable).
use crate::{
    Error, ErrorDetail, Result,
    models::{
        AudioFormat, Chart, ChartEntry, FallbackEndpoint, LyricDocument, Playlist,
        PlaylistCategory, StreamInfo, StreamQuality, Track, TrackId,
    },
    provider::{
        AuthState, AuthenticationProvider, ChartProvider, CollectionProvider, LoginStatus,
        LyricsProvider, MusicProvider, PlaylistProvider, QrAuthenticationProvider, QrLoginKind,
        QrLoginPoll, QrLoginSession, QrLoginStatus, ResolveOptions, SearchPage, SearchQuery,
        StreamResolver, TrackSearch,
    },
    session::{ProviderSession, SessionStore},
    transport::{ClientProfile, Http, RetryPolicy, TransportRequest},
};
use async_trait::async_trait;
use md5::{Digest, Md5};
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
/// Chart board endpoints (voicefox bili/leaderboard.rs:11-14): three
/// unsigned endpoints plus the WBI-signed ranking/v2 board.
const MUSIC_HOT_ENDPOINT: &str =
    "https://api.bilibili.com/x/centralization/interface/music/hot/rank";
const REGION_RECOMMEND_ENDPOINT: &str = "https://api.bilibili.com/x/web-interface/region/feed/rcmd";
const POPULAR_ENDPOINT: &str = "https://api.bilibili.com/x/web-interface/popular";
const RANKING_V2_ENDPOINT: &str = "https://api.bilibili.com/x/web-interface/ranking/v2";
/// Pinned chart page size: the ChartProvider trait has no page parameters
/// (docs/MIGRATION.md defers chart pagination), so each board serves one
/// pinned page. 50 is the popular board's server maximum (upstream ps
/// clamps to 50); the recommend board clamps to its native page size.
const CHART_PAGE_SIZE: u32 = 50;
/// The region recommend endpoint rejects oversized request_cnt values; 20 is
/// its native page size ("Azusa uses the endpoint's native page size of 20",
/// leaderboard.rs:88-90). from_region=1003 carries over as an opaque
/// constant.
const RECOMMEND_PAGE_SIZE: u32 = 20;
/// The 64-entry permutation that scrambles img_key + sub_key into the WBI
/// mixin key; only the first 32 scrambled chars participate.
const WBI_MIXIN_KEY_TABLE: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];
/// Only interface-relevant cookies ride along; tracking cookies stay out.
const BILI_COOKIE_NAMES: &[&str] = &["SESSDATA", "bili_jct", "buvid3", "buvid4", "DedeUserID"];
/// Bilibili's "账号未登录" API verdict: the presented credentials were
/// rejected. Anonymous callers receive it by design (the nav endpoint hands
/// out WBI keys on exactly this shape), so it only marks session expiry when
/// the request actually carried SESSDATA.
const CODE_NOT_LOGGED_IN: i64 = -101;

#[derive(Clone)]
pub struct BilibiliProvider {
    http: Http,
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
        // Client construction moved onto the unified transport layer: the
        // profile carries exactly today's knob (15s total timeout,
        // redirect-follow default, no client-level UA — the UA stays
        // request-level); the build fallback shape lives in `build_client`
        // (TRANSPORT_DESIGN.md §4.3).
        Self {
            http: Http::from_profile(&ClientProfile::provider_default()),
            sessions: SessionStore::default(),
            wbi_keys: std::sync::Arc::new(RwLock::new(None)),
        }
    }

    /// Test seam: inject a transport handle (a mock in unit tests). The
    /// production constructor is [`BilibiliProvider::new`]. Test-only so
    /// non-test builds see no dead constructor.
    #[cfg(test)]
    pub fn with_transport(http: Http) -> Self {
        Self {
            http,
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
    /// Derive the unified [`AuthState`] from the stored session (no network
    /// I/O; the netease/qq/kugou wiring, AUTH_REGISTRY_DESIGN.md §2). The
    /// `login_expired` token written by [`Self::mark_session_expired`] is the
    /// Expired bit; a session holding the login cookie (`SESSDATA`) is
    /// `Authenticated`; anything else is `LoggedOut`.
    fn auth_state(&self) -> AuthState {
        match self.sessions.get(ID) {
            None => AuthState::LoggedOut,
            Some(session) if !session.cookies.contains_key("SESSDATA") => AuthState::LoggedOut,
            Some(session) if session.tokens.contains_key("login_expired") => AuthState::Expired,
            Some(_) => AuthState::Authenticated,
        }
    }
    /// Register all currently implemented Bilibili capabilities in one step.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_authentication(provider.clone());
        registry.register_qr_authentication(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_playlist(provider.clone());
        registry.register_chart(provider);
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
    /// Flag the current session as expired so `auth_state`/`login_status`
    /// report it, and return the classified [`Error::SessionExpired`] for the
    /// caller to propagate (the netease/qq/kugou landing shape, §9.2).
    /// Idempotent: a second mark on an already-expired session is a no-op
    /// write plus the same error (§2.2 self-transition).
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
    /// Adds the request-level headers (UA, Referer) and the whitelisted
    /// session cookies to a [`TransportRequest`]. Platform-specific and kept
    /// in the provider: the cookie-name filter (BILI_COOKIE_NAMES) is
    /// bilibili policy (TRANSPORT_DESIGN.md §1); the 15s client timeout comes
    /// from the profile.
    fn request(&self, request: TransportRequest) -> TransportRequest {
        let mut request = request
            .header("user-agent", USER_AGENT)
            .header("referer", BILI_REFERER);
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
                request = request.header("cookie", cookie);
            }
        }
        request
    }
    /// Raw JSON GET through the unified transport layer (`send_raw` +
    /// provider-side parse, TRANSPORT_DESIGN.md §7.5): transport errors,
    /// 429/5xx and `Retry-After` all retry under `RetryPolicy::standard()`
    /// with the engine's exponential backoff + jitter, replacing the fixed
    /// `[600,1200,2400,4000]` table. The send budget is unchanged (at most 5
    /// sends); the backoff shape change is declared in TRANSPORT_DESIGN.md
    /// §5.5 row 1, and body-read failures mid-response now retry as
    /// `TransportError::Transport` instead of dying terminally inside the
    /// old `.json()` call (§5.5 row 4). Classification stays in `retry`
    /// (`classify_http`); the JSON parse is provider-side exactly as before,
    /// so the missing-code semantics of every call site (each checks
    /// `json["code"]` itself) are untouched.
    async fn request_json(&self, url: &str, label: &str) -> Result<Value> {
        let request = self.request(TransportRequest::get(url));
        let response = self
            .http
            .send_raw(&request, &RetryPolicy::standard())
            .await
            .map_err(|error| {
                tracing::debug!("Bili {label} failed: {error}");
                perr(format!("{label}: {error}"))
            })?;
        let value: Value =
            serde_json::from_slice(&response.body).map_err(|e| perr(format!("{label}: {e}")))?;
        // API login-invalid write point (AUTH_REGISTRY_DESIGN.md §8 slice 5):
        // code -101 means the presented credentials were rejected. When the
        // request actually carried SESSDATA that is a session expiry — mark
        // the session and surface the classified `SessionExpired` (the
        // netease/qq/kugou mark pattern). Anonymous callers receive -101 by
        // design (the nav/WBI-keys shape depends on it), so without SESSDATA
        // the body passes through untouched. Declared drift: a stored but
        // rejected session now turns the previously tolerated anonymous-shape
        // nav/WBI-keys fetch into `SessionExpired` — an honest re-login
        // prompt instead of silently degrading to anonymous.
        if value["code"].as_i64() == Some(CODE_NOT_LOGGED_IN) && self.session_holds_login() {
            return Err(self.mark_session_expired(Some(ErrorDetail::new(label))));
        }
        Ok(value)
    }
    /// Whether the stored session carries the SESSDATA login credential (the
    /// request side of the [`Self::auth_state`] predicate — the cookie
    /// whitelist is what actually rides along).
    fn session_holds_login(&self) -> bool {
        self.sessions
            .get(ID)
            .is_some_and(|session| session.cookies.contains_key("SESSDATA"))
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
    /// WBI-signed JSON GET; -403/-412 trigger one key refresh and retry. A
    /// gate code that survives the re-sign no longer falls through to the
    /// call site's Provider string: -403 returns `PermissionDenied` and -412
    /// returns `RateLimited` ([`gate_error`]); any other body keeps today's
    /// call-site shape.
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
        let retried = self.signed_get_inner(endpoint, params, label).await?;
        match retried["code"].as_i64() {
            Some(code) if matches!(code, -403 | -412) => Err(gate_error(code, label)),
            _ => Ok(retried),
        }
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
/// The exhausted gate verdict (`signed_get`, AUTH_REGISTRY_DESIGN.md §8
/// slice 5): after the single key-rotation retry the risk-control codes leave
/// the Provider-string channel — -403 ("访问权限不足", access refused) lands
/// on `Error::PermissionDenied` with the typed code slot, -412 ("请求被拦截",
/// bilibili's rate-limit interception) on `Error::RateLimited`, the only
/// retryable one of the pair. The call label rides in the detail slot (§1.2).
fn gate_error(code: i64, label: &str) -> Error {
    match code {
        -403 => Error::PermissionDenied {
            provider: Some(ID.into()),
            code: Some(code),
            detail: Some(ErrorDetail::new(label)),
        },
        // The body carries no Retry-After, so the slot stays None.
        _ => Error::RateLimited {
            provider: Some(ID.into()),
            retry_after: None,
            detail: Some(ErrorDetail::new(label)),
        },
    }
}
/// The passport `qrcode/poll` scan-state code (`data.code`) → flow status:
/// 0 success, 86101 not scanned yet, 86090 scanned unconfirmed, 86038 QR
/// expired. `None` = undocumented code, which `poll_qr_login` reports as an
/// error exactly as before. Bilibili's QR protocol carries no 998-style
/// alias: 86038 is the one expired code (upstream voicefox
/// `source/src/bili/mod.rs:239-241` pins the same four).
fn qr_status_from_code(code: i64) -> Option<QrLoginStatus> {
    match code {
        0 => Some(QrLoginStatus::Success),
        86101 => Some(QrLoginStatus::Waiting),
        86090 => Some(QrLoginStatus::Scanned),
        86038 => Some(QrLoginStatus::Expired),
        _ => None,
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
#[derive(Debug)]
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
                audio: None,
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
                audio: None,
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
        audio: None,
        extra,
    })
}
/// DASH audio selection: gather audio/dolby/flac candidates, pick by
/// bandwidth (closest to 128k for Low, max for higher tiers). Without any
/// DASH audio the legacy `durl[0].url` container (FLV/MP4) wins, reported
/// as Low like upstream.
struct SelectedAudio {
    url: String,
    quality: StreamQuality,
    /// DASH backup CDN URLs beyond the selected primary (`backupUrl`/
    /// `backup_url` arrays minus the URL already serving as the primary).
    /// Empty on the legacy `durl` container, which carries no parsed backup
    /// list.
    backup_urls: Vec<String>,
}
impl SelectedAudio {
    /// Structured backup endpoints (MODEL_ALIGNMENT_DESIGN.md §2.4): each
    /// mirror serves the same bandwidth-selected stream, and the DASH CDN
    /// requires the same Referer/UA header set on every mirror — the same
    /// headers the primary URL carries.
    fn fallback_endpoints(&self) -> Vec<FallbackEndpoint> {
        self.backup_urls
            .iter()
            .map(|url| FallbackEndpoint {
                url: url.clone(),
                headers: vec![
                    ("Referer".into(), BILI_REFERER.into()),
                    ("User-Agent".into(), USER_AGENT.into()),
                ],
                expires_at_unix: None,
                content_length: None,
                quality: Some(self.quality),
            })
            .collect()
    }
}
/// The backup CDN URLs of one DASH candidate item: both spellings join, in
/// `backupUrl` then `backup_url` order, trimmed, deduplicated keeping the
/// first occurrence, with the primary URL (baseUrl or first backup,
/// `stream_url`) dropped so no endpoint duplicates the primary — the same
/// cross-slot rule `StreamInfo::ordered_fallback_urls` applies (§2.4), the
/// producer mirrors it explicitly.
fn backup_urls(item: &Value, primary: &str) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for key in ["backupUrl", "backup_url"] {
        let Some(list) = item[key].as_array() else {
            continue;
        };
        for url in list.iter().filter_map(Value::as_str) {
            let url = url.trim();
            if url.is_empty() || url == primary || urls.iter().any(|existing| existing == url) {
                continue;
            }
            urls.push(url.to_owned());
        }
    }
    urls
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
    };
    if let Some(selected) = selected {
        let bandwidth = selected["bandwidth"].as_u64().unwrap_or_default();
        let url = stream_url(selected)?;
        return Some(SelectedAudio {
            backup_urls: backup_urls(selected, &url),
            url,
            quality: if bandwidth > 160_000 {
                StreamQuality::High
            } else {
                StreamQuality::Low
            },
        });
    }
    let url = json["data"]["durl"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["url"].as_str())
        .filter(|url| !url.is_empty())?
        .to_owned();
    Some(SelectedAudio {
        url,
        quality: StreamQuality::Low,
        backup_urls: Vec::new(),
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
            charts: true,
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
        parse_search_result(&json, q.page.max(1))
    }
}
/// Parse the signed video-search response; `page` is the caller's page
/// (already clamped to >= 1) and drives the has_more computation against
/// the server-reported numResults/pagesize.
fn parse_search_result(json: &Value, page: u32) -> Result<SearchPage> {
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
    let has_more = !items.is_empty() && u64::from(page) * page_size < total;
    Ok(SearchPage {
        items,
        total: Some(total),
        has_more,
    })
}
impl BilibiliProvider {
    async fn resolve_video_reference(&self, input: &str) -> Result<Option<VideoReference>> {
        if let Some(reference) = parse_video_reference(input) {
            return Ok(Some(reference));
        }
        let Some(short_url) = short_video_url(input) else {
            return Ok(None);
        };
        // b23.tv expansion is a redirect-following face whose success is the
        // FINAL url, not the status: today's client sends exactly once and
        // never classifies the HTTP status. `send_once` keeps that shape
        // (one attempt, the response handed back whatever its status) where
        // `send_raw` would collapse a 3xx into a terminal error
        // (TRANSPORT_DESIGN.md §4.1). No session cookies ride along today;
        // only the two request-level headers do.
        let request = TransportRequest::get(&short_url)
            .header("user-agent", USER_AGENT)
            .header("referer", BILI_REFERER);
        let response = self
            .http
            .send_once(&request)
            .await
            .map_err(|e| perr(format!("short link expansion failed: {e}")))?;
        let final_url = response.url.as_deref().unwrap_or_default();
        parse_video_reference(final_url)
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
        // Structured slot backfill (MODEL_ALIGNMENT_DESIGN.md §2.4): the DASH
        // backup CDN mirrors beyond the primary baseUrl ride as endpoints
        // with the same Referer/UA requirement and the same stream tier. No
        // legacy strings are written (one slot per producer, §2.4).
        let fallback_endpoints = selected.fallback_endpoints();
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
            size_is_advisory: false,
            checksum_algorithm: None,
            fallback_endpoints,
        })
    }
}
/// The typed landing for the five unimplemented faces (AUTH_REGISTRY_DESIGN.md
/// §1.3): `UnsupportedOperation` carries the provider id in its typed slot and
/// the site reason in the detail slot, where the legacy `Error::Unsupported`
/// payload could not answer `provider_id()`. Capability flags stay false, so
/// callers that consult them never reach these.
fn unsupported_operation(site: &str) -> Error {
    Error::UnsupportedOperation {
        provider: Some(ID.into()),
        detail: Some(ErrorDetail::new(site)),
    }
}
#[async_trait]
impl LyricsProvider for BilibiliProvider {
    async fn lyrics(&self, _track: &Track) -> Result<Option<LyricDocument>> {
        // Bilibili exposes no lyric API; the capability stays false.
        Err(unsupported_operation("no lyric API exists on Bilibili"))
    }
}
/// SESSDATA is the login credential; other cookies are secondary.
#[async_trait]
impl AuthenticationProvider for BilibiliProvider {
    /// Reports through the derived [`AuthState`] (§2.3 fold), replacing the
    /// hand-classified copy of the same predicates. Behavior is unchanged —
    /// LoggedOut without a SESSDATA-carrying session, LoggedIn with identity
    /// from the session otherwise — except that the Expired branch is now
    /// reachable, since this slice added the `login_expired` write point.
    /// The Authenticated arm keeps the independent identity pipe (user_id /
    /// display_name straight from the session), the kugou §9.2 shape.
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
            // The generate API returns only the QR-content URL (the host
            // renders it); no image bytes are served, so image_png stays
            // None per the actual return (M3 QR provider slice,
            // MODEL_ALIGNMENT_DESIGN.md §6).
            image_png: None,
            kind: QrLoginKind::Standard,
        })
    }
    async fn poll_qr_login(&self, key: &str) -> Result<QrLoginPoll> {
        let key = key.trim();
        if key.is_empty() {
            return Err(Error::InvalidInput("QR login key is empty".into()));
        }
        let url = format!("{QR_POLL_URL}?qrcode_key={}", urlencoding::encode(key));
        // QR poll sends exactly once and never classifies the HTTP status
        // today; the login cookies arrive in the Set-Cookie headers of THIS
        // response, so the response must come back whatever its status.
        // `send_once` keeps that shape (TRANSPORT_DESIGN.md §4.1). No session
        // cookies ride along; only the two request-level headers do.
        let request = TransportRequest::get(&url)
            .header("user-agent", USER_AGENT)
            .header("referer", BILI_REFERER);
        let response = self
            .http
            .send_once(&request)
            .await
            .map_err(|e| perr(format!("QR poll request failed: {e}")))?;
        // Success cookies arrive in the Set-Cookie headers of this response.
        // `all("set-cookie")` already drops wire values that were not valid
        // UTF-8, matching the previous `HeaderValue::to_str().ok()` skip
        // (TRANSPORT_DESIGN.md §5.5 row 9).
        let set_cookies = response.all("set-cookie");
        let json: Value = serde_json::from_slice(&response.body)
            .map_err(|e| perr(format!("QR poll parse failed: {e}")))?;
        // The outer code is HTTP-level; data.code carries the scan state.
        if json["code"].as_i64() != Some(0) {
            return Err(perr(api_error(&json, "QR poll failed")));
        }
        let data = &json["data"];
        let data_code = data["code"].as_i64().unwrap_or(-1);
        let status = match qr_status_from_code(data_code) {
            Some(status) => status,
            None => return Err(perr(format!("QR poll failed (code={data_code})"))),
        };
        let mut user_id = None;
        let mut display_name = None;
        if status == QrLoginStatus::Success {
            self.capture_login_session(&set_cookies, data)?;
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
    /// `set_cookies` comes from `TransportResponse::all("set-cookie")`,
    /// which preserves duplicates and already skips values that were not
    /// valid UTF-8 on the wire (the previous `to_str().ok()` drop,
    /// TRANSPORT_DESIGN.md §5.5 row 9).
    fn capture_login_session(&self, set_cookies: &[&str], data: &Value) -> Result<()> {
        let mut session = self.sessions.get(ID).unwrap_or_default();
        for value in set_cookies {
            if let Some(pair) = value.split(';').next()
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
        // §2.2 Expired→Authenticated edge: a fresh scan is a successful
        // re-login, so a stale `login_expired` marker left behind by
        // `mark_session_expired` must not survive it (the same wiring the
        // kugou slice landed, §9.2).
        session.tokens.remove("login_expired");
        self.sessions.set(ID, session)?;
        Ok(())
    }
}
#[async_trait]
impl CollectionProvider for BilibiliProvider {
    async fn playlist(&self, _id: &str) -> Result<Playlist> {
        Err(unsupported_operation(
            "single playlist fetch is not implemented",
        ))
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
                    if let Some(track) = parse_fav_resource(item)
                        && seen.insert(track.id.id.clone())
                    {
                        all.push(track);
                    }
                }
            }
            let has_more = json["data"]["has_more"].as_bool().unwrap_or(false);
            if !has_more || page_empty {
                break;
            }
            // The listing is truncated at the page cap; report the limit
            // instead of silently returning a partial favorites list.
            if current_page >= MAX_PAGES {
                return Err(perr(format!(
                    "favorites exceed {} items; loading stopped",
                    PAGE_SIZE * MAX_PAGES
                )));
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
        Err(unsupported_operation(
            "playlist categories are not implemented",
        ))
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
            .filter_map(parse_fav_folder)
            .collect())
    }
    async fn search_playlists(&self, _keyword: &str, _page: u32) -> Result<Vec<Playlist>> {
        Err(unsupported_operation("playlist search is not implemented"))
    }
    async fn user_playlists(&self, _page: u32, _page_size: u32) -> Result<Vec<Playlist>> {
        // Bilibili favorites are the user playlists; handled via playlists().
        Err(unsupported_operation(
            "user playlists are the favorites; use playlists()",
        ))
    }
}
// ── Charts (four hardcoded boards: three unsigned endpoints + ranking/v2) ──
#[async_trait]
impl ChartProvider for BilibiliProvider {
    /// Board catalogue is hardcoded upstream (leaderboard.rs:17-40): four
    /// id/name pairs, no network call, no descriptions.
    async fn charts(&self) -> Result<Vec<Chart>> {
        Ok(chart_boards())
    }
    /// One pinned page of CHART_PAGE_SIZE rows: the trait has no page
    /// parameters and docs/MIGRATION.md defers chart pagination. The pin
    /// takes the popular board's server maximum (upstream ps clamps to 50)
    /// and lets the recommend board clamp to its native page size of 20, so
    /// every board answers with a single request, like upstream at page 1.
    async fn chart_tracks(&self, id: &str) -> Result<Vec<ChartEntry>> {
        let board = id.trim();
        if board.is_empty() {
            return Err(Error::InvalidInput("chart id is empty".into()));
        }
        match board {
            "popular" => self.fetch_popular_chart().await,
            "ranking" => self.fetch_ranking_chart().await,
            "music-hot" => self.fetch_music_hot_chart().await,
            "recommend" => self.fetch_recommend_chart().await,
            // Upstream errors on unknown ids (leaderboard.rs:53).
            _ => Err(Error::InvalidInput(format!(
                "unknown bilibili chart id: {board}"
            ))),
        }
    }
}
impl BilibiliProvider {
    /// popular board (leaderboard.rs:136-188): unsigned, server-side
    /// pagination with ps/pn.
    async fn fetch_popular_chart(&self) -> Result<Vec<ChartEntry>> {
        let json = self
            .get_json(
                POPULAR_ENDPOINT,
                &[("ps", CHART_PAGE_SIZE.to_string()), ("pn", "1".into())],
                "popular chart",
            )
            .await?;
        parse_popular_chart(&json)
    }
    /// ranking board (leaderboard.rs:191-218): the only WBI-signed chart
    /// call; the whole data.list comes back at once and is sliced to the
    /// pinned page client-side (upstream page_slice at offset 0).
    async fn fetch_ranking_chart(&self) -> Result<Vec<ChartEntry>> {
        let json = self
            .signed_get(
                RANKING_V2_ENDPOINT,
                &ranking_chart_params(),
                "ranking chart",
            )
            .await?;
        parse_ranking_chart(&json)
    }
    /// music-hot board (leaderboard.rs:57-81): unsigned, plat=2.
    async fn fetch_music_hot_chart(&self) -> Result<Vec<ChartEntry>> {
        let json = self
            .get_json(
                MUSIC_HOT_ENDPOINT,
                &[("plat", "2".into())],
                "music hot chart",
            )
            .await?;
        parse_music_hot_chart(&json)
    }
    /// recommend board (leaderboard.rs:83-134): unsigned; request_cnt clamps
    /// to the endpoint's native page size of 20 and from_region=1003 carries
    /// over as an opaque constant. A gated, shape-broken or empty reply
    /// silently falls back to the popular board like upstream
    /// (leaderboard.rs:106-123) — a product decision kept deliberately.
    async fn fetch_recommend_chart(&self) -> Result<Vec<ChartEntry>> {
        let request_limit = CHART_PAGE_SIZE.clamp(1, RECOMMEND_PAGE_SIZE);
        let json = self
            .get_json(
                REGION_RECOMMEND_ENDPOINT,
                &[
                    ("display_id", "1".into()),
                    ("request_cnt", request_limit.to_string()),
                    ("from_region", "1003".into()),
                ],
                "recommend chart",
            )
            .await?;
        match parse_recommend_chart(&json) {
            Some(rows) => Ok(rows),
            None => self.fetch_popular_chart().await,
        }
    }
}
fn normalize_url(value: Option<&str>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty()).map(|value| {
        value
            .strip_prefix("//")
            .map_or_else(|| value.to_owned(), |url| format!("https://{url}"))
    })
}
/// One entry of the created-favorites folder list (login-gated endpoint).
fn parse_fav_folder(folder: &Value) -> Option<Playlist> {
    let id = folder["id"].as_u64()?.to_string();
    let title = folder["title"].as_str()?.to_owned();
    Some(Playlist {
        id: TrackId::new(ID, id),
        title,
        description: None,
        artwork_url: normalize_url(folder["cover"].as_str()),
        track_count: folder["media_count"].as_u64().map(|n| n as u32),
        tracks: vec![],
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
    })
}
/// One entry of a favorite folder's media list. `cid` may arrive as a JSON
/// string or a number on this endpoint; both forms are accepted.
fn parse_fav_resource(item: &Value) -> Option<Track> {
    let bvid = item["bvid"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let mut extra = BTreeMap::new();
    extra.insert("bvid".into(), bvid.to_owned());
    let cid = bili_value_string(&item["cid"]);
    if !cid.is_empty() {
        extra.insert("cid".into(), cid);
    }
    Some(Track {
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
        audio: None,
        extra,
    })
}
// ── Chart parsing (voicefox bili/leaderboard.rs ports) ─────────────────────
/// The hardcoded board catalogue (upstream get_boards, leaderboard.rs:17-40).
fn chart_boards() -> Vec<Chart> {
    [
        ("popular", "全网热门"),
        ("ranking", "全站排行榜"),
        ("music-hot", "音乐热歌榜"),
        ("recommend", "热门推荐"),
    ]
    .into_iter()
    .map(|(id, title)| Chart {
        id: TrackId::new(ID, id),
        title: title.to_owned(),
        description: None,
        updated_description: None,
    })
    .collect()
}
/// The ranking board's signed parameter set: rid=0/type=all
/// (leaderboard.rs:196-202).
fn ranking_chart_params() -> Vec<(&'static str, String)> {
    vec![("rid", "0".into()), ("type", "all".into())]
}
/// Chart-row covers (upstream non_empty_url, leaderboard.rs:320-328):
/// whitespace is dropped first, then normalize_cover prefixes
/// protocol-relative URLs with https:.
fn chart_cover(value: Option<&str>) -> Option<String> {
    normalize_cover(value.map(str::trim))
}
/// Shared Track construction for chart rows; the three board parsers differ
/// only in field names (upstream parse_*_song all fill SongInfo the same
/// way). The board row id is the bvid, like search; upstream carries no
/// rank, so position stays implicit list order.
fn chart_track(
    bvid: &str,
    title: String,
    artist: String,
    album: Option<String>,
    cover: Option<String>,
    duration: Option<Duration>,
    cid: Option<u64>,
) -> Track {
    let mut extra = BTreeMap::new();
    extra.insert("bvid".into(), bvid.to_owned());
    if let Some(cid) = cid {
        extra.insert("cid".into(), cid.to_string());
    }
    Track {
        id: TrackId::new(ID, bvid),
        title,
        artists: vec![artist],
        album,
        album_id: Some(bvid.to_owned()),
        duration,
        artwork_url: cover,
        isrc: None,
        available_qualities: vec![],
        audio: None,
        extra,
    }
}
/// popular/ranking rows (upstream parse_popular_song, leaderboard.rs:220-248):
/// bvid required, author = owner.name else flat author else "哔哩哔哩用户",
/// album = title, cover = pic else cover, duration accepts "h:mm:ss"/
/// "mm:ss"/"ss" strings and numeric seconds.
fn chart_popular_track(value: &Value) -> Option<Track> {
    let bvid = value["bvid"].as_str()?;
    if bvid.is_empty() {
        return None;
    }
    let title = value["title"].as_str().unwrap_or_default().to_owned();
    let author = value["owner"]["name"]
        .as_str()
        .or_else(|| value["author"].as_str())
        .unwrap_or("哔哩哔哩用户");
    let duration = value["duration"]
        .as_str()
        .map(parse_bili_duration)
        .or_else(|| value["duration"].as_u64().map(Duration::from_secs))
        .filter(|duration| duration.as_secs() > 0);
    Some(chart_track(
        bvid,
        title.clone(),
        author.to_owned(),
        Some(title),
        chart_cover(value["pic"].as_str().or_else(|| value["cover"].as_str())),
        duration,
        value["cid"].as_u64(),
    ))
}
/// music-hot rows (upstream parse_music_song, leaderboard.rs:250-275): no
/// owner object — author and album come straight off the row and may stay
/// empty, the cover field is `cover`, and duration is numeric seconds only
/// (a string duration is ignored, unlike the popular board).
fn chart_music_track(value: &Value) -> Option<Track> {
    let bvid = value["bvid"].as_str()?;
    if bvid.is_empty() {
        return None;
    }
    Some(chart_track(
        bvid,
        value["music_title"].as_str().unwrap_or_default().to_owned(),
        value["author"].as_str().unwrap_or_default().to_owned(),
        Some(value["album"].as_str().unwrap_or_default().to_owned()),
        chart_cover(value["cover"].as_str()),
        value["duration"]
            .as_u64()
            .map(Duration::from_secs)
            .filter(|duration| duration.as_secs() > 0),
        value["cid"].as_u64(),
    ))
}
/// recommend rows (upstream parse_recommend_song, leaderboard.rs:277-303):
/// author = owner.name else author.name else "哔哩哔哩用户", album = title,
/// cover = pic else cover, duration numeric seconds only.
fn chart_recommend_track(value: &Value) -> Option<Track> {
    let bvid = value["bvid"].as_str()?;
    if bvid.is_empty() {
        return None;
    }
    let title = value["title"].as_str().unwrap_or_default().to_owned();
    let author = value["owner"]["name"]
        .as_str()
        .or_else(|| value["author"]["name"].as_str())
        .unwrap_or("哔哩哔哩用户");
    Some(chart_track(
        bvid,
        title.clone(),
        author.to_owned(),
        Some(title),
        chart_cover(value["pic"].as_str().or_else(|| value["cover"].as_str())),
        value["duration"]
            .as_u64()
            .map(Duration::from_secs)
            .filter(|duration| duration.as_secs() > 0),
        value["cid"].as_u64(),
    ))
}
/// ChartEntry.rank is derived from list order: upstream carries no rank and
/// position is the page_slice offset+index (leaderboard.rs:305-318); the
/// pinned page starts at offset 0, so ranks stay contiguous 1..N.
fn rank_chart_rows(rows: impl Iterator<Item = Track>) -> Vec<ChartEntry> {
    rows.enumerate()
        .map(|(index, track)| ChartEntry {
            rank: index as u32 + 1,
            track,
        })
        .collect()
}
/// Top-level code gate shared by the board replies (leaderboard.rs:66-73 and
/// friends): code must be 0 or the server rejection — with risk-control
/// codes rewritten to the explicit message — becomes an Api error.
fn chart_code_gate(json: &Value, fallback: &str) -> Result<()> {
    if json["code"].as_i64().unwrap_or(-1) != 0 {
        return Err(perr(api_error(json, fallback)));
    }
    Ok(())
}
/// popular board rows (upstream get_popular, leaderboard.rs:161-188): rows
/// without a bvid are dropped, the page is capped at CHART_PAGE_SIZE rows
/// and ranked in list order. data.no_more and the total heuristic have no
/// slot in the musid-dli chart surface and are dropped with it.
fn parse_popular_chart(json: &Value) -> Result<Vec<ChartEntry>> {
    chart_code_gate(json, "popular chart failed")?;
    let list = json["data"]["list"]
        .as_array()
        .ok_or_else(|| perr("popular chart data is empty"))?;
    Ok(rank_chart_rows(
        list.iter()
            .filter_map(chart_popular_track)
            .take(CHART_PAGE_SIZE as usize),
    ))
}
/// ranking board rows (upstream get_ranking + page_slice, leaderboard.rs:
/// 211-217): the whole data.list is fetched at once and sliced to the
/// pinned page client-side.
fn parse_ranking_chart(json: &Value) -> Result<Vec<ChartEntry>> {
    chart_code_gate(json, "ranking chart failed")?;
    let list = json["data"]["list"]
        .as_array()
        .ok_or_else(|| perr("ranking chart data is empty"))?;
    Ok(rank_chart_rows(
        list.iter()
            .filter_map(chart_popular_track)
            .take(CHART_PAGE_SIZE as usize),
    ))
}
/// music-hot board rows (upstream get_music_hot, leaderboard.rs:74-81).
fn parse_music_hot_chart(json: &Value) -> Result<Vec<ChartEntry>> {
    chart_code_gate(json, "music hot chart failed")?;
    let list = json["data"]["list"]
        .as_array()
        .ok_or_else(|| perr("music hot chart data is empty"))?;
    Ok(rank_chart_rows(
        list.iter()
            .filter_map(chart_music_track)
            .take(CHART_PAGE_SIZE as usize),
    ))
}
/// recommend board rows (upstream get_recommend, leaderboard.rs:104-123):
/// returns None exactly when upstream would silently fall back to the
/// popular board — a non-zero code, missing data.archives, or zero parsable
/// rows — and the fetch side performs that fallback.
fn parse_recommend_chart(json: &Value) -> Option<Vec<ChartEntry>> {
    if json["code"].as_i64().unwrap_or(-1) != 0 {
        return None;
    }
    let archives = json["data"]["archives"].as_array()?;
    let rows = rank_chart_rows(
        archives
            .iter()
            .filter_map(chart_recommend_track)
            .take(RECOMMEND_PAGE_SIZE as usize),
    );
    (!rows.is_empty()).then_some(rows)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Loads a reconstructed real-response fixture from tests/fixtures/bilibili
    /// (see the README there for the derivation of these shapes).
    macro_rules! fixture {
        ($name:expr) => {
            serde_json::from_str::<Value>(include_str!(concat!(
                "../tests/fixtures/bilibili/",
                $name
            )))
            .expect("bilibili fixture must be valid JSON")
        };
    }
    /// The audit golden vectors (V2-V4) use the upstream test key material
    /// with a frozen `wts`; production injects the wall-clock `wts` instead.
    fn golden_wbi_keys() -> (String, String) {
        ("a".repeat(32), "b".repeat(32))
    }
    fn signed_params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

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
        assert!(descriptor.capabilities.charts);
        // No lyric API exists on Bilibili.
        assert!(!descriptor.capabilities.lyrics);
        assert!(!descriptor.capabilities.albums);
        assert!(registry.stream_provider(ID).is_some());
        assert!(registry.qr_authentication_provider(ID).is_some());
        assert!(registry.chart_provider(ID).is_some());
    }

    #[test]
    fn wbi_table_is_a_complete_permutation() {
        // Sanity companion to the golden digests below: no duplicated or
        // missing index in the 64-entry scramble table.
        let mut sorted = WBI_MIXIN_KEY_TABLE;
        sorted.sort_unstable();
        assert_eq!(sorted.to_vec(), (0..64).collect::<Vec<usize>>());
    }
    #[test]
    fn wbi_signing_matches_golden_vectors() {
        let keys = golden_wbi_keys();
        // Golden vector A (V2): keyword search, md5sum-verified during the
        // audit and re-verified in this session.
        let mut params = signed_params(&[("keyword", "晴天"), ("page", "1")]);
        params.insert("wts".into(), "1700000000".into());
        assert_eq!(
            encode_wbi_query(&params, &keys),
            "keyword=%E6%99%B4%E5%A4%A9&page=1&wts=1700000000&w_rid=6ced9cbc63d8f95c0a0d7348a302198a"
        );
        // Golden vector B (V3): `!'()*` are stripped from values before
        // encoding and digesting.
        let mut params = signed_params(&[("page", "2"), ("weird", "va!l(u)e")]);
        params.insert("wts".into(), "1700000000".into());
        assert_eq!(
            encode_wbi_query(&params, &keys),
            "page=2&weird=value&wts=1700000000&w_rid=df00e3f5b2c28ad415a440625f123711"
        );
        // Golden vector C (V4): the exact signed-search parameter set built
        // by search() (search_type=video / keyword / page / page_size /
        // tids=3 music zone). This pins the query construction only; the
        // server itself risk-controls anonymous keyword search.
        let mut params = signed_params(&[
            ("keyword", "周杰伦"),
            ("page", "1"),
            ("page_size", "20"),
            ("tids", "3"),
        ]);
        params.insert("wts".into(), "1700000000".into());
        assert_eq!(
            encode_wbi_query(&params, &keys),
            "keyword=%E5%91%A8%E6%9D%B0%E4%BC%A6&page=1&page_size=20&tids=3&wts=1700000000&w_rid=7c0b45a87fcd6ae3cc153497c9295fbc"
        );
    }
    #[test]
    fn wbi_keys_come_from_the_nav_url_file_name() {
        // The nav fixture pins the anonymous shape: code -101 (not logged
        // in) but data.wbi_img is present, which is all wbi_keys() needs.
        let json = fixture!("nav_wbi_keys.json");
        assert_eq!(json["code"].as_i64(), Some(-101));
        assert_eq!(
            key_from_url(json["data"]["wbi_img"]["img_url"].as_str()),
            "0a1b2c3d4e5f60718293a4b5c6d7e8f1"
        );
        assert_eq!(
            key_from_url(json["data"]["wbi_img"]["sub_url"].as_str()),
            "99887766554433221100ffeeddccbbaa"
        );
        assert!(key_from_url(None).is_empty());
    }
    #[test]
    fn search_fixture_pins_pagination_and_has_more() {
        let json = fixture!("search_music_zone.json");
        let page = parse_search_result(&json, 2).unwrap();
        assert_eq!(page.items.len(), 20);
        assert_eq!(page.total, Some(45));
        assert!(page.has_more);
        assert!(!parse_search_result(&json, 3).unwrap().has_more);
        let first = &page.items[0];
        assert_eq!(first.id.id, "BV0000000000");
        assert_eq!(first.artists, vec!["artist"]);
        // Search duration is a "mm:ss" string.
        assert_eq!(first.duration.map(|d| d.as_secs()), Some(180));
        assert_eq!(
            first.artwork_url.as_deref(),
            Some("https://example.com/cover.jpg")
        );
    }
    #[test]
    fn search_fixture_strips_em_and_uses_video_as_album() {
        let json = fixture!("search_title_em.json");
        let track = &parse_search_result(&json, 1).unwrap().items[0];
        assert_eq!(track.title, "测试视频");
        assert_eq!(track.album.as_deref(), Some("测试视频"));
        assert_eq!(track.album_id.as_deref(), Some("BV1xx411c7mD"));
        assert_eq!(track.artists, vec!["UP主"]);
        // Search cid/aid are always numeric JSON here.
        assert_eq!(track.extra["cid"], "1001");
        assert_eq!(track.extra["aid"], "170001");
    }
    #[test]
    fn view_fixture_expands_each_part() {
        let json = fixture!("view_multipage.json");
        let tracks = parse_video_result(&json, None).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "第一段");
        // Every part of a multi-page video carries the -p{n} suffix.
        assert_eq!(tracks[0].id.id, "BV1xx411c7mD-p1");
        assert_eq!(tracks[1].id.id, "BV1xx411c7mD-p2");
        assert_eq!(tracks[1].extra["cid"], "1002");
        assert_eq!(tracks[1].extra["bili_part_title"], "第二段");
        assert_eq!(tracks[0].extra["aid"], "170001");
        assert_eq!(tracks[0].duration.map(|d| d.as_secs()), Some(60));
        assert_eq!(tracks[0].album.as_deref(), Some("测试视频"));
        // ?p=2 keeps only that part.
        let tracks = parse_video_result(&json, Some(2)).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id.id, "BV1xx411c7mD-p2");
        assert_eq!(tracks[0].duration.map(|d| d.as_secs()), Some(90));
    }
    #[test]
    fn view_fixture_single_page_uses_video_title() {
        let json = fixture!("view_singlepage.json");
        let tracks = parse_video_result(&json, None).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "测试视频");
        assert_eq!(tracks[0].id.id, "BV1xx411c7mD");
        assert_eq!(tracks[0].extra["bili_part_title"], "第一段");
        // No quality is advertised until a stream is chosen.
        assert!(tracks[0].available_qualities.is_empty());
    }
    #[test]
    fn view_fixture_falls_back_to_top_level_cid() {
        let json = fixture!("view_nopages_fallback.json");
        let tracks = parse_video_result(&json, None).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "单P视频");
        assert_eq!(tracks[0].extra["cid"], "777");
        assert_eq!(tracks[0].extra["page"], "1");
        assert_eq!(tracks[0].duration.map(|d| d.as_secs()), Some(120));
    }
    #[test]
    fn dash_fixture_selects_by_requested_quality() {
        // Upstream inline vector verbatim (url.rs tests): 64k/128k/192k.
        // The fixture carries the real-response backupUrl arrays per item,
        // so the selection also pins the structured backup endpoints
        // (MODEL_ALIGNMENT_DESIGN.md §2.4): the mirrors beyond the primary
        // baseUrl ride with the DASH Referer/UA set and the selected tier.
        let json = fixture!("playurl_dash.json");
        let low = choose_audio(&json, StreamQuality::Low).unwrap();
        assert_eq!(low.url, "https://example.com/128");
        assert_eq!(low.quality, StreamQuality::Low);
        assert_eq!(
            low.backup_urls,
            vec!["https://example.com/128-b1".to_owned()]
        );
        let endpoints = low.fallback_endpoints();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].url, "https://example.com/128-b1");
        assert_eq!(endpoints[0].quality, Some(StreamQuality::Low));
        assert_eq!(
            endpoints[0].headers,
            vec![
                ("Referer".to_owned(), BILI_REFERER.to_owned()),
                ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ]
        );
        let high = choose_audio(&json, StreamQuality::High).unwrap();
        assert_eq!(high.url, "https://example.com/192");
        assert_eq!(high.quality, StreamQuality::High);
        assert_eq!(
            high.backup_urls,
            vec!["https://example.com/192-b1".to_owned()]
        );
    }
    #[test]
    fn dash_fixture_pins_dolby_flac_and_snake_case_urls() {
        // Reconstruction vector: dolby.audio as a single object and
        // flac.audio as an array both join the candidate pool; the
        // snake_case base_url form is read as well.
        let json = fixture!("playurl_dash_dolby_flac.json");
        let high = choose_audio(&json, StreamQuality::High).unwrap();
        assert_eq!(high.url, "https://cdn/flac");
        assert_eq!(high.quality, StreamQuality::High);
        let low = choose_audio(&json, StreamQuality::Low).unwrap();
        assert_eq!(low.url, "https://cdn/64");
        assert_eq!(low.quality, StreamQuality::Low);
    }
    #[test]
    fn dash_fixture_backup_url_prefers_camel_case() {
        // Reconstruction vector: without baseUrl the first backupUrl wins
        // over the snake_case backup_url. The winner becomes the primary, so
        // the structured backup list keeps only the snake_case mirror (the
        // primary URL itself is never duplicated into the endpoint slot).
        let json = fixture!("playurl_dash_backup.json");
        let audio = choose_audio(&json, StreamQuality::Low).unwrap();
        assert_eq!(audio.url, "https://cdn/backup-camel");
        assert_eq!(
            audio.backup_urls,
            vec!["https://cdn/backup-snake".to_owned()]
        );
    }
    #[test]
    fn dash_fixture_falls_back_to_legacy_durl() {
        // Reconstruction vector: no dash node at all, the legacy durl[0]
        // container (FLV/MP4) is the stream; reported as Low.
        let json = fixture!("playurl_durl_fallback.json");
        let audio = choose_audio(&json, StreamQuality::High).unwrap();
        assert_eq!(audio.url, "https://cdn/legacy-flv");
        assert_eq!(audio.quality, StreamQuality::Low);
        assert!(
            audio.backup_urls.is_empty(),
            "the legacy durl container has no parsed backup list"
        );
        assert!(choose_audio(&json!({"code": 0, "data": {}}), StreamQuality::Low).is_none());
    }
    #[test]
    fn fav_folder_fixture_parses_created_folders() {
        let json = fixture!("fav_folders.json");
        let folders = json["data"]["list"].as_array().unwrap();
        let folder = parse_fav_folder(&folders[0]).unwrap();
        assert_eq!(folder.id.id, "123");
        assert_eq!(folder.title, "我的收藏");
        assert_eq!(
            folder.artwork_url.as_deref(),
            Some("https://i0.hdslb.com/bfs/fav/xx.jpg")
        );
        assert_eq!(folder.track_count, Some(42));
    }
    #[test]
    fn fav_resource_fixture_pins_dual_cid_forms() {
        let json = fixture!("fav_resources.json");
        let medias = json["data"]["medias"].as_array().unwrap();
        // cid arrives as a JSON string on this endpoint...
        let first = parse_fav_resource(&medias[0]).unwrap();
        assert_eq!(first.id.id, "BV1xx411c7mD");
        assert_eq!(first.title, "歌名");
        assert_eq!(first.extra["cid"], "1001");
        // ...and as a number on the same endpoint.
        let second = parse_fav_resource(&medias[1]).unwrap();
        assert_eq!(second.extra["cid"], "1002");
        // duration is integer seconds; zero stays None.
        assert_eq!(first.duration.map(|d| d.as_secs()), Some(245));
        assert_eq!(second.duration, None);
        assert_eq!(first.artists, vec!["UP主"]);
        assert_eq!(
            first.artwork_url.as_deref(),
            Some("https://i0.hdslb.com/bfs/archive/yy.jpg")
        );
        // has_more:false is the pagination terminator.
        assert_eq!(json["data"]["has_more"].as_bool(), Some(false));
    }
    #[test]
    fn risk_control_code_is_reported_honestly() {
        // -412 (and -352) rewrite to an explicit risk-control error instead
        // of surfacing the server message; the rejection is never disguised.
        let json = fixture!("search_risk_control.json");
        assert_eq!(
            api_error(&json, "search failed"),
            "Bilibili risk control blocked the request (code=-412)"
        );
    }
    #[test]
    fn parse_bili_duration_accepts_all_segment_forms() {
        // Search durations arrive as "mm:ss" / "hh:mm:ss" / bare seconds.
        assert_eq!(parse_bili_duration("03:00").as_secs(), 180);
        assert_eq!(parse_bili_duration("1:02:03").as_secs(), 3723);
        assert_eq!(parse_bili_duration("45").as_secs(), 45);
        assert_eq!(parse_bili_duration("").as_secs(), 0);
    }
    #[test]
    fn parses_upstream_reference_vectors() {
        // Audit V11, upstream inline vectors verbatim.
        let reference = parse_video_reference("BV1xx411c7mD").unwrap();
        assert!(
            matches!(reference, VideoReference::Bvid { ref bvid, page: None } if bvid == "BV1xx411c7mD")
        );
        let reference = parse_video_reference(
            "https://www.bilibili.com/video/BV1xx411c7mD/?share_source=copy_web&p=2",
        )
        .unwrap();
        assert!(matches!(
            reference,
            VideoReference::Bvid { page: Some(2), .. }
        ));
        let reference =
            parse_video_reference("https://www.bilibili.com/video/av170001?p=3").unwrap();
        assert!(
            matches!(reference, VideoReference::Aid { ref aid, page: Some(3) } if aid == "170001")
        );
        // Shared text is not a video reference.
        assert!(parse_video_reference("周杰伦 晴天").is_none());
    }

    // --- chart fixtures: the four-board catalogue and per-board row shapes
    // Vectors come from the 2026-11-18 VoiceFox leaderboard.rs audit; the
    // fixtures are reconstructed (no raw capture exists anywhere), see
    // tests/fixtures/bilibili/README.md. ---

    #[test]
    fn chart_catalogue_is_the_four_hardcoded_boards() {
        // Audit vector: get_boards() yields exactly these four id/name pairs
        // in order, without a network call (leaderboard.rs:17-40).
        let boards = chart_boards();
        let pairs: Vec<(&str, &str)> = boards
            .iter()
            .map(|board| (board.id.id.as_str(), board.title.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("popular", "全网热门"),
                ("ranking", "全站排行榜"),
                ("music-hot", "音乐热歌榜"),
                ("recommend", "热门推荐"),
            ]
        );
        // Upstream LeaderboardInfo carries no description for these boards.
        assert!(boards.iter().all(|board| board.description.is_none()));
    }

    #[test]
    fn chart_popular_fixture_parses_rows_and_drops_bvid_less_ones() {
        let entries = parse_popular_chart(&fixture!("chart_popular.json")).unwrap();
        assert_eq!(entries.len(), 2);
        let first = &entries[0];
        assert_eq!(first.rank, 1);
        assert_eq!(first.track.id.provider, ID);
        assert_eq!(first.track.id.id, "BV1xx411c7mD");
        assert_eq!(first.track.title, "全网热门甲");
        assert_eq!(first.track.artists, vec!["UP主甲"]);
        assert_eq!(first.track.album.as_deref(), Some("全网热门甲"));
        assert_eq!(first.track.album_id.as_deref(), Some("BV1xx411c7mD"));
        // "3:45" parses to 225 seconds; protocol-relative covers get https:.
        assert_eq!(first.track.duration.map(|d| d.as_secs()), Some(225));
        assert_eq!(
            first.track.artwork_url.as_deref(),
            Some("https://i0.hdslb.com/x.jpg")
        );
        assert_eq!(first.track.extra["bvid"], "BV1xx411c7mD");
        assert_eq!(first.track.extra["cid"], "111");
        // Numeric duration seconds and the pic -> cover fallback.
        let second = &entries[1];
        assert_eq!(second.rank, 2);
        assert_eq!(second.track.duration.map(|d| d.as_secs()), Some(223));
        assert_eq!(
            second.track.artwork_url.as_deref(),
            Some("https://i0.hdslb.com/y.jpg")
        );
    }

    #[test]
    fn chart_ranking_fixture_parses_with_author_fallbacks() {
        // Ranking rows share the popular row shape (leaderboard.rs:211-217).
        let entries = parse_ranking_chart(&fixture!("chart_ranking.json")).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].track.artists, vec!["UP主甲"]);
        assert_eq!(entries[0].track.duration.map(|d| d.as_secs()), Some(200));
        // owner.name missing: the flat author string wins.
        assert_eq!(entries[1].track.artists, vec!["UP主乙"]);
        assert_eq!(entries[1].track.duration.map(|d| d.as_secs()), Some(245));
        assert_eq!(entries[1].rank, 2);
        // Neither owner nor author: the upstream "哔哩哔哩用户" placeholder;
        // a whitespace-only pic parses to no artwork (cover vector).
        assert_eq!(entries[2].track.artists, vec!["哔哩哔哩用户"]);
        assert_eq!(entries[2].track.artwork_url, None);
        // "1:02:03" parses to 3723 seconds.
        assert_eq!(entries[2].track.duration.map(|d| d.as_secs()), Some(3723));
        assert_eq!(entries[2].rank, 3);
    }

    #[test]
    fn chart_music_hot_fixture_ignores_string_durations() {
        let entries = parse_music_hot_chart(&fixture!("chart_music_hot.json")).unwrap();
        assert_eq!(entries.len(), 2);
        let first = &entries[0];
        assert_eq!(first.track.title, "热歌甲");
        assert_eq!(first.track.artists, vec!["歌手甲"]);
        assert_eq!(first.track.album.as_deref(), Some("专辑甲"));
        assert_eq!(
            first.track.artwork_url.as_deref(),
            Some("https://i0.hdslb.com/m1.jpg")
        );
        assert_eq!(first.track.duration.map(|d| d.as_secs()), Some(245));
        assert_eq!(first.track.extra["cid"], "301");
        // No owner object on this board: an empty author/album stays empty
        // (upstream unwrap_or_default, no placeholder), an empty cover drops,
        // and a "mm:ss" string duration is ignored — music-hot rows are
        // numeric seconds only (upstream as_u64).
        let second = &entries[1];
        assert_eq!(second.track.artists, vec![""]);
        assert_eq!(second.track.album.as_deref(), Some(""));
        assert_eq!(second.track.artwork_url, None);
        assert_eq!(second.track.duration, None);
    }

    #[test]
    fn chart_recommend_fixture_parses_both_author_shapes() {
        let entries = parse_recommend_chart(&fixture!("chart_recommend.json")).unwrap();
        assert_eq!(entries.len(), 3);
        // owner.name author shape.
        assert_eq!(entries[0].track.artists, vec!["UP主丙"]);
        assert_eq!(
            entries[0].track.artwork_url.as_deref(),
            Some("https://i0.hdslb.com/rc1.jpg")
        );
        assert_eq!(entries[0].track.duration.map(|d| d.as_secs()), Some(188));
        // author:{name} nested-object shape with the cover fallback field.
        assert_eq!(entries[1].track.artists, vec!["UP主丁"]);
        assert_eq!(
            entries[1].track.artwork_url.as_deref(),
            Some("https://i0.hdslb.com/rc2.jpg")
        );
        assert_eq!(entries[1].rank, 2);
        // No owner and no author object: the upstream placeholder artist.
        assert_eq!(entries[2].track.artists, vec!["哔哩哔哩用户"]);
        assert_eq!(entries[2].rank, 3);
    }

    #[test]
    fn chart_recommend_parse_falls_back_on_gate_or_shape_failures() {
        // Upstream silently falls back to the popular board when the reply is
        // gated, carries no archives, or parses to zero rows
        // (leaderboard.rs:106-123); the parser signals that with None and the
        // fetch side performs the fallback.
        assert!(parse_recommend_chart(&json!({"code": -412, "message": "blocked"})).is_none());
        assert!(parse_recommend_chart(&json!({"code": 0, "data": {}})).is_none());
        assert!(parse_recommend_chart(&json!({"code": 0, "data": {"archives": []}})).is_none());
        assert!(
            parse_recommend_chart(&json!({"code": 0, "data": {"archives": [{"bvid": ""}]}}))
                .is_none()
        );
    }

    #[test]
    fn chart_list_parsers_error_without_list_or_on_gate() {
        // code != 0 keeps the server rejection (risk-control codes get the
        // explicit message), a missing data.list is a parse error, while an
        // empty list stays a valid empty page.
        assert!(parse_popular_chart(&json!({"code": -412, "message": "blocked"})).is_err());
        assert!(parse_popular_chart(&json!({"code": 0, "data": {}})).is_err());
        assert!(parse_ranking_chart(&json!({"code": -352, "message": "risk control"})).is_err());
        assert!(parse_ranking_chart(&json!({"code": 0, "data": {}})).is_err());
        assert!(parse_ranking_chart(&json!({"code": 0, "data": {"list": []}})).is_ok());
        assert!(parse_music_hot_chart(&json!({"code": 0, "data": {}})).is_err());
    }

    #[test]
    fn ranking_chart_params_sign_the_audit_golden_vector() {
        // Formula-class vector (audit 2026-11-18): pins the ranking board's
        // parameter set and ordering — rid=0&type=all — with the frozen-wts
        // convention shared by the V2-V4 vectors above.
        let mut params = BTreeMap::new();
        for (key, value) in ranking_chart_params() {
            params.insert(key.to_owned(), value);
        }
        params.insert("wts".into(), "1700000000".into());
        let query = encode_wbi_query(&params, &golden_wbi_keys());
        assert_eq!(
            query,
            "rid=0&type=all&wts=1700000000&w_rid=59f77402974c7a0b2a942a8bc3283d8e"
        );
    }

    #[test]
    fn chart_duration_and_cover_vectors_hold() {
        // Duration strings (leaderboard.rs:330-340) reuse parse_bili_duration.
        assert_eq!(parse_bili_duration("1:02:03"), Duration::from_secs(3723));
        assert_eq!(parse_bili_duration("3:45"), Duration::from_secs(225));
        assert_eq!(parse_bili_duration("59"), Duration::from_secs(59));
        // Covers (leaderboard.rs:320-328): https: prefix for protocol-relative
        // URLs, empty/whitespace dropped.
        assert_eq!(
            chart_cover(Some("//i0.hdslb.com/x.jpg")).as_deref(),
            Some("https://i0.hdslb.com/x.jpg")
        );
        assert_eq!(chart_cover(Some("   ")), None);
        assert_eq!(chart_cover(Some("")), None);
        assert_eq!(chart_cover(None), None);
    }

    #[tokio::test]
    async fn chart_tracks_rejects_empty_and_unknown_board_ids() {
        // Upstream errors on unknown ids before any request
        // (leaderboard.rs:53); an empty id is a caller-input problem.
        let provider = BilibiliProvider::new();
        assert!(matches!(
            provider.chart_tracks("").await,
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            provider.chart_tracks("  ").await,
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            provider.chart_tracks("not-a-board").await,
            Err(Error::InvalidInput(_))
        ));
    }

    // --- unified transport migration tests (mock transport, no network):
    // the provider runs against Http::from_shared_mock and the recorded
    // request log pins the migrated request faces (TRANSPORT_DESIGN.md §7.5):
    // request_json on send_raw with the cookie whitelist, the provider-side
    // -403/-412 key rotation, the buvid bootstrap, the send_once faces
    // (b23.tv expansion, QR poll) and the Set-Cookie harvesting.

    use crate::transport::{HttpMethod, MockTransport, TransportError, TransportResponse};
    use std::sync::Arc;

    fn mock_json(status: u16, body: &str) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: body.as_bytes().to_vec(),
        }
    }

    fn mock_response(
        status: u16,
        headers: &[(&str, &str)],
        url: Option<&str>,
        body: &str,
    ) -> TransportResponse {
        TransportResponse {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            url: url.map(str::to_owned),
            body: body.as_bytes().to_vec(),
        }
    }

    fn header_of<'a>(request: &'a TransportRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn login_cookies() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("SESSDATA".into(), "s1".into()),
            ("bili_jct".into(), "j1".into()),
            ("buvid3".into(), "b3".into()),
            ("buvid4".into(), "b4".into()),
            ("DedeUserID".into(), "1".into()),
            ("SOME_TRACKER".into(), "dropped".into()),
        ])
    }

    #[tokio::test]
    async fn request_json_runs_through_send_raw_with_cookie_whitelist() {
        let mock = Arc::new(MockTransport::single(mock_json(
            200,
            r#"{"code":0,"data":{"ok":1}}"#,
        )));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        provider.set_cookies(login_cookies(), None, None).unwrap();
        let value = provider
            .get_json(VIEW_ENDPOINT, &[("bvid", "BV1xx411c7mD".into())], "view")
            .await
            .unwrap();
        assert_eq!(value["data"]["ok"], 1);
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        let request = &recorded[0];
        assert_eq!(request.method, HttpMethod::Get);
        assert!(request.idempotent, "GETs keep the engine's retry gate open");
        assert_eq!(
            request.url,
            format!("{VIEW_ENDPOINT}?bvid=BV1xx411c7mD"),
            "get_json still builds the encoded query itself"
        );
        assert_eq!(header_of(request, "referer"), Some(BILI_REFERER));
        assert_eq!(header_of(request, "user-agent"), Some(USER_AGENT));
        let cookie = header_of(request, "cookie").expect("whitelisted cookies must ride along");
        for pair in [
            "SESSDATA=s1",
            "bili_jct=j1",
            "buvid3=b3",
            "buvid4=b4",
            "DedeUserID=1",
        ] {
            assert!(
                cookie.contains(pair),
                "cookie must contain {pair}: {cookie}"
            );
        }
        assert!(
            !cookie.contains("SOME_TRACKER"),
            "tracking cookies stay out of the header (BILI_COOKIE_NAMES filter)"
        );
    }

    #[tokio::test]
    async fn request_json_retries_transport_and_5xx_then_succeeds() {
        // The old fixed-table loop lived here; the merged engine now owns
        // both legs (transport error and 429/5xx), same 5-send budget.
        let mock = Arc::new(MockTransport::script([
            Err(TransportError::Transport("connection reset".into())),
            Ok(mock_json(503, "overloaded")),
            Ok(mock_json(200, r#"{"code":0,"data":{"ok":1}}"#)),
        ]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let value = provider
            .get_json(POPULAR_ENDPOINT, &[], "popular chart")
            .await
            .unwrap();
        assert_eq!(value["data"]["ok"], 1);
        assert_eq!(
            mock.recorded().len(),
            3,
            "one transport retry and one 503 retry, then success"
        );
    }

    #[tokio::test]
    async fn request_json_fatal_status_keeps_label_prefix_and_classify_text() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(404, "no page"))]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider
            .get_json(VIEW_ENDPOINT, &[], "view")
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider bilibili failed: view: HTTP 404",
            "the classify_http Fatal text reaches the caller byte-identically"
        );
        assert_eq!(mock.recorded().len(), 1, "404 is terminal, no retry");
    }

    #[tokio::test]
    async fn request_json_body_parse_failure_is_terminal_and_parse_only() {
        // parse-only after the loop: request_json never classifies the
        // body's own code field, so every call site keeps checking
        // json["code"] itself. The decode failure text carries the raw
        // serde message (declared drift: reqwest's "error decoding response
        // body: " wrapper is gone with the buffered model).
        let mock = Arc::new(MockTransport::script([Ok(mock_json(200, "not json"))]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider
            .get_json(NAV_ENDPOINT, &[], "wbi keys")
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider bilibili failed: wbi keys: expected ident at line 1 column 2",
            "the raw serde message reaches the caller without reqwest's decode wrapper"
        );
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn signed_get_refreshes_wbi_keys_after_gating_code() {
        // "Signature key invalid" is a Provider-layer retry: signed_get sees
        // the gating code in the parsed body, drops the key cache, refetches
        // the nav keys and re-signs once. The transport layer stays a
        // single-shot raw sender per leg.
        for code in [-403, -412] {
            let nav = fixture!("nav_wbi_keys.json").to_string();
            // buvid3 pre-seeded so ensure_buvid short-circuits without a
            // finger fetch; the mock script then pins the exact
            // nav → signed(-403) → nav → re-signed sequence.
            let mock = Arc::new(MockTransport::script([
                Ok(mock_json(200, &nav)),
                Ok(mock_json(
                    200,
                    &format!(r#"{{"code":{code},"message":"gate"}}"#),
                )),
                Ok(mock_json(200, &nav)),
                Ok(mock_json(200, r#"{"code":0,"data":{}}"#)),
            ]));
            let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
            provider
                .set_cookies(BTreeMap::from([("buvid3".into(), "b3".into())]), None, None)
                .unwrap();
            let json = provider
                .signed_get(SEARCH_ENDPOINT, &[("keyword", "x".into())], "search")
                .await
                .expect("the key refresh must recover the request");
            assert_eq!(json["code"], 0, "code {code} must recover after re-signing");
            let recorded = mock.recorded();
            assert_eq!(
                recorded.len(),
                4,
                "code {code}: nav, signed, nav, re-signed"
            );
            assert_eq!(recorded[0].url, NAV_ENDPOINT, "keys fetched once first");
            for signed in [&recorded[1], &recorded[3]] {
                assert!(
                    signed.url.starts_with(SEARCH_ENDPOINT),
                    "code {code}: the signed query hits the search endpoint"
                );
                assert!(
                    signed.url.contains("wts=") && signed.url.contains("w_rid="),
                    "code {code}: the WBI signature rides in the query"
                );
                assert!(
                    signed.url.contains("keyword=x"),
                    "code {code}: business params stay in the signed query"
                );
            }
            assert_eq!(
                recorded[2].url, NAV_ENDPOINT,
                "keys refetched after the gate"
            );
        }
    }

    #[tokio::test]
    async fn buvid_bootstrap_fetches_finger_once_and_rides_along() {
        let nav = fixture!("nav_wbi_keys.json").to_string();
        let mock = Arc::new(MockTransport::script([
            Ok(mock_json(
                200,
                r#"{"code":0,"data":{"b_3":"b3-value","b_4":"b4-value"}}"#,
            )),
            Ok(mock_json(200, &nav)),
            Ok(mock_json(200, r#"{"code":0,"data":{}}"#)),
            // Second signed_get: no finger fetch, no nav fetch (keys cached).
            Ok(mock_json(200, r#"{"code":0,"data":{}}"#)),
        ]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        for _ in 0..2 {
            let json = provider
                .signed_get(SEARCH_ENDPOINT, &[("keyword", "x".into())], "search")
                .await
                .unwrap();
            assert_eq!(json["code"], 0);
        }
        let session = provider.session_store().get(ID).unwrap();
        assert_eq!(
            session.cookies.get("buvid3").map(String::as_str),
            Some("b3-value")
        );
        assert_eq!(
            session.cookies.get("buvid4").map(String::as_str),
            Some("b4-value")
        );
        let recorded = mock.recorded();
        assert_eq!(
            recorded.iter().filter(|r| r.url == FINGER_ENDPOINT).count(),
            1,
            "the bootstrap fetch runs once, then the session cookie answers it"
        );
        assert_eq!(
            recorded.iter().filter(|r| r.url == NAV_ENDPOINT).count(),
            1,
            "the WBI keys stay cached across signed_get calls"
        );
        assert_eq!(recorded.len(), 4, "finger, nav, search, search");
        let cookie = header_of(&recorded[2], "cookie").expect("bootstrap cookie must ride along");
        assert!(
            cookie.contains("buvid3=b3-value"),
            "the signed call after bootstrap carries the fingerprint: {cookie}"
        );
    }

    #[tokio::test]
    async fn qr_poll_harvests_set_cookie_and_url_query_into_the_session() {
        let mock = Arc::new(MockTransport::script([
            Ok(mock_response(
                200,
                &[
                    ("set-cookie", "SESSDATA=abc; Path=/; HttpOnly"),
                    ("set-cookie", "bili_jct=jct-9; Path=/"),
                    ("set-cookie", "tracker=tk; Path=/"),
                ],
                None,
                r#"{"code":0,"data":{"code":0,"url":"https://passport.bilibili.com/x/passport-login/web?DedeUserID=42&ac_time_value=1710000000","refresh_token":"tok-1"}}"#,
            )),
            Ok(mock_json(
                200,
                r#"{"code":0,"data":{"isLogin":true,"uname":"用户","mid":42}}"#,
            )),
        ]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let poll = provider.poll_qr_login("key-1").await.unwrap();
        assert_eq!(poll.status, QrLoginStatus::Success);
        assert_eq!(poll.user_id.as_deref(), Some("42"));
        assert_eq!(poll.display_name.as_deref(), Some("用户"));
        let session = provider.session_store().get(ID).unwrap();
        assert_eq!(
            session.cookies.get("SESSDATA").map(String::as_str),
            Some("abc")
        );
        assert_eq!(
            session.cookies.get("bili_jct").map(String::as_str),
            Some("jct-9")
        );
        assert_eq!(
            session.cookies.get("DedeUserID").map(String::as_str),
            Some("42"),
            "the data.url query pairs join the session cookies"
        );
        assert_eq!(
            session.tokens.get("refresh_token").map(String::as_str),
            Some("tok-1")
        );
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2, "poll, then the nav login confirmation");
        let poll_request = &recorded[0];
        assert_eq!(poll_request.method, HttpMethod::Get);
        assert_eq!(header_of(poll_request, "user-agent"), Some(USER_AGENT));
        assert_eq!(header_of(poll_request, "referer"), Some(BILI_REFERER));
        assert!(
            header_of(poll_request, "cookie").is_none(),
            "today's poll attaches no session cookies; keep it bare"
        );
        assert_eq!(recorded[1].url, NAV_ENDPOINT);
    }

    #[tokio::test]
    async fn qr_poll_request_failure_and_parse_failure_keep_their_labels() {
        let mock = Arc::new(MockTransport::script([Err(TransportError::Transport(
            "connection reset".into(),
        ))]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider.poll_qr_login("key-1").await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider bilibili failed: QR poll request failed: connection reset"
        );
        let mock = Arc::new(MockTransport::script([Ok(mock_json(200, "not json"))]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider.poll_qr_login("key-1").await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider bilibili failed: QR poll parse failed: expected ident at line 1 column 2"
        );
    }

    #[tokio::test]
    async fn short_link_expansion_is_a_send_once_face() {
        // The expansion success is the FINAL url, not the status: today's
        // client follows redirects and never classifies the HTTP status, so
        // even a 404 answer passes through and its url gets parsed.
        // `send_raw` would collapse the 404 into a terminal error and drop
        // the response.
        let mock = Arc::new(MockTransport::script([Ok(mock_response(
            404,
            &[],
            Some("https://www.bilibili.com/video/BV1xx411c7mD/?share_source=copy_web&p=2"),
            "redirect body",
        ))]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let reference = provider
            .resolve_video_reference("https://b23.tv/abcd")
            .await
            .unwrap();
        assert!(matches!(
            reference,
            Some(VideoReference::Bvid {
                ref bvid,
                page: Some(2)
            }) if bvid == "BV1xx411c7mD"
        ));
        let recorded = mock.recorded();
        assert_eq!(
            recorded.len(),
            1,
            "send_once: exactly one attempt, no retry"
        );
        assert_eq!(recorded[0].url, "https://b23.tv/abcd");
        assert_eq!(header_of(&recorded[0], "user-agent"), Some(USER_AGENT));
        assert_eq!(header_of(&recorded[0], "referer"), Some(BILI_REFERER));
        assert!(
            header_of(&recorded[0], "cookie").is_none(),
            "the expansion attaches no session cookies today"
        );
        // Transport failure keeps today's error label.
        let mock = Arc::new(MockTransport::script([Err(TransportError::Transport(
            "dns failure".into(),
        ))]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider
            .resolve_video_reference("https://b23.tv/abcd")
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider bilibili failed: short link expansion failed: dns failure"
        );
    }

    // --- provider-error-auth-bilibili slice (AUTH_REGISTRY_DESIGN.md §8
    // slice 5 / §9.4): the -101 login-invalid write point, the AuthState
    // wiring, the exhausted -403/-412 gate classification and the QR fold. ---

    fn sample_track() -> Track {
        Track {
            id: TrackId::new(ID, "BV1xx411c7mD"),
            title: "测试视频".into(),
            artists: vec!["UP主".into()],
            album: None,
            album_id: None,
            duration: Some(Duration::from_secs(60)),
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: BTreeMap::new(),
        }
    }

    fn plant_expired_marker(provider: &BilibiliProvider) {
        let mut session = provider.session_store().get(ID).unwrap();
        session.tokens.insert("login_expired".into(), "1".into());
        provider.session_store().set(ID, session).unwrap();
    }

    #[tokio::test]
    async fn api_not_logged_in_verdict_marks_the_session_expired() {
        // -101 with SESSDATA riding along is the login-invalid write point:
        // the marker lands in the store and the classified error surfaces.
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":-101,"message":"账号未登录"}"#,
        ))]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        provider
            .set_cookies(login_cookies(), Some("42".into()), Some("用户".into()))
            .unwrap();
        let error = provider
            .get_json(VIEW_ENDPOINT, &[("bvid", "BV1xx411c7mD".into())], "view")
            .await
            .unwrap_err();
        assert_eq!(error.category(), crate::ErrorCategory::SessionExpired);
        assert_eq!(error.provider_id(), Some(ID));
        assert_eq!(error.detail(), Some("view"), "the label rides in detail");
        assert!(!error.is_retryable());
        assert!(
            error
                .to_string()
                .contains("login required or session expired"),
            "the classified Display body: {error}"
        );
        // Both views report Expired and the marker is persisted.
        assert_eq!(provider.auth_state(), AuthState::Expired);
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);
        assert!(
            provider
                .session_store()
                .get(ID)
                .unwrap()
                .tokens
                .contains_key("login_expired")
        );
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn anonymous_not_logged_in_body_stays_passthrough() {
        // The nav/WBI-keys shape: anonymous callers get code -101 and the
        // keys ride along anyway. Without SESSDATA the -101 must NOT mark
        // anything — the body passes through untouched.
        let body = r#"{"code":-101,"message":"账号未登录","data":{"wbi_img":{"img_url":"https://i1.hdslb.com/bfs/wbi/0a1b2c3d4e5f60718293a4b5c6d7e8f1.jpg","sub_url":"https://i1.hdslb.com/bfs/wbi/99887766554433221100ffeeddccbbaa.jpg"}}}"#;
        let mock = Arc::new(MockTransport::script([Ok(mock_json(200, body))]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock));
        let json = provider
            .request_json(NAV_ENDPOINT, "wbi keys")
            .await
            .unwrap();
        assert_eq!(json["code"], -101);
        assert!(json["data"]["wbi_img"]["img_url"].as_str().is_some());
        assert!(provider.session_store().get(ID).is_none());
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
    }

    #[tokio::test]
    async fn stale_session_turns_the_wbi_keys_nav_into_session_expired() {
        // Declared drift of the write point: a stored but rejected SESSDATA
        // now surfaces the classified error instead of silently degrading to
        // anonymous on the previously -101-tolerant nav fetch.
        let nav = fixture!("nav_wbi_keys.json").to_string();
        let mock = Arc::new(MockTransport::script([Ok(mock_json(200, &nav))]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
        provider.set_cookies(login_cookies(), None, None).unwrap();
        let error = provider
            .signed_get(SEARCH_ENDPOINT, &[("keyword", "x".into())], "search")
            .await
            .unwrap_err();
        assert_eq!(error.category(), crate::ErrorCategory::SessionExpired);
        assert_eq!(error.detail(), Some("wbi keys"));
        assert_eq!(
            mock.recorded().len(),
            1,
            "the nav alone, no re-sign after the expiry verdict"
        );
        assert_eq!(provider.auth_state(), AuthState::Expired);
    }

    #[tokio::test]
    async fn login_status_views_stay_in_step_with_auth_state() {
        // login_status reports through auth_state; the legacy behavior is
        // preserved and the Expired branch is now reachable.
        let provider = BilibiliProvider::new();
        // No session at all.
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        // Secondary cookies without SESSDATA stay LoggedOut.
        provider
            .set_cookies(BTreeMap::from([("buvid3".into(), "b3".into())]), None, None)
            .unwrap();
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        // SESSDATA without the marker: Authenticated / LoggedIn.
        provider
            .set_cookies(login_cookies(), Some("42".into()), Some("用户".into()))
            .unwrap();
        assert_eq!(provider.auth_state(), AuthState::Authenticated);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedIn {
                user_id: Some("42".into()),
                display_name: Some("用户".into()),
            }
        );
        // The write-point product: Expired in both views.
        plant_expired_marker(&provider);
        assert_eq!(provider.auth_state(), AuthState::Expired);
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);
    }

    #[tokio::test]
    async fn gate_codes_survive_the_key_rotation_as_categorized_errors() {
        // After the single key-rotation retry the -403/-412 gate no longer
        // falls through to the call site's Provider string: -403 is
        // PermissionDenied (typed code slot), -412 is RateLimited (the only
        // retryable one of the pair).
        for (code, category, retryable, display_prefix) in [
            (
                -403,
                crate::ErrorCategory::PermissionDenied,
                false,
                "permission denied (API code -403)",
            ),
            (
                -412,
                crate::ErrorCategory::RateLimited,
                true,
                "rate limited",
            ),
        ] {
            let nav = fixture!("nav_wbi_keys.json").to_string();
            let gate = format!(r#"{{"code":{code},"message":"gate"}}"#);
            let mock = Arc::new(MockTransport::script([
                Ok(mock_json(200, &nav)),
                Ok(mock_json(200, &gate)),
                Ok(mock_json(200, &nav)),
                Ok(mock_json(200, &gate)),
            ]));
            let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock.clone()));
            provider
                .set_cookies(BTreeMap::from([("buvid3".into(), "b3".into())]), None, None)
                .unwrap();
            let error = provider
                .signed_get(SEARCH_ENDPOINT, &[("keyword", "x".into())], "search")
                .await
                .unwrap_err();
            assert_eq!(error.category(), category, "code {code}");
            assert_eq!(error.provider_id(), Some(ID), "code {code}");
            assert_eq!(error.is_retryable(), retryable, "code {code}");
            assert_eq!(error.detail(), Some("search"), "code {code}");
            assert!(
                error.to_string().starts_with(display_prefix),
                "code {code}: {error}"
            );
            assert_eq!(
                mock.recorded().len(),
                4,
                "code {code}: nav, signed, nav, re-signed — then classify"
            );
        }
    }

    #[test]
    fn qr_statuses_fold_into_the_auth_state_machine() {
        // data.code → flow status → session state (traits.rs §2.3 fold).
        // 86038 is the ONE expired code in bilibili's QR protocol; the 998
        // alias named in the ask does not exist anywhere in the protocol
        // (upstream voicefox source/src/bili/mod.rs:239-241, fixtures) and
        // must stay an error, not an Expired mapping.
        assert_eq!(qr_status_from_code(0), Some(QrLoginStatus::Success));
        assert_eq!(qr_status_from_code(86101), Some(QrLoginStatus::Waiting));
        assert_eq!(qr_status_from_code(86090), Some(QrLoginStatus::Scanned));
        assert_eq!(qr_status_from_code(86038), Some(QrLoginStatus::Expired));
        assert_eq!(qr_status_from_code(998), None);
        assert_eq!(qr_status_from_code(-1), None);
        // The fold: success arms the session, flow states stay pending — QR
        // expiry is flow-internal (a fresh code is shown), never session
        // expiry.
        assert_eq!(
            AuthState::from(&QrLoginStatus::Success),
            AuthState::Authenticated
        );
        for status in [
            QrLoginStatus::Waiting,
            QrLoginStatus::Scanned,
            QrLoginStatus::Expired,
        ] {
            assert_eq!(AuthState::from(&status), AuthState::PendingQrScan);
        }
    }

    #[tokio::test]
    async fn qr_poll_undocumented_code_errors_without_session_changes() {
        // 998 is not a QR expired code: the poll reports it as an error and
        // the stored session is untouched.
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":0,"data":{"code":998,"message":"unknown"}}"#,
        ))]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock));
        let error = provider.poll_qr_login("key-1").await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider bilibili failed: QR poll failed (code=998)"
        );
        assert!(provider.session_store().get(ID).is_none());
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
    }

    #[tokio::test]
    async fn qr_success_clears_a_stale_expiry_marker() {
        // §2.2 Expired→Authenticated edge: a fresh scan is a successful
        // re-login, so the stale `login_expired` marker must not survive it.
        let mock = Arc::new(MockTransport::script([
            Ok(mock_response(
                200,
                &[("set-cookie", "SESSDATA=fresh; Path=/")],
                None,
                r#"{"code":0,"data":{"code":0,"url":"https://passport.bilibili.com/x/passport-login/web?DedeUserID=42","refresh_token":"tok-2"}}"#,
            )),
            Ok(mock_json(
                200,
                r#"{"code":0,"data":{"isLogin":true,"uname":"用户","mid":42}}"#,
            )),
        ]));
        let provider = BilibiliProvider::with_transport(Http::from_shared_mock(mock));
        provider
            .set_cookies(login_cookies(), Some("42".into()), Some("用户".into()))
            .unwrap();
        plant_expired_marker(&provider);
        assert_eq!(provider.auth_state(), AuthState::Expired);
        let poll = provider.poll_qr_login("key-1").await.unwrap();
        assert_eq!(poll.status, QrLoginStatus::Success);
        let session = provider.session_store().get(ID).unwrap();
        assert!(
            !session.tokens.contains_key("login_expired"),
            "the stale marker is cleared by the successful re-login"
        );
        assert_eq!(
            session.cookies.get("SESSDATA").map(String::as_str),
            Some("fresh")
        );
        assert_eq!(provider.auth_state(), AuthState::Authenticated);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedIn {
                user_id: Some("42".into()),
                display_name: Some("用户".into()),
            }
        );
    }

    #[tokio::test]
    async fn unsupported_sites_map_to_unsupported_operation() {
        // The five unimplemented faces carry the provider id in the typed
        // slot and the site reason in the detail slot; error.rs Display
        // impls are untouched ("unsupported operation" + detail suffix).
        let provider = BilibiliProvider::new();
        let track = sample_track();
        let checks: Vec<(&str, Error)> = vec![
            ("lyrics", provider.lyrics(&track).await.unwrap_err()),
            ("playlist", provider.playlist("1").await.unwrap_err()),
            ("categories", provider.categories().await.unwrap_err()),
            (
                "search_playlists",
                provider.search_playlists("x", 1).await.unwrap_err(),
            ),
            (
                "user_playlists",
                provider.user_playlists(1, 20).await.unwrap_err(),
            ),
        ];
        for (site, error) in checks {
            assert_eq!(
                error.category(),
                crate::ErrorCategory::UnsupportedOperation,
                "{site}"
            );
            assert_eq!(error.provider_id(), Some(ID), "{site}");
            assert!(!error.is_retryable(), "{site}");
            assert!(error.detail().is_some(), "{site}: the reason rides along");
            assert!(
                error.to_string().starts_with("unsupported operation"),
                "{site}: {error}"
            );
        }
    }
}
