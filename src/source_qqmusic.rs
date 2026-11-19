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
    Error, ErrorDetail, Result,
    lrc::parse_lrc,
    models::{
        Album, AudioFormat, Chart, ChartEntry, LyricDocument, LyricFormat, Playlist,
        PlaylistCategory, StreamInfo, StreamQuality, Track, TrackId,
    },
    provider::{
        AlbumProvider, AuthState, AuthenticationProvider, ChartProvider, CollectionProvider,
        LinkResolver, LoginStatus, LyricsProvider, MusicProvider, PlaylistProvider,
        QrAuthenticationProvider, QrLoginKind, QrLoginPoll, QrLoginSession, QrLoginStatus,
        ResolveOptions, ResolvedLink, SearchPage, SearchQuery, StreamResolver, TrackSearch,
    },
    retry::{QQ_CODES, StepOutcome, classify_api},
    session::{ProviderSession, SessionStore},
    transport::{
        ClientProfile, Http, RetryPolicy, TransportError, TransportRequest, TransportResponse,
    },
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
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
    /// Business face (musicu.fcg, fcg endpoints, WeChat channel): follows
    /// redirects, 15s client timeout, via the unified transport layer.
    http: Http,
    /// Login face (y.qq.com QR flow): `redirect_none` so redirects are
    /// followed manually hop by hop and every hop's Set-Cookie headers can
    /// be harvested.
    login_http: Http,
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
        // Client construction moved onto the unified transport layer as two
        // `ClientProfile`s (TRANSPORT_DESIGN.md §4.3): the business client
        // carries exactly today's knobs (15s total timeout, follow-default);
        // the login client is `provider_default()` + `redirect_none: true`,
        // mapping the old `redirect::Policy::none()` login client verbatim.
        let http = Http::from_profile(&ClientProfile::provider_default());
        let login_http = Http::from_profile(&ClientProfile {
            redirect_none: true,
            ..ClientProfile::provider_default()
        });
        Self {
            http,
            login_http,
            sessions: SessionStore::default(),
        }
    }

    /// Test seam: inject both transport handles (mocks in unit tests). The
    /// production constructor is [`QqMusicProvider::new`], whose two profiles
    /// carry the redirect policies. Test-only so non-test builds see no dead
    /// constructor.
    #[cfg(test)]
    pub(crate) fn with_transports(http: Http, login_http: Http) -> Self {
        Self {
            http,
            login_http,
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
                ..Default::default()
            },
        )
    }
    /// Flag the current session as expired so `Self::auth_state` (and
    /// `login_status` through it) reports it, and return the classified
    /// [`Error::SessionExpired`] for the caller to propagate — the M2 landing
    /// spot for the `retry` classifier's `expired=[1000, 301]` verdict
    /// (AUTH_REGISTRY_DESIGN.md §1.1, §8 slice 2). The legacy free-form
    /// "login required or session expired" string is replaced by the variant,
    /// whose Display body is that exact text; `detail` carries the call-site
    /// label, the only underlying information the classifier's verdict
    /// preserves.
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
    /// Derive the unified [`AuthState`] from the stored session (no network
    /// I/O). The `login_expired` token written by
    /// [`Self::mark_session_expired`] is the Expired bit; a session carrying
    /// both an identity cookie (uin/wxuin) and a key cookie
    /// (qqmusic_key/qm_keyst) is `Authenticated`; anything else is
    /// `LoggedOut`. This is the AuthState wiring for the M2 auth state
    /// machine (AUTH_REGISTRY_DESIGN.md §2, §8 slice 2): `login_status`
    /// reports through this state instead of hand-classifying the session.
    fn auth_state(&self) -> AuthState {
        match self.sessions.get(ID) {
            None => AuthState::LoggedOut,
            Some(session) if !has_login_cookies(&session) => AuthState::LoggedOut,
            Some(session) if session.tokens.contains_key("login_expired") => AuthState::Expired,
            Some(_) => AuthState::Authenticated,
        }
    }
    /// Adds the request-level headers (Referer, web UA) and the session
    /// cookies to a [`TransportRequest`]. Platform-specific and kept in the
    /// provider (TRANSPORT_DESIGN.md §1); QQ sends every session cookie,
    /// unlike netease's whitelist. Header order (referer, user-agent, cookie)
    /// and the per-request UA append below in `musicu_inner` carry over
    /// verbatim from the old `RequestBuilder` chain.
    fn request(&self, mut request: TransportRequest) -> TransportRequest {
        request = request
            .header("referer", REFERER)
            .header("user-agent", UA_WEB);
        if let Some(session) = self.sessions.get(ID) {
            let cookie = session
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            if !cookie.is_empty() {
                request = request.header("cookie", cookie);
            }
        }
        request
    }
    /// GET through the unified transport layer (`send_raw` +
    /// `RetryPolicy::standard()`): transport errors and 429/5xx retry under
    /// exponential backoff + jitter + `Retry-After`, replacing the fixed
    /// `[600,1200,2400,4000]` table. The send count is unchanged (at most 5);
    /// the backoff shape change is declared in TRANSPORT_DESIGN.md §5.5 rows
    /// 1-2. Non-retryable statuses keep the exact `classify_http` text
    /// (`{label}: HTTP {status}`).
    async fn raw_get(&self, url: &str, label: &str) -> Result<TransportResponse> {
        let request = self.request(TransportRequest::get(url));
        match self.http.send_raw(&request, &RetryPolicy::standard()).await {
            Ok(response) => Ok(response),
            Err(error) => {
                tracing::debug!("QQ {label} failed: {error}");
                Err(perr(format!("{label}: {error}")))
            }
        }
    }
    async fn get_json(&self, url: &str, label: &str) -> Result<Value> {
        let response = self.raw_get(url, label).await?;
        let value = response
            .json::<Value>()
            .map_err(|e| perr(format!("{label}: {e}")))?;
        // The API-level classification has no retry semantics for QQ (no
        // throttled code), so attempt == budget is always correct here; it
        // stays provider-side because this face runs on `send_raw`.
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
                Err(self.mark_session_expired(Some(ErrorDetail::new(label))))
            }
            StepOutcome::Fatal(m) => Err(api_code_error(label, &m)),
        }
    }
    /// GET endpoints that answer with JSONP or plain JSON; the callback shell
    /// is stripped. API-level `code` checks stay at the call sites because the
    /// legacy fcg endpoints vary in shape.
    async fn get_jsonp(&self, url: &str, label: &str) -> Result<Value> {
        let response = self.raw_get(url, label).await?;
        // The body was buffered by the transport, so today's separate
        // text-read failure is unreachable (§5.5 row 4): a mid-body reset
        // surfaces as a `send_raw` transport error above.
        let text = response.text();
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
    /// musicu POST through the unified transport layer (`send_api_json`).
    /// The signed/unsigned JSON body is resent on retries, declared with
    /// `.idempotent(true)` per TRANSPORT_DESIGN.md §5.2 so the engine's gate
    /// allows it at all three resend sites. Today's fixed `[400,900,1800]`
    /// table allowed 4 sends; `standard()` allows 5 (§5.5 row 2) with
    /// exponential backoff + jitter + `Retry-After`. Classification stays in
    /// `retry` (`classify_http` / `classify_api` via the engine);
    /// session-expiry and fatal body codes surface as the classified M2
    /// errors (`mark_session_expired` / `api_code_error`, the label rides in
    /// the detail slot) while the remaining transport texts keep today's
    /// `{label}: {error}` shape.
    async fn musicu_inner(
        &self,
        url: &str,
        body: &str,
        user_agent: &str,
        label: &str,
    ) -> Result<Value> {
        let request = self
            .request(TransportRequest::post(url).idempotent(true))
            .header("user-agent", user_agent)
            .header("content-type", "application/json")
            .body(body.to_owned());
        match self
            .http
            .send_api_json(&request, &RetryPolicy::standard(), &QQ_CODES, qq_code_of)
            .await
        {
            Ok(value) => Ok(value),
            Err(TransportError::SessionExpired) => {
                Err(self.mark_session_expired(Some(ErrorDetail::new(label))))
            }
            Err(TransportError::Api(text)) => Err(api_code_error(label, &text)),
            Err(error) => {
                tracing::debug!("QQ {label} failed: {error}");
                Err(perr(format!("{label}: {error}")))
            }
        }
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
/// Recover the platform API code from the shared classifier's fatal text.
/// `retry::classify_api` formats fatal outcomes as `"API code {code}: ..."`
/// (`retry.rs:100-103`) and the transport layer carries only that text
/// ([`TransportError::Api`]), so the typed `code` slot of the M2 variant is
/// fed by parsing this pinned shape. Texts without the prefix (parse
/// failures, transport notes) yield `None` and keep the legacy catch-all.
/// Mirrors the netease slice's helper (AUTH_REGISTRY_DESIGN.md §8 slice 2).
fn fatal_api_code(text: &str) -> Option<i64> {
    text.strip_prefix("API code ")?
        .split_once(':')?
        .0
        .trim()
        .parse()
        .ok()
}
/// Classify a fatal body-code text into the M2 taxonomy (AUTH_REGISTRY_DESIGN.md
/// §1.1 qqmusic row, §8 slice 2): QQ declares no throttled body code
/// (`QQ_CODES.throttled` is `None`), so every recognizable platform code is
/// an upstream contract violation ([`Error::InvalidResponse`] with the code
/// preserved); unrecognizable texts keep the legacy [`Error::Provider`]
/// catch-all with the exact pre-migration `{label}: {text}` shape. The label
/// rides in the detail slot, exactly like the netease slice.
fn api_code_error(label: &str, text: &str) -> Error {
    match fatal_api_code(text) {
        Some(code) => Error::InvalidResponse {
            provider: Some(ID.into()),
            code: Some(code),
            detail: Some(ErrorDetail::new(format!("{label}: {text}"))),
        },
        None => perr(format!("{label}: {text}")),
    }
}
/// `(code, message)` extractor handed to `Http::send_api_json`; extraction
/// stays provider-side, classification lives in `retry` (TRANSPORT_DESIGN.md
/// §4.1). `message` falls back to `msg` (both spellings occur), and a body
/// without a code field falls back to the OK code, exactly like the previous
/// `unwrap_or(QQ_CODES.ok)` loops (review P-7).
fn qq_code_of(value: &Value) -> (i64, String) {
    (
        value["code"].as_i64().unwrap_or(QQ_CODES.ok),
        value["message"]
            .as_str()
            .or_else(|| value["msg"].as_str())
            .unwrap_or("unknown error")
            .to_owned(),
    )
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
/// Whether the vkey envelope listed a fully usable file entry (non-empty
/// purl and vkey) but withheld the sip host list (every entry blank). That
/// shape — pinned by the `stream_vkey_empty_sip` fixture — is QQ's
/// "not playable anonymously" risk-control signal; the complementary shape
/// (usable host list, purl/vkey withheld) is the VIP/cookie gate.
fn stream_host_withheld(value: &Value) -> bool {
    let usable_entry = value["req_0"]["data"]["midurlinfo"]
        .as_array()
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                !entry["purl"].as_str().unwrap_or_default().trim().is_empty()
                    && !entry["vkey"].as_str().unwrap_or_default().trim().is_empty()
            })
        });
    let usable_host = value["req_0"]["data"]["sip"]
        .as_array()
        .is_some_and(|hosts| {
            hosts
                .iter()
                .any(|host| !host.as_str().unwrap_or_default().trim().is_empty())
        });
    usable_entry && !usable_host
}
/// Classify the "musicu answered but no playable stream URL came back"
/// outcome (AUTH_REGISTRY_DESIGN.md §8 slice 2; the upgraded
/// source_qqmusic.rs:714-718 prose). Discriminated by the vkey module code:
///
/// - module code 0 with the host list withheld while an entry stays usable:
///   the risk-control signal (see [`stream_host_withheld`]) →
///   [`Error::PermissionDenied`];
/// - module code 0 otherwise (purl/vkey withheld): the VIP/cookie gate —
///   lx-music's official tx source comments this exact empty-purl shape as
///   "// vip" → [`Error::VipRequired`] (需会员/无 cookie);
/// - any other module code, or no module envelope at all: the vkey module
///   itself refused; documented in the ecosystem as an intermittent
///   server-side failure rather than a content property →
///   [`Error::Unavailable`].
///
/// The detail slot preserves the pre-redacted module-envelope facts
/// (AUTH_REGISTRY_DESIGN.md §1.2); cookies and request URLs never enter it.
fn no_playable_stream_error(value: &Value) -> Error {
    let module_code = value["req_0"]["code"].as_i64();
    let detail = ErrorDetail::new(format!(
        "stream resolve: no playable stream URL returned (module code {module_code:?})"
    ));
    match module_code {
        Some(0) if stream_host_withheld(value) => Error::PermissionDenied {
            provider: Some(ID.into()),
            code: None,
            detail: Some(detail),
        },
        Some(0) => Error::VipRequired {
            provider: Some(ID.into()),
            detail: Some(detail),
        },
        _ => Error::Unavailable {
            provider: Some(ID.into()),
            detail: Some(detail),
        },
    }
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
        audio: None,
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
        let has_more = search_has_more(page, limit, total);
        Ok(SearchPage {
            items,
            total,
            has_more,
        })
    }
}
/// `has_more = page * limit < total`; a missing totalnum means no more pages.
fn search_has_more(page: u32, limit: u32, total: Option<u64>) -> bool {
    total.is_some_and(|t| u64::from(page) * u64::from(limit) < t)
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
            return Err(no_playable_stream_error(&value));
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
            size_is_advisory: matches!(candidate.1, StreamQuality::Lossless | StreamQuality::HiRes),
            checksum_algorithm: None,
            fallback_endpoints: vec![],
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
        Ok(parse_lyric_response(&value))
    }
}
/// Module code != 0 means "no lyrics", not an API failure; a missing or
/// undecodable `lyric` field likewise yields no document.
fn parse_lyric_response(value: &Value) -> Option<LyricDocument> {
    let data = &value["req"];
    if data["code"].as_i64().unwrap_or(0) != 0 {
        return None;
    }
    let original = decode_b64(&data["data"]["lyric"])?;
    Some(LyricDocument {
        format: LyricFormat::Lrc,
        lines: parse_lrc(&original),
        translation: decode_b64(&data["data"]["trans"]),
        romanization: decode_b64(&data["data"]["roma"]),
        original,
    })
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
        // Single-shot GETs on the business face, no retry, exactly as today:
        // `send_once` because today's faces read the body whatever the status
        // is (a non-2xx page still fails at uuid extraction, not at the
        // status classifier).
        let response = self
            .http
            .send_once(
                &TransportRequest::get(&connect_url)
                    .header("user-agent", LOGIN_UA)
                    .header("referer", "https://y.qq.com/"),
            )
            .await
            // The body is buffered by the transport, so today's separate
            // "page read failed" text is unreachable (§5.5 row 4): a mid-body
            // reset surfaces as a send error.
            .map_err(|e| perr(format!("WeChat QR page request failed: {e}")))?;
        let html = response.text().into_owned();
        let uuid =
            extract_wx_uuid(&html).ok_or_else(|| perr("WeChat QR page did not contain a uuid"))?;
        let jpeg = self
            .http
            .send_once(
                &TransportRequest::get(format!("{WX_QRCODE_URL}/{uuid}"))
                    .header("user-agent", LOGIN_UA)
                    .header("referer", "https://y.qq.com/"),
            )
            .await
            .map_err(|e| perr(format!("WeChat QR image request failed: {e}")))?
            .body;
        if jpeg.is_empty() {
            return Err(perr("WeChat QR image is empty"));
        }
        self.set_wx_seen_scan(&uuid, false);
        // The JPEG bytes are already in hand from the qrconnect code image,
        // so `image_png` is filled with the same base64 the `url` data URI
        // carries — one encode, two slots (M3 QR provider slice,
        // MODEL_ALIGNMENT_DESIGN.md §6 landed deltas). The slot keeps its
        // upstream name although WeChat serves JPEG: voicefox
        // `login.rs:86-89` stores the JPEG base64 there too.
        let jpeg_b64 = STANDARD.encode(&jpeg);
        Ok(QrLoginSession {
            key: format!("{WX_KEY_PREFIX}{uuid}"),
            url: format!("data:image/jpeg;base64,{jpeg_b64}"),
            expires_in: WX_EXPIRES_IN,
            image_png: Some(jpeg_b64),
            kind: QrLoginKind::WeChat,
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
        // WeChat long-poll blocks server-side for ~15s; the 35s
        // `WX_POLL_TIMEOUT` rides as the per-request timeout override
        // (TRANSPORT_DESIGN.md §4.3), which reqwest applies instead of the
        // profile's 15s client timeout. One `send_once` attempt, no retry,
        // exactly as today.
        let response = self
            .http
            .send_once(
                &TransportRequest::get(&poll_url)
                    .timeout(WX_POLL_TIMEOUT)
                    .header("user-agent", LOGIN_UA)
                    .header("referer", "https://y.qq.com/"),
            )
            .await
            .map_err(|e| perr(format!("WeChat poll request failed: {e}")))?;
        let body = response.text().into_owned();
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
            "403" => {
                // M2 migration (AUTH_REGISTRY_DESIGN.md §1.1, §8 slice 2):
                // the WeChat rejection is a credentials-rejected outcome, so
                // the plain Failed poll status is upgraded to the classified
                // error; the poll terminates instead of reporting a
                // retryable-looking status. The original user-facing message
                // is preserved in the detail slot.
                return Err(Error::AuthenticationFailed {
                    provider: Some(ID.into()),
                    code: Some(403),
                    detail: Some(ErrorDetail::new("微信登录被拒绝")),
                });
            }
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
        // Host failover, not a time retry: each hop is one `send_once`
        // attempt (TRANSPORT_DESIGN.md §4.1, review P-2); the loop's host
        // rotation stays the only retry, exactly as today (single attempt
        // per host, no extra time retry on top).
        for api in std::iter::once(MUSICU_URL).chain(WX_MUSICU_FALLBACKS) {
            let request = TransportRequest::post(api)
                .header("user-agent", LOGIN_UA)
                .header("referer", "https://y.qq.com/")
                .header("origin", "https://y.qq.com")
                .header("cookie", "login_type=2")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(payload.clone());
            match self.http.send_once(&request).await {
                Ok(response) => {
                    // u16 status: today's message printed the full
                    // StatusCode ("502 Bad Gateway"); the buffered model
                    // carries the bare code ("502"). Declared text drift.
                    let status = response.status;
                    let text = response.text().into_owned();
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
        Ok(merge_user_playlists(&created, &collected))
    }
}
/// Created playlists first, then collected ones whose id is not already
/// present (the same list can be both created and collected).
fn merge_user_playlists(created: &Value, collected: &Value) -> Vec<Playlist> {
    let mut result = created_playlists(created);
    for item in collected["data"]["cdlist"].as_array().into_iter().flatten() {
        if let Some(playlist) = parse_user_playlist(item)
            && !result.iter().any(|existing| existing.id == playlist.id)
        {
            result.push(playlist);
        }
    }
    result
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
        Ok(chart_song_entries(&value))
    }
}
/// Chart entries keep list order and are ranked 1..N after unparseable
/// songs are dropped, so ranks stay contiguous.
fn chart_song_entries(value: &Value) -> Vec<ChartEntry> {
    value["toplist"]["data"]["songInfoList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_song)
        .enumerate()
        .map(|(index, track)| ChartEntry {
            rank: (index + 1) as u32,
            track,
        })
        .collect()
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
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
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
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
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
        provider: Some(ID.to_owned()),
        extra: Default::default(),
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
                provider: Some(ID.to_owned()),
                extra: Default::default(),
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
        updated_description: None,
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
/// Harvest `name=value` from every Set-Cookie header of one hop. The values
/// arrive pre-filtered by `TransportResponse::all("set-cookie")`, which skips
/// lossy-mangled (non-UTF-8 on the wire) entries — the same drop semantics as
/// the old `HeaderValue::to_str().ok()` loop (§5.5 row 9).
fn collect_cookies(values: &[&str], into: &mut BTreeMap<String, String>) {
    for value in values {
        if let Some(pair) = value.split(';').next()
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
        // Transport errors are ignored, exactly like today's `if let Ok(...)`.
        let warmup_url = format!(
            "{XLOGIN_URL}?appid={QQ_APPID}&daid=383&style=33&login_text=登录&hide_title_bar=1&hide_border=1&target=self&s_url={OAUTH_JUMP}&pt_3rd_aid={OAUTH_CLIENT_ID}&theme=2"
        );
        if let Ok(response) = self
            .login_http
            .send_once(
                &TransportRequest::get(&warmup_url)
                    .header("user-agent", LOGIN_UA)
                    .header("referer", LOGIN_REFERER),
            )
            .await
        {
            collect_cookies(&response.all("set-cookie"), &mut cookies);
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or_default();
        let show_url = format!(
            "{QR_SHOW_URL}?appid={QQ_APPID}&e=2&l=M&s=3&d=72&v=4&t={timestamp}&daid=383&pt_3rd_aid={OAUTH_CLIENT_ID}&u1={OAUTH_JUMP}"
        );
        let mut request = TransportRequest::get(&show_url)
            .header("user-agent", LOGIN_UA)
            .header("referer", LOGIN_REFERER);
        if let Some(cookie) = cookie_header(&cookies) {
            request = request.header("cookie", cookie);
        }
        let response = self
            .login_http
            .send_once(&request)
            .await
            .map_err(|e| perr(format!("QR image request failed: {e}")))?;
        collect_cookies(&response.all("set-cookie"), &mut cookies);
        // The body was buffered by the transport, so today's separate
        // "QR image download failed" text is unreachable (§5.5 row 4): a
        // mid-body reset surfaces as a send error above.
        let png = response.body;
        let qrsig = cookies
            .get("qrsig")
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| perr("QR login did not return a qrsig cookie"))?
            .to_owned();
        cookies.remove("qrsig");
        self.store_qr_pending(&cookies)?;
        // The PNG bytes are already in hand from ptqrshow, so `image_png` is
        // filled with the same base64 the `url` data URI carries — one
        // encode, two slots (M3 QR provider slice,
        // MODEL_ALIGNMENT_DESIGN.md §6 landed deltas).
        let png_b64 = STANDARD.encode(&png);
        Ok(QrLoginSession {
            key: qrsig,
            url: format!("data:image/png;base64,{png_b64}"),
            expires_in: 180,
            image_png: Some(png_b64),
            kind: QrLoginKind::Standard,
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
        let mut request = TransportRequest::get(&url)
            .header("user-agent", LOGIN_UA)
            .header("referer", LOGIN_REFERER);
        if let Some(cookie) = cookie_header(&cookies) {
            request = request.header("cookie", cookie);
        }
        let response = self
            .login_http
            .send_once(&request)
            .await
            .map_err(|e| perr(format!("QR poll request failed: {e}")))?;
        let body = response.text().into_owned();
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
    /// Manual hop-by-hop redirect walk on the redirect-none login profile
    /// (`MAX_REDIRECTS` hops), harvesting each hop's Set-Cookie headers.
    /// Kept in the provider as host failover (TRANSPORT_DESIGN.md §1). Each
    /// hop is one `send_once` attempt: the walk depends on the login profile
    /// NOT following redirects, so a 302 comes back as a normal Ok response
    /// whose Set-Cookie/Location the loop resolves itself — `send_raw` would
    /// collapse it into `Err(Status("HTTP 302"))` and drop the harvest
    /// (§4.1).
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
            let mut request = TransportRequest::get(&location)
                .header("user-agent", LOGIN_UA)
                .header("referer", LOGIN_REFERER);
            if let Some(cookie) = cookie_header(&collected) {
                request = request.header("cookie", cookie);
            }
            let Ok(response) = self.login_http.send_once(&request).await else {
                break;
            };
            collect_cookies(&response.all("set-cookie"), &mut collected);
            // Lossy-mangled Location values are skipped by `header()`, the
            // same drop semantics as today's `to_str().ok()` (§5.5 row 9).
            let next = response.header("location").unwrap_or_default().to_owned();
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
        let Some((playlist, songs)) = diss_detail(&value["req"], disstid) else {
            return Err(perr("playlist detail request failed"));
        };
        let complete = songs.len() < DETAIL_PAGE_SIZE as usize;
        Ok((playlist, songs, complete))
    }
}
/// Playlist-detail body: triple gate (`req.code`, `req.data.code`), songs at
/// `data.songlist`, meta from `dirinfo.title|dissname` / `desc` /
/// `dirinfo.picurl` / `total_song_num|dirinfo.songnum`.
fn diss_detail(req: &Value, disstid: u64) -> Option<(Playlist, Vec<Track>)> {
    if req["code"].as_i64().unwrap_or(-1) != 0 || req["data"]["code"].as_i64().unwrap_or(-1) != 0 {
        return None;
    }
    let data = &req["data"];
    let songs = data["songlist"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_song)
        .collect::<Vec<_>>();
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
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
    };
    Some((playlist, songs))
}
/// The login-cookie gate shared by `QqMusicProvider::auth_state` and the
/// legacy checks: an identity cookie (uin/wxuin) and a key cookie
/// (qqmusic_key/qm_keyst) must both be present.
fn has_login_cookies(session: &ProviderSession) -> bool {
    IDENTITY_COOKIES
        .iter()
        .any(|name| session.cookies.contains_key(*name))
        && KEY_COOKIES
            .iter()
            .any(|name| session.cookies.contains_key(*name))
}
#[async_trait]
impl AuthenticationProvider for QqMusicProvider {
    /// Migrated onto the unified [`AuthState`] machine (§8 slice 2 wiring):
    /// the state comes from `QqMusicProvider::auth_state` and the legacy
    /// three-state shape is its documented fold
    /// ([`AuthState::to_login_status`]). The account identity keeps today's
    /// independent user_id/display_name plumbing in the `Authenticated` arm
    /// (QQ and WeChat login write both together, but a partial profile still
    /// reports the field it has, exactly like the pre-migration code).
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, MockTransport};
    use std::sync::Arc;
    /// Loads a reconstructed real-response fixture from tests/fixtures/qqmusic
    /// (see the README there for the derivation of these shapes).
    macro_rules! fixture {
        ($name:expr) => {
            serde_json::from_str::<Value>(include_str!(concat!(
                "../tests/fixtures/qqmusic/",
                $name
            )))
            .expect("qqmusic fixture must be valid JSON")
        };
    }

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
        provider.mark_session_expired(None);
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

    // --- fixture tests: reconstructed real-response shapes, see
    // tests/fixtures/qqmusic/README.md for the derivation of each file.

    #[test]
    fn search_fixture_parses_items_total_and_has_more() {
        let value = fixture!("search_songs.json");
        assert_eq!(value["code"], 0);
        let song = &value["data"]["song"];
        assert_eq!(song["totalnum"].as_u64(), Some(523));
        let items: Vec<_> = song["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_song)
            .collect();
        // The empty-mid and blank-title entries are dropped.
        assert_eq!(items.len(), 2);
        let first = &items[0];
        assert_eq!(first.id.id, "003aAYrm3GE0Ac");
        assert_eq!(first.title, "晴天");
        assert_eq!(first.artists, vec!["周杰伦", "杨瑞代"]);
        assert_eq!(first.album.as_deref(), Some("叶惠美"));
        assert_eq!(first.album_id.as_deref(), Some("002Neh8l0uciQ1"));
        assert_eq!(first.duration.map(|d| d.as_secs()), Some(269));
        assert_eq!(
            first.artwork_url.as_deref(),
            Some("https://y.gtimg.cn/music/photo_new/T002R500x500M000002Neh8l0uciQ1.jpg")
        );
        assert_eq!(
            first.extra.get("songId").map(String::as_str),
            Some("483034200")
        );
        assert_eq!(
            first.extra.get("strMediaMid").map(String::as_str),
            Some("003aAYrm3GE0Ac")
        );
        assert_eq!(
            first.extra.get("size_128mp3").map(String::as_str),
            Some("4312832")
        );
        for quality in [
            StreamQuality::Low,
            StreamQuality::High,
            StreamQuality::Lossless,
            StreamQuality::HiRes,
        ] {
            assert!(first.available_qualities.contains(&quality));
        }
        // Second item: detail-style `name` field, no album object, empty
        // media_mid falls back to the song mid, only the 128k size is set.
        let second = &items[1];
        assert_eq!(second.title, "七里香");
        assert_eq!(second.album, None);
        assert_eq!(second.album_id, None);
        assert_eq!(second.artwork_url, None);
        assert_eq!(second.duration.map(|d| d.as_secs()), Some(284));
        assert_eq!(
            second.extra.get("strMediaMid").map(String::as_str),
            Some("001J5QJL0pQYmg")
        );
        assert_eq!(second.available_qualities, vec![StreamQuality::Low]);
        // has_more mirrors search(): page * limit < totalnum.
        assert!(search_has_more(1, 20, Some(523)));
        assert!(!search_has_more(27, 20, Some(523)));
        assert!(!search_has_more(1, 20, None));
    }

    #[test]
    fn song_detail_fixture_reads_the_name_field_fallback() {
        let value = fixture!("song_detail.json");
        assert_eq!(value["code"], 0);
        // fetch_song reads data[0]; detail items carry `name` instead of `title`.
        let track = value["data"]
            .as_array()
            .and_then(|data| data.first())
            .and_then(parse_song)
            .expect("detail song");
        assert_eq!(track.id.id, "003aAYrm3GE0Ac");
        assert_eq!(track.title, "晴天");
        assert_eq!(track.artists, vec!["周杰伦"]);
        assert_eq!(track.album.as_deref(), Some("叶惠美"));
        assert_eq!(track.duration.map(|d| d.as_secs()), Some(269));
        // size_flac is 0, so Lossless must not be offered.
        assert!(track.available_qualities.contains(&StreamQuality::Low));
        assert!(track.available_qualities.contains(&StreamQuality::High));
        assert!(!track.available_qualities.contains(&StreamQuality::Lossless));
    }

    #[test]
    fn stream_fixture_matches_entries_by_filename_and_skips_empty_ones() {
        let value = fixture!("stream_vkey_ok.json");
        assert_eq!(value["req_0"]["code"], 0);
        let candidates: Vec<StreamCandidate> = tier(StreamQuality::HiRes)
            .iter()
            .map(|(prefix, quality, format)| {
                (
                    format!("{prefix}003aAYrm3GE0Ac.{}", extension(*format)),
                    *quality,
                    *format,
                    size_key(prefix),
                )
            })
            .collect();
        let (candidate, sip, purl, vkey) = pick_stream(&value, &candidates).unwrap();
        // The AI00 entry (empty purl) and the M500 entry (empty vkey) are
        // skipped; the first fully valid requested filename wins.
        assert_eq!(candidate.0, "M800003aAYrm3GE0Ac.mp3");
        assert_eq!(candidate.1, StreamQuality::High);
        assert_eq!(candidate.2, AudioFormat::Mp3);
        assert_eq!(sip, "http://dl.stream.qqmusic.qq.com/");
        assert_eq!(purl, "M800003aAYrm3GE0Ac.mp3?extra=1");
        assert_eq!(vkey, "v_800");
        // URL assembly mirrors resolve(): STREAM_QUERY with guid/vkey filled.
        let query = STREAM_QUERY
            .replace("{guid}", "4242")
            .replace("{vkey}", &vkey);
        assert_eq!(query, "guid=4242&vkey=v_800&uin=0&fromtag=120093");
        assert_eq!(
            format!("{sip}{purl}?{query}"),
            "http://dl.stream.qqmusic.qq.com/M800003aAYrm3GE0Ac.mp3?extra=1?guid=4242&vkey=v_800&uin=0&fromtag=120093"
        );
    }

    #[test]
    fn stream_fixture_empty_sip_means_not_playable() {
        let value = fixture!("stream_vkey_empty_sip.json");
        let candidates: Vec<StreamCandidate> = tier(StreamQuality::Low)
            .iter()
            .map(|(prefix, quality, format)| {
                (
                    format!("{prefix}003aAYrm3GE0Ac.{}", extension(*format)),
                    *quality,
                    *format,
                    size_key(prefix),
                )
            })
            .collect();
        // Empty sip is how QQ signals "not playable anonymously" (risk
        // control), even with a fully valid midurlinfo entry: resolve()
        // turns this into NotFound.
        assert!(pick_stream(&value, &candidates).is_none());
    }

    #[test]
    fn lyric_fixture_decodes_plain_base64_lrc_fields() {
        let value = fixture!("lyric_plain_lrc.json");
        let doc = parse_lyric_response(&value).expect("lyric document");
        assert_eq!(doc.format, LyricFormat::Lrc);
        assert_eq!(
            doc.original,
            "[00:12.50]晴天\n[00:17.30]故事的小黄花\n[00:21.90]从出生那年就飘着"
        );
        assert_eq!(doc.lines.len(), 3);
        assert_eq!(doc.lines[0].start_ms, 12_500);
        assert_eq!(doc.lines[2].text, "从出生那年就飘着");
        assert_eq!(doc.translation.as_deref(), Some("[00:12.50]Sunny Day"));
        assert_eq!(doc.romanization.as_deref(), Some("[00:12.50]qing tian"));
        // Module code != 0 means "no lyrics", not an API failure.
        assert!(parse_lyric_response(&json!({"req": {"code": 2000}})).is_none());
        // A missing/undecodable lyric field yields no document either.
        assert!(parse_lyric_response(&json!({"req": {"code": 0, "data": {}}})).is_none());
    }

    #[test]
    fn playlist_detail_fixture_gates_and_maps_dirinfo() {
        let value = fixture!("playlist_detail.json");
        assert_eq!(value["code"], 0);
        let (playlist, songs) = diss_detail(&value["req"], 7_242_345_568).expect("playlist detail");
        assert_eq!(playlist.id.id, "7242345568");
        assert_eq!(playlist.title, "经典华语精选");
        assert_eq!(playlist.description.as_deref(), Some("样例歌单简介"));
        // dirinfo.picurl is kept as-is on this path (no http-to-https upgrade).
        assert_eq!(
            playlist.artwork_url.as_deref(),
            Some("http://img.example/diss.jpg")
        );
        assert_eq!(playlist.track_count, Some(3));
        assert_eq!(songs.len(), 2);
        assert_eq!(songs[0].id.id, "003aAYrm3GE0Ac");
        assert_eq!(songs[0].title, "晴天");
        assert!(
            songs[0]
                .available_qualities
                .contains(&StreamQuality::Lossless)
        );
        assert_eq!(songs[1].id.id, "000Qg2SD0ZqYFn");
        // A non-zero req.code or req.data.code fails the triple gate.
        assert!(diss_detail(&json!({"code": 5001}), 1).is_none());
        assert!(diss_detail(&json!({"code": 0, "data": {"code": -1}}), 1).is_none());
    }

    #[test]
    fn playlist_detail_fixture_falls_back_to_disstid_title() {
        let value = fixture!("playlist_detail_minimal.json");
        let (playlist, songs) = diss_detail(&value["req"], 7_242_345_568).expect("detail");
        // No dirinfo and no dissname: the title falls back to the disstid.
        assert_eq!(playlist.title, "QQ 歌单 7242345568");
        assert_eq!(playlist.description, None);
        assert_eq!(playlist.artwork_url, None);
        // 1981-song playlists have been observed live; the count rides on
        // total_song_num even when the page itself is empty.
        assert_eq!(playlist.track_count, Some(1981));
        assert!(songs.is_empty());
    }

    #[test]
    fn playlist_by_tag_fixture_parses_numeric_and_string_ids() {
        let value = fixture!("playlist_by_tag.json");
        assert_eq!(value["code"], 0);
        let items: Vec<_> = value["data"]["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_diss_by_tag_item)
            .collect();
        // The blank-name entry is dropped.
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id.id, "6800944944");
        assert_eq!(items[0].title, "劲爆嗨曲中文dj舞曲");
        assert_eq!(
            items[0].artwork_url.as_deref(),
            Some("https://img.example/logo.jpg")
        );
        assert_eq!(items[0].track_count, Some(71));
        assert_eq!(items[0].description.as_deref(), Some("简介"));
        // String dissid is tolerated; song_count falls back to song_num.
        assert_eq!(items[1].id.id, "2654954555");
        assert_eq!(items[1].track_count, Some(45));
        assert_eq!(items[1].artwork_url, None);
        assert_eq!(items[1].description, None);
    }

    #[test]
    fn playlist_categories_fixture_skips_sentinel_and_unusable() {
        let value = fixture!("playlist_categories.json");
        assert_eq!(value["code"], 0);
        let cats = parse_categories(&value);
        assert_eq!(cats.len(), 3);
        assert_eq!(cats[0].id, "10000000");
        assert_eq!(cats[0].name, "全部");
        assert_eq!(cats[0].group.as_deref(), Some("全部"));
        assert_eq!(cats[1].name, "华语");
        assert_eq!(cats[1].group.as_deref(), Some("语种"));
        // categoryId 0 and usable == 0 entries are skipped.
        assert_eq!(cats[2].name, "摇滚");
        assert_eq!(cats[2].group.as_deref(), Some("风格"));
    }

    #[test]
    fn user_playlists_fixture_merges_created_and_collected_shapes() {
        let created = fixture!("user_created_playlists.json");
        assert_eq!(created["code"], 0);
        let collected = fixture!("user_collected_playlists.json");
        assert_eq!(collected["code"], 0);
        let merged = merge_user_playlists(&created, &collected);
        // Created: dirid 201 (favorites) and tid 0 (system dirs) dropped.
        // Collected: the duplicate of a created id and the blank-name entry
        // dropped.
        let ids: Vec<_> = merged.iter().map(|p| p.id.id.clone()).collect();
        assert_eq!(ids, vec!["6800944944", "7758023114", "6800944945"]);
        assert_eq!(merged[0].title, "自建歌单");
        assert_eq!(merged[0].track_count, Some(5));
        assert_eq!(
            merged[0].artwork_url.as_deref(),
            Some("https://img.example/self.jpg")
        );
        assert_eq!(merged[2].title, "劲爆嗨曲中文dj舞曲");
        assert_eq!(merged[2].track_count, Some(71));
    }

    #[test]
    fn album_fixture_derives_metadata_and_drops_bad_songs() {
        let value = fixture!("album_songs.json");
        assert_eq!(value["code"], 0);
        let data = &value["albumSonglist"]["data"];
        assert_eq!(value["albumSonglist"]["code"], 0);
        let album = album_from_songlist(data, "002Neh8l0uciQ1").expect("album");
        assert_eq!(album.id.id, "002Neh8l0uciQ1");
        assert_eq!(album.title, "叶惠美");
        assert_eq!(album.artists, vec!["周杰伦"]);
        assert_eq!(album.track_count, Some(3));
        assert_eq!(
            album.artwork_url.as_deref(),
            Some("https://y.gtimg.cn/music/photo_new/T002R500x500M000002Neh8l0uciQ1.jpg")
        );
        let songs: Vec<_> = data["songList"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| parse_song(&item["songInfo"]))
            .collect();
        // The empty-mid songInfo is dropped.
        assert_eq!(songs.len(), 2);
        assert_eq!(songs[0].title, "晴天");
        assert!(
            songs[0]
                .available_qualities
                .contains(&StreamQuality::Lossless)
        );
        assert_eq!(songs[1].artists, vec!["周杰伦", "方文山"]);
        assert_eq!(songs[1].available_qualities, vec![StreamQuality::Low]);
    }

    #[test]
    fn charts_fixture_filters_id_201_and_strips_prefixes() {
        let value = fixture!("charts_toplist.json");
        assert_eq!(value["code"], 0);
        let boards: Vec<_> = value["data"]["topList"]
            .as_array()
            .into_iter()
            .flatten()
            // charts() filters board id 201 before parsing.
            .filter(|board| board["id"].as_i64() != Some(201))
            .filter_map(parse_toplist_board)
            .collect();
        // The blank-topTitle board is dropped by the parser itself.
        assert_eq!(boards.len(), 2);
        assert_eq!(boards[0].id.id, "26");
        assert_eq!(boards[0].title, "热歌榜");
        assert_eq!(boards[1].id.id, "57");
        assert_eq!(boards[1].title, "内地榜");
    }

    #[test]
    fn chart_tracks_fixture_ranks_after_dropping_bad_entries() {
        let value = fixture!("chart_tracks.json");
        assert_eq!(value["code"], 0);
        assert_eq!(value["toplist"]["code"], 0);
        let entries = chart_song_entries(&value);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].rank, 1);
        assert_eq!(entries[0].track.id.id, "003aAYrm3GE0Ac");
        assert_eq!(entries[1].rank, 2);
        assert_eq!(entries[1].track.id.id, "000Qg2SD0ZqYFn");
    }

    // --- unified transport migration tests (mock transport, no network):
    // the provider runs against two `Http::from_shared_mock` handles (the
    // business and the redirect-none login face) and the recorded request
    // log pins the migrated request faces (TRANSPORT_DESIGN.md §7.2).

    fn mock_json(status: u16, body: &str) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: body.as_bytes().to_vec(),
        }
    }

    fn mock_response(status: u16, headers: &[(&str, &str)], body: &str) -> TransportResponse {
        TransportResponse {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            url: None,
            body: body.as_bytes().to_vec(),
        }
    }

    /// A mock that fails the test the moment the wrong face is touched.
    fn forbidden(label: &'static str) -> Http {
        Http::from_mock(MockTransport::new(move |request| {
            panic!("{label} transport must not see traffic: {}", request.url);
        }))
    }

    fn sample_track() -> Track {
        Track {
            id: TrackId::new(ID, "003aAYrm3GE0Ac"),
            title: "晴天".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![StreamQuality::Low],
            audio: None,
            extra: BTreeMap::new(),
        }
    }

    fn logged_in_provider(http: Http, login_http: Http) -> QqMusicProvider {
        let provider = QqMusicProvider::with_transports(http, login_http);
        let mut cookies = BTreeMap::new();
        cookies.insert("uin".into(), "12345".into());
        cookies.insert("qqmusic_key".into(), "k1".into());
        provider.set_cookies(cookies, None, None).unwrap();
        provider
    }

    #[tokio::test]
    async fn search_gets_through_send_raw_with_headers_and_cookies() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert_eq!(request.method, HttpMethod::Get);
            assert!(request.idempotent);
            assert!(
                request
                    .url
                    .starts_with("https://c.y.qq.com/soso/fcgi-bin/client_search_cp?")
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "referer" && v == REFERER)
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "user-agent" && v == UA_WEB)
            );
            Ok(mock_json(
                200,
                r#"{"code":0,"data":{"song":{"totalnum":1,"list":[
                    {"mid":"003aAYrm3GE0Ac","title":"晴天","singer":[{"name":"周杰伦"}],
                     "file":{"size_128mp3":100}}]}}}"#,
            ))
        }));
        let provider = logged_in_provider(Http::from_shared_mock(mock.clone()), forbidden("login"));
        let page = provider.search(&SearchQuery::new("晴天")).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id.id, "003aAYrm3GE0Ac");
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        // Every session cookie rides the request (no whitelist for QQ).
        let cookie = recorded[0]
            .headers
            .iter()
            .find(|(k, _)| k == "cookie")
            .map(|(_, v)| v.as_str())
            .expect("session cookies must be sent");
        assert!(cookie.contains("uin=12345"), "cookie: {cookie}");
        assert!(cookie.contains("qqmusic_key=k1"), "cookie: {cookie}");
    }

    #[tokio::test]
    async fn raw_get_retries_transport_error_then_succeeds() {
        let mock = Arc::new(MockTransport::script([
            Err(TransportError::Transport("connection reset".into())),
            Ok(mock_json(200, r#"{"code":0,"data":{"song":{"list":[]}}}"#)),
        ]));
        let provider = QqMusicProvider::with_transports(
            Http::from_shared_mock(mock.clone()),
            forbidden("login"),
        );
        let page = provider.search(&SearchQuery::new("x")).await.unwrap();
        assert!(page.items.is_empty());
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2, "one transport retry then success");
        for request in &recorded {
            assert_eq!(request.method, HttpMethod::Get);
            assert!(request.idempotent);
        }
    }

    #[tokio::test]
    async fn raw_get_fatal_status_keeps_the_classify_http_text() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(404, "missing"))]));
        let provider = QqMusicProvider::with_transports(
            Http::from_shared_mock(mock.clone()),
            forbidden("login"),
        );
        let error = provider.search(&SearchQuery::new("x")).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider qqmusic failed: search: HTTP 404"
        );
        assert_eq!(mock.recorded().len(), 1, "fatal statuses never retry");
    }

    #[tokio::test]
    async fn get_jsonp_strips_the_callback_shell_on_the_buffered_body() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"callback({"code":0,"data":{"categories":[]}});"#,
        ))]));
        let provider = QqMusicProvider::with_transports(
            Http::from_shared_mock(mock.clone()),
            forbidden("login"),
        );
        let value = provider
            .get_jsonp("https://c.y.qq.com/test.fcg", "label")
            .await
            .unwrap();
        assert_eq!(value["code"], 0);
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn resolve_posts_idempotent_musicu_body_through_send_api_json() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert_eq!(request.method, HttpMethod::Post);
            assert!(
                request.idempotent,
                "the musicu POST declares its body resend (.idempotent(true))"
            );
            assert_eq!(request.url, MUSICU_URL);
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "content-type" && v == "application/json")
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "user-agent" && v == UA_WEB)
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "referer" && v == REFERER)
            );
            let body = std::str::from_utf8(request.body.as_deref().unwrap_or_default())
                .unwrap_or_default();
            assert!(
                body.contains("vkey.GetVkeyServer"),
                "vkey body must ride the POST: {body}"
            );
            Ok(mock_json(
                200,
                r#"{"code":0,"req_0":{"code":0,"data":{
                    "sip":["http://dl.stream.qqmusic.qq.com/"],
                    "midurlinfo":[{"filename":"M500003aAYrm3GE0Ac.mp3",
                                   "purl":"M500003aAYrm3GE0Ac.mp3?extra=1","vkey":"v_500"}]}}}"#,
            ))
        }));
        let provider = logged_in_provider(Http::from_shared_mock(mock.clone()), forbidden("login"));
        let info = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .unwrap();
        assert!(
            info.url.starts_with(
                "http://dl.stream.qqmusic.qq.com/M500003aAYrm3GE0Ac.mp3?extra=1?guid="
            ),
            "url: {}",
            info.url
        );
        assert!(
            info.url.contains("vkey=v_500&uin=0&fromtag=120093"),
            "url: {}",
            info.url
        );
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn signed_lyrics_retry_carries_zzc_sign_and_android_ua() {
        let lrc = "[00:12.50]晴天";
        let lyric_b64 = STANDARD.encode(lrc);
        let mock = Arc::new(MockTransport::script([
            Err(TransportError::Transport("connection reset".into())),
            Ok(mock_json(
                200,
                &format!(r#"{{"code":0,"req":{{"code":0,"data":{{"lyric":"{lyric_b64}"}}}}}}"#),
            )),
        ]));
        let provider = QqMusicProvider::with_transports(
            Http::from_shared_mock(mock.clone()),
            forbidden("login"),
        );
        let mut track = sample_track();
        track.extra.insert("songId".into(), "483034200".into());
        let doc = provider.lyrics(&track).await.unwrap().unwrap();
        assert_eq!(doc.original, lrc);
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2, "one transport retry then success");
        for request in &recorded {
            assert_eq!(request.method, HttpMethod::Post);
            assert!(request.idempotent);
            assert!(
                request.url.starts_with(&format!("{MUSICU_URL}?sign=zzc")),
                "lyrics is the only signed call: {}",
                request.url
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "user-agent" && *v == UA_ANDROID),
                "signed lyrics carry the android UA"
            );
        }
    }

    #[tokio::test]
    async fn musicu_session_expired_marks_the_session() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":1000,"message":"not login"}"#,
        ))]));
        let provider = logged_in_provider(Http::from_shared_mock(mock.clone()), forbidden("login"));
        let error = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .unwrap_err();
        // Pinned text updated for the M2 migration (AUTH_REGISTRY_DESIGN.md
        // §1.2/§8 slice 2): `Error::SessionExpired` keeps the exact "login
        // required or session expired" body and drops the "provider qqmusic
        // failed:" prefix; the call-site label survives in the detail slot.
        assert_eq!(
            error.to_string(),
            "login required or session expired: stream resolve"
        );
        assert_eq!(error.category(), crate::ErrorCategory::SessionExpired);
        assert_eq!(error.provider_id(), Some("qqmusic"));
        assert!(
            !error.is_retryable(),
            "expiry recovers by re-login, not resend"
        );
        assert_eq!(error.detail(), Some("stream resolve"));
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn wechat_poll_carries_timeout_and_exchange_walks_failover_hosts_once() {
        let mock = Arc::new(MockTransport::new(|request| {
            if request.url.starts_with(WX_POLL_URL) {
                assert_eq!(request.method, HttpMethod::Get);
                assert_eq!(
                    request.timeout,
                    Some(WX_POLL_TIMEOUT),
                    "the long-poll must carry the 35s per-request timeout override"
                );
                return Ok(mock_response(
                    200,
                    &[],
                    "window.wx_errcode=405;window.wx_code='abc123';",
                ));
            }
            if request.url == MUSICU_URL {
                return Err(TransportError::Transport("primary down".into()));
            }
            if request.url == WX_MUSICU_FALLBACKS[0] {
                return Ok(mock_response(200, &[], "this is not json"));
            }
            if request.url == WX_MUSICU_FALLBACKS[1] {
                return Ok(mock_json(
                    200,
                    r#"{"code":0,"req":{"code":0,"data":{
                        "musickey":"W_X_key","str_musicid":"123456","openid":"o1"}}}"#,
                ));
            }
            panic!("unexpected WeChat url: {}", request.url);
        }));
        let provider = QqMusicProvider::with_transports(
            Http::from_shared_mock(mock.clone()),
            forbidden("login"),
        );
        let poll = provider.poll_qr_login("wx:061AbC-dEf").await.unwrap();
        assert_eq!(poll.status, QrLoginStatus::Success);
        assert_eq!(poll.user_id.as_deref(), Some("123456"));
        let recorded = mock.recorded();
        assert_eq!(
            recorded.len(),
            4,
            "one poll GET + three failover hosts, exactly one send each"
        );
        assert!(recorded[0].url.starts_with(WX_POLL_URL));
        assert_eq!(
            recorded[1].url, MUSICU_URL,
            "primary host first, then the fallback chain, in order"
        );
        assert_eq!(recorded[2].url, WX_MUSICU_FALLBACKS[0]);
        assert_eq!(recorded[3].url, WX_MUSICU_FALLBACKS[1]);
        for request in &recorded[1..] {
            assert_eq!(request.method, HttpMethod::Post);
            assert!(
                !request.idempotent,
                "exchange POSTs stay method-default single-shot; host rotation is the only retry"
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "cookie" && v == "login_type=2")
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "origin" && v == "https://y.qq.com")
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "content-type" && v == "application/x-www-form-urlencoded")
            );
        }
        // The exchange result landed as the WeChat session cookies.
        let session = provider.session_store().get(ID).unwrap();
        assert_eq!(
            session.cookies.get("qm_keyst").map(String::as_str),
            Some("W_X_key")
        );
        assert_eq!(session.user_id.as_deref(), Some("123456"));
    }

    #[tokio::test]
    async fn qq_qr_login_walks_redirect_hops_and_harvests_cookies() {
        let mock = Arc::new(MockTransport::new(|request| {
            let url = request.url.as_str();
            if url.starts_with(QR_CHECK_URL) {
                return Ok(mock_response(
                    200,
                    &[],
                    "ptuiCB('0','0','https://ssl.ptlogin2.qq.com/check_sig?aid=x','0','登录成功！','昵称');",
                ));
            }
            if url.starts_with("https://ssl.ptlogin2.qq.com/check_sig") {
                // Hop 1: drops the identity cookie, absolute Location.
                return Ok(mock_response(
                    302,
                    &[
                        ("set-cookie", "uin=12345; Path=/; Domain=.qq.com"),
                        ("location", "https://graph.qq.com/oauth2.0/authorize_jump"),
                    ],
                    "",
                ));
            }
            if url == "https://graph.qq.com/oauth2.0/authorize_jump" {
                // Hop 2: drops the music key, relative Location resolved
                // against this hop's host.
                return Ok(mock_response(
                    302,
                    &[
                        ("set-cookie", "qm_keyst=W_X_key; Path=/"),
                        ("location", "/hop3"),
                    ],
                    "",
                ));
            }
            if url == "https://graph.qq.com/hop3" {
                // Final hop: 200 without Location ends the walk.
                return Ok(mock_response(200, &[], "done"));
            }
            panic!("unexpected login url: {url}");
        }));
        let provider = QqMusicProvider::with_transports(
            forbidden("business"),
            Http::from_shared_mock(mock.clone()),
        );
        provider
            .store_qr_pending(&BTreeMap::from([("pt_login_sig".into(), "sig1".into())]))
            .unwrap();
        let poll = provider.poll_qr_login("QRSIG").await.unwrap();
        assert_eq!(poll.status, QrLoginStatus::Success);
        assert_eq!(poll.user_id.as_deref(), Some("12345"));
        assert_eq!(poll.display_name.as_deref(), Some("昵称"));
        let recorded = mock.recorded();
        assert_eq!(
            recorded.len(),
            4,
            "one check GET + three manual hops, one send each"
        );
        for request in &recorded {
            assert_eq!(request.method, HttpMethod::Get);
        }
        fn cookie_of(request: &TransportRequest) -> &str {
            request
                .headers
                .iter()
                .find(|(k, _)| k == "cookie")
                .map(|(_, v)| v.as_str())
                .unwrap_or_default()
        }
        // The check carries the pending warmup cookie plus the qrsig.
        let check_cookie = cookie_of(&recorded[0]);
        assert!(check_cookie.contains("qrsig=QRSIG"), "{check_cookie}");
        assert!(check_cookie.contains("pt_login_sig=sig1"), "{check_cookie}");
        // Each hop accumulates the cookies harvested on the way.
        let hop1 = cookie_of(&recorded[1]);
        assert!(
            hop1.contains("qrsig=QRSIG") && !hop1.contains("uin="),
            "{hop1}"
        );
        let hop2 = cookie_of(&recorded[2]);
        assert!(hop2.contains("uin=12345"), "{hop2}");
        let hop3 = cookie_of(&recorded[3]);
        assert!(hop3.contains("qm_keyst=W_X_key"), "{hop3}");
        // The harvested cookie set replaced the session and cleared the
        // pending token.
        let session = provider.session_store().get(ID).unwrap();
        assert_eq!(
            session.cookies.get("uin").map(String::as_str),
            Some("12345")
        );
        assert_eq!(
            session.cookies.get("qm_keyst").map(String::as_str),
            Some("W_X_key")
        );
        assert!(!session.tokens.contains_key(QR_PENDING_KEY));
        assert!(matches!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedIn { .. }
        ));
    }

    #[tokio::test]
    async fn create_qr_login_harvests_warmup_and_show_cookies() {
        let mock = Arc::new(MockTransport::new(|request| {
            if request.url.starts_with(XLOGIN_URL) {
                return Ok(mock_response(
                    200,
                    &[("set-cookie", "pt_login_sig=WARM; Path=/")],
                    "warmup page",
                ));
            }
            if request.url.starts_with(QR_SHOW_URL) {
                // The show request must carry the warmup cookie.
                assert!(
                    request
                        .headers
                        .iter()
                        .any(|(k, v)| k == "cookie" && v == "pt_login_sig=WARM")
                );
                return Ok(mock_response(
                    200,
                    &[("set-cookie", "qrsig=SIG; Path=/")],
                    "PNGBYTES",
                ));
            }
            panic!("unexpected login url: {}", request.url);
        }));
        let provider = QqMusicProvider::with_transports(
            forbidden("business"),
            Http::from_shared_mock(mock.clone()),
        );
        let session = provider.create_qr_login().await.unwrap();
        assert_eq!(session.key, "SIG");
        assert!(session.url.starts_with("data:image/png;base64,"));
        // The PNG bytes are in hand at the create site, so image_png carries
        // the same base64 payload as the `url` data URI (M3 QR provider
        // slice, MODEL_ALIGNMENT_DESIGN.md §6).
        assert_eq!(session.image_png.as_deref(), Some("UE5HQllURVM="));
        assert_eq!(
            session.url,
            format!(
                "data:image/png;base64,{}",
                session.image_png.as_deref().unwrap()
            ),
            "url and image_png carry the same base64 payload"
        );
        assert_eq!(session.kind, QrLoginKind::Standard);
        assert_eq!(session.expires_in, 180);
        // qrsig rides as the poll key; the rest is parked for the poll.
        let pending = provider.qr_pending();
        assert_eq!(
            pending.get("pt_login_sig").map(String::as_str),
            Some("WARM")
        );
        assert!(!pending.contains_key("qrsig"));
        assert_eq!(mock.recorded().len(), 2);
    }

    #[tokio::test]
    async fn create_qr_login_ignores_warmup_transport_errors_like_today() {
        // Warmup fails (deterministic Build sentinel, so no retry), the show
        // request still proceeds and returns the QR session.
        let mock = Arc::new(MockTransport::script([
            Err(TransportError::Build("warmup down".into())),
            Ok(mock_response(
                200,
                &[("set-cookie", "qrsig=SIG; Path=/")],
                "PNGBYTES",
            )),
        ]));
        let provider = QqMusicProvider::with_transports(
            forbidden("business"),
            Http::from_shared_mock(mock.clone()),
        );
        let session = provider.create_qr_login().await.unwrap();
        assert_eq!(session.key, "SIG");
        assert_eq!(session.image_png.as_deref(), Some("UE5HQllURVM="));
        assert_eq!(mock.recorded().len(), 2);
    }

    #[tokio::test]
    async fn create_wechat_login_fills_image_png_from_the_jpeg_bytes() {
        // WeChat channel: the qrconnect page carries the uuid, the qrcode
        // endpoint serves the JPEG bytes, and both slots are filled from
        // that single in-hand payload (M3 QR provider slice,
        // MODEL_ALIGNMENT_DESIGN.md §6).
        let mock = Arc::new(MockTransport::new(|request| {
            if request.url.starts_with(WX_QR_CONNECT_URL) {
                return Ok(mock_response(
                    200,
                    &[],
                    "<script>connect/l/qrconnect?uuid=WXUUID123</script>",
                ));
            }
            if request.url.starts_with(WX_QRCODE_URL) {
                assert_eq!(request.url, format!("{WX_QRCODE_URL}/WXUUID123"));
                return Ok(mock_response(200, &[], "JPEGBYTES"));
            }
            panic!("unexpected WeChat url: {}", request.url);
        }));
        let provider = QqMusicProvider::with_transports(
            Http::from_shared_mock(mock.clone()),
            forbidden("login"),
        );
        let session = provider.create_wechat_login().await.unwrap();
        assert_eq!(session.key, "wx:WXUUID123");
        assert_eq!(session.kind, QrLoginKind::WeChat);
        assert_eq!(session.expires_in, WX_EXPIRES_IN);
        assert_eq!(session.image_png.as_deref(), Some("SlBFR0JZVEVT"));
        assert_eq!(
            session.url,
            format!(
                "data:image/jpeg;base64,{}",
                session.image_png.as_deref().unwrap()
            ),
            "url and image_png carry the same base64 payload"
        );
        assert_eq!(mock.recorded().len(), 2);
    }

    // --- M2 error-migration tests (AUTH_REGISTRY_DESIGN.md §8 slice 2):
    // the prose error sites are upgraded to the structured categories.

    #[test]
    fn no_playable_stream_classifies_by_module_code() {
        // Module code 0 with the purl/vkey withheld: the VIP/cookie gate
        // (lx-music's official tx source comments this shape as "// vip").
        let vip = json!({"req_0": {"code": 0, "data": {
            "sip": ["http://dl.stream.qqmusic.qq.com/"],
            "midurlinfo": [{"filename": "M500abc.mp3", "purl": "", "vkey": ""}]
        }}});
        let error = no_playable_stream_error(&vip);
        assert_eq!(error.category(), crate::ErrorCategory::VipRequired);
        assert_eq!(error.provider_id(), Some("qqmusic"));
        assert!(!error.is_retryable());
        assert_eq!(
            error.detail(),
            Some("stream resolve: no playable stream URL returned (module code Some(0))")
        );
        assert!(error.to_string().starts_with("vip membership required: "));

        // Module code 0 with a usable entry but every sip host blank: the
        // risk-control signal (stream_vkey_empty_sip fixture shape).
        let risk = json!({"req_0": {"code": 0, "data": {
            "sip": [""],
            "midurlinfo": [{"filename": "M500abc.mp3", "purl": "x", "vkey": "v"}]
        }}});
        let error = no_playable_stream_error(&risk);
        assert_eq!(error.category(), crate::ErrorCategory::PermissionDenied);
        assert_eq!(error.provider_id(), Some("qqmusic"));
        assert!(!error.is_retryable());
        assert!(error.to_string().starts_with("permission denied: "));

        // A non-zero module code: the vkey module itself refused.
        let refused = json!({"req_0": {"code": 2001, "data": {}}});
        let error = no_playable_stream_error(&refused);
        assert_eq!(error.category(), crate::ErrorCategory::Unavailable);
        assert!(error.is_retryable(), "intermittent server-side refusal");
        assert!(error.to_string().starts_with("provider unavailable: "));

        // No module envelope at all: unclassifiable, stays Unavailable.
        let error = no_playable_stream_error(&json!({}));
        assert_eq!(error.category(), crate::ErrorCategory::Unavailable);
        assert!(
            error.detail().unwrap().contains("module code None"),
            "detail: {:?}",
            error.detail()
        );
        // No credential material enters the detail slot (§1.2).
        let lowered = error.to_string().to_lowercase();
        for marker in ["cookie", "token", "secret", "sign", "password"] {
            assert!(!lowered.contains(marker), "leaked {marker:?}");
        }
    }

    #[tokio::test]
    async fn resolve_vip_gated_stream_surfaces_vip_required() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert_eq!(request.url, MUSICU_URL);
            Ok(mock_json(
                200,
                r#"{"code":0,"req_0":{"code":0,"data":{
                    "sip":["http://dl.stream.qqmusic.qq.com/"],
                    "midurlinfo":[{"filename":"M500003aAYrm3GE0Ac.mp3",
                                   "purl":"","vkey":""}]}}}"#,
            ))
        }));
        let provider = QqMusicProvider::with_transports(
            Http::from_shared_mock(mock.clone()),
            forbidden("login"),
        );
        let error = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .unwrap_err();
        // The M2 upgrade of the source_qqmusic.rs:714-718 prose: VIP/cookie
        // gate → VipRequired, underlying envelope facts preserved in detail.
        assert_eq!(error.category(), crate::ErrorCategory::VipRequired);
        assert_eq!(error.provider_id(), Some("qqmusic"));
        assert!(!error.is_retryable());
        assert_eq!(
            error.detail(),
            Some("stream resolve: no playable stream URL returned (module code Some(0))")
        );
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn resolve_risk_controlled_stream_surfaces_permission_denied() {
        let mock = Arc::new(MockTransport::new(|_request| {
            Ok(mock_json(
                200,
                // The stream_vkey_empty_sip shape: usable entry, host list
                // withheld — the "not playable anonymously" signal.
                r#"{"code":0,"req_0":{"code":0,"data":{
                    "sip":[""],
                    "midurlinfo":[{"filename":"M500003aAYrm3GE0Ac.mp3",
                                   "purl":"M500003aAYrm3GE0Ac.mp3?extra=1","vkey":"v_500"}]}}}"#,
            ))
        }));
        let provider = QqMusicProvider::with_transports(
            Http::from_shared_mock(mock.clone()),
            forbidden("login"),
        );
        let error = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.category(), crate::ErrorCategory::PermissionDenied);
        assert_eq!(error.provider_id(), Some("qqmusic"));
        assert_eq!(
            error.detail(),
            Some("stream resolve: no playable stream URL returned (module code Some(0))")
        );
    }

    #[tokio::test]
    async fn resolve_module_failure_surfaces_unavailable() {
        let mock = Arc::new(MockTransport::new(|_request| {
            Ok(mock_json(
                200,
                r#"{"code":0,"req_0":{"code":2001,"data":{}}}"#,
            ))
        }));
        let provider = QqMusicProvider::with_transports(
            Http::from_shared_mock(mock.clone()),
            forbidden("login"),
        );
        let error = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .unwrap_err();
        // A non-zero module code is the vkey module refusing, not a content
        // property: Unavailable (retryable), module facts in the detail.
        assert_eq!(error.category(), crate::ErrorCategory::Unavailable);
        assert!(error.is_retryable());
        assert_eq!(
            error.detail(),
            Some("stream resolve: no playable stream URL returned (module code Some(2001))")
        );
    }

    #[tokio::test]
    async fn search_session_expired_marks_the_session_and_keeps_the_label() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":1000,"message":"not login"}"#,
        ))]));
        let provider = logged_in_provider(Http::from_shared_mock(mock.clone()), forbidden("login"));
        let error = provider
            .search(&SearchQuery::new("晴天"))
            .await
            .unwrap_err();
        // get_json path (raw_get + classify_api): the expired=[1000, 301]
        // verdict surfaces as the classified variant with the label in the
        // detail slot; the expiry marker moves the auth state.
        assert_eq!(
            error.to_string(),
            "login required or session expired: search"
        );
        assert_eq!(error.category(), crate::ErrorCategory::SessionExpired);
        assert_eq!(error.provider_id(), Some("qqmusic"));
        assert_eq!(error.detail(), Some("search"));
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn search_fatal_api_code_surfaces_invalid_response() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":55,"message":"quota exceeded"}"#,
        ))]));
        let provider = QqMusicProvider::with_transports(
            Http::from_shared_mock(mock.clone()),
            forbidden("login"),
        );
        let error = provider.search(&SearchQuery::new("x")).await.unwrap_err();
        // Pinned text for the M2 migration: the classifier's fatal text is
        // preserved verbatim and the typed code slot carries the platform
        // code (netease-mirror shape, §1.2/§8 slice 2).
        assert_eq!(
            error.to_string(),
            "invalid response (API code 55): search: API code 55: quota exceeded"
        );
        assert_eq!(error.category(), crate::ErrorCategory::InvalidResponse);
        assert_eq!(error.provider_id(), Some("qqmusic"));
        assert!(!error.is_retryable());
        assert_eq!(error.detail(), Some("search: API code 55: quota exceeded"));
        assert!(
            matches!(error, Error::InvalidResponse { code: Some(55), .. }),
            "the typed code slot carries the platform code"
        );
        assert_eq!(mock.recorded().len(), 1, "fatal codes never retry");
    }

    #[test]
    fn fatal_api_code_parses_the_classifier_text() {
        assert_eq!(fatal_api_code("API code 55: quota exceeded"), Some(55));
        assert_eq!(fatal_api_code("API code 2001: refused"), Some(2001));
        assert_eq!(fatal_api_code("HTTP 502"), None);
        assert_eq!(fatal_api_code("API code nope: x"), None);
        assert_eq!(fatal_api_code(""), None);
    }

    #[test]
    fn api_code_error_keeps_legacy_catch_all_for_unparsed_texts() {
        // The one realistic Api text without an "API code" prefix (the
        // engine's non-idempotent retry note) and any future classifier
        // texts keep the exact legacy `perr` shape.
        let error = api_code_error(
            "search",
            "retryable body verdict on a non-idempotent request; not resent",
        );
        assert_eq!(
            error.to_string(),
            "provider qqmusic failed: search: retryable body verdict on a non-idempotent request; not resent"
        );
        assert_eq!(error.category(), crate::ErrorCategory::Provider);
    }

    #[tokio::test]
    async fn auth_state_derivation_drives_login_status() {
        let provider = QqMusicProvider::new();
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );

        // A key cookie alone is not a session.
        let mut cookies = BTreeMap::new();
        cookies.insert("qqmusic_key".into(), "k1".into());
        provider.set_cookies(cookies, None, None).unwrap();
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );

        // Identity + key (QQ spelling): Authenticated, folded back through
        // the state machine with the identity fields kept.
        let mut cookies = BTreeMap::new();
        cookies.insert("uin".into(), "12345".into());
        cookies.insert("qqmusic_key".into(), "k1".into());
        provider
            .set_cookies(cookies, Some("12345".into()), Some("tester".into()))
            .unwrap();
        assert_eq!(provider.auth_state(), AuthState::Authenticated);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedIn {
                user_id: Some("12345".into()),
                display_name: Some("tester".into()),
            }
        );

        // WeChat spelling also counts as the login cookies.
        let mut cookies = BTreeMap::new();
        cookies.insert("wxuin".into(), "67890".into());
        cookies.insert("qm_keyst".into(), "k2".into());
        provider.set_cookies(cookies, None, None).unwrap();
        assert_eq!(provider.auth_state(), AuthState::Authenticated);
        assert!(matches!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedIn { .. }
        ));

        // The expiry marker moves the state to Expired; both views agree.
        let error = provider.mark_session_expired(Some(ErrorDetail::new("search")));
        assert_eq!(error.category(), crate::ErrorCategory::SessionExpired);
        assert_eq!(error.provider_id(), Some("qqmusic"));
        assert_eq!(provider.auth_state(), AuthState::Expired);
        assert_eq!(provider.login_status().await.unwrap(), LoginStatus::Expired);

        provider.logout().await.unwrap();
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
    }

    #[tokio::test]
    async fn wechat_poll_rejection_surfaces_authentication_failed() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert!(request.url.starts_with(WX_POLL_URL));
            Ok(mock_response(
                200,
                &[],
                "window.wx_errcode=403;window.wx_code='';",
            ))
        }));
        let provider = QqMusicProvider::with_transports(
            Http::from_shared_mock(mock.clone()),
            forbidden("login"),
        );
        let error = provider.poll_qr_login("wx:061AbC-dEf").await.unwrap_err();
        // M2 migration (§1.1/§8 slice 2): the WeChat QR rejection is a
        // credentials-rejected outcome — the classified error replaces the
        // plain Failed poll status, the errcode rides the typed slot and the
        // original user-facing message is preserved in the detail slot.
        assert_eq!(error.category(), crate::ErrorCategory::AuthenticationFailed);
        assert_eq!(error.provider_id(), Some("qqmusic"));
        assert!(!error.is_retryable());
        assert_eq!(error.detail(), Some("微信登录被拒绝"));
        assert_eq!(
            error.to_string(),
            "authentication failed (API code 403): 微信登录被拒绝"
        );
        assert!(
            matches!(
                error,
                Error::AuthenticationFailed {
                    code: Some(403),
                    ..
                }
            ),
            "the typed code slot carries the WeChat errcode"
        );
        // No session was written by the rejected attempt.
        assert!(provider.session_store().get(ID).is_none());
        assert_eq!(mock.recorded().len(), 1);
    }
}
