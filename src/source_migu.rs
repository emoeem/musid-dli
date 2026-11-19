//! Migu Music provider migrated from VoiceFox's `lx-source::mg`.
//! Protocol origin: lx-music `musicSdk/mg/`. Signed search, three-tier
//! anonymous stream resolution (resourceinfo metadata, H5 strategy endpoint
//! with a magic-prefixed obfuscated response, listenSong 302 fallback) and
//! MRC word-timed lyrics (TEA-decrypted containers) are implemented. Migu
//! anonymously serves only PQ (128k) audio in practice: the resolved quality
//! is corrected against the actually served file size, never the metadata
//! claim. Albums ride the anonymous queryAlbumSong track list (no
//! album-detail endpoint upstream: metadata is derived from the first track
//! plus totalCount). Login and artists remain pending. There is no session
//! state to keep either: upstream `mg` has no login flow at all — the
//! `sid=USS` search parameter (mg/search.rs:21) and the search device id are
//! fixed literals, not stored sessions — so unlike the other providers this
//! struct carries no `SessionStore`; the auth-session store has no migu
//! consumer (AUTH_REGISTRY_DESIGN.md §9.5).
use crate::{
    Error, ErrorDetail, Result,
    lrc::parse_lrc,
    models::{
        Album, AudioFormat, Chart, ChartEntry, FallbackEndpoint, LyricDocument, LyricFormat,
        LyricLine, LyricWord, Playlist, PlaylistCategory, StreamInfo, StreamQuality, Track,
        TrackId,
    },
    provider::{
        AlbumProvider, ChartProvider, CollectionProvider, LinkResolver, LyricsProvider,
        MusicProvider, PlaylistProvider, ResolveOptions, ResolvedLink, SearchPage, SearchQuery,
        StreamResolver, TrackSearch,
    },
    transport::{ClientProfile, Http, RetryPolicy, TransportError, TransportRequest},
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use md5::{Digest, Md5};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet},
    io::Read,
    time::Duration,
};

const ID: &str = "migu";
const UA_MOBILE: &str = "Mozilla/5.0 (Linux; U; Android 11.0.0; zh-cn; MI 11 Build/OPR1.170623.032) AppleWebKit/534.30 (KHTML, like Gecko) Version/4.0 Mobile Safari/534.30";
const UA_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 9_1 like Mac OS X) AppleWebKit/601.1.46 (KHTML, like Gecko) Version/9.0 Mobile/13B143 Safari/601.1";
const UA_ANDROID: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36";
const SEARCH_URL: &str = "https://jadeite.migu.cn/music_search/v3/search/searchAll";
const RESOURCE_INFO_URL: &str = "https://c.musicapp.migu.cn/MIGUM2.0/v1.0/content/resourceinfo.do";
/// Link-parsing song metadata (mg/parse.rs:107-117): the same resourceinfo.do
/// shape as the stream path but a plain GET to the anonymous
/// app.c.nf.migu.cn host (the stream path POSTs to c.musicapp.migu.cn), so it
/// gets its own constant rather than reusing RESOURCE_INFO_URL.
const LINK_SONG_INFO_URL: &str = "https://app.c.nf.migu.cn/MIGUM2.0/v1.0/content/resourceinfo.do";
const STRATEGY_URL: &str = "https://c.musicapp.migu.cn/strategy/listen-url/h5/v2.4";
const LISTEN_SONG_URL: &str = "http://app.pd.nf.migu.cn/MIGUM2.0/v1.0/content/sub/listenSong.do";
const LYRIC_REFERER: &str = "https://app.c.nf.migu.cn/";
const LISTEN_SONG_REFERER: &str = "http://music.migu.cn/";
/// The mobile playing page the H5 strategy endpoint expects as Referer.
const MOBILE_PLAY_REFERER: &str = "https://m.music.migu.cn/v3/lib/playing/930257.html";
// Playlist endpoints (mobile web shape, anonymous).
const PLAYLIST_SQUARE_URL: &str =
    "https://app.c.nf.migu.cn/pc/bmw/page-data/playlist-square-recommend/v1.0";
const PLAYLIST_SONG_URL: &str = "https://app.c.nf.migu.cn/MIGUM3.0/resource/playlist/song/v2.0";
const SEARCH_ALL_URL: &str = "http://pd.musicapp.migu.cn/MIGUM2.0/v1.0/content/search_all.do";
const PLAYLIST_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 13_2_3 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/13.0.3 Mobile/15E148 Safari/604.1";
// Chart endpoints: anonymous GETs whose headers (Referer app.c.nf.migu.cn,
// channel 0146921, Nexus-6 UA) are exactly the fetch_text/fetch_json set.
const CHART_RANK_INDEX_URL: &str = "https://app.c.nf.migu.cn/pc/bmw/rank/rank-index/v1.0";
const CHART_SONGS_URL: &str = "https://app.c.nf.migu.cn/MIGUM2.0/v1.0/content/querycontentbyId.do";
/// Album track list: plain anonymous GET with the playlist header set
/// (Referer m.music.migu.cn, channel 0146921, iPhone UA). pageNo > 1 answers
/// an empty data object live, so the single first-page request is the whole
/// contract.
const ALBUM_SONG_URL: &str = "https://app.c.nf.migu.cn/MIGUM2.0/v1.0/content/queryAlbumSong";
/// Detail pages use a 50-per-page window; albums hardcode the upstream
/// single-page pageSize=100 (never paginated upstream, and live pageNo > 1
/// returns nothing).
const PLAYLIST_PAGE_SIZE: u64 = 50;
const ALBUM_PAGE_SIZE: u64 = 100;
const LYRIC_UA: &str = "Mozilla/5.0 (Linux; Android 5.1.1; Nexus 6 Build/LYZ28E) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/59.0.3071.115 Mobile Safari/537.36";
/// Built-in public user id the anonymous listenSong calls go through.
const MAGIC_USER_ID: &str = "15548614588710179085069";
const SEARCH_DEVICE_ID: &str = "963B7AA0D21511ED807EE5846EC87D20";
const SEARCH_SIGN_MD5: &str = "6cdc72a439cef99a3418d2a78aa28c73";
/// Strategy responses carry this magic prefix plus a one-byte seed.
const MIGU_MAGIC: &[u8] = b"\xab\xcd\x01";
const MIGU_KEY: &[u8] = b"Jk8qzuePiJ1qE3mDYhLQ3T73DtDoAhLP";

#[derive(Clone)]
pub struct MiguProvider {
    /// Business face over the unified transport layer (follow-redirects
    /// profile): search, lyrics, playlists, albums, charts, link info, the
    /// resolve resource POST + strategy GET and the served-size probe legs.
    http: Http,
    /// listenSong.do answers with a 302 whose Location is the CDN link;
    /// redirects must be followed manually to read it, so this handle's
    /// profile carries `redirect_none: true` (the old `listen_client`,
    /// TRANSPORT_DESIGN.md §4.3).
    listen_http: Http,
}
impl std::fmt::Debug for MiguProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiguProvider").finish()
    }
}
impl Default for MiguProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl MiguProvider {
    pub fn new() -> Self {
        // Client construction moved onto the unified transport layer
        // (TRANSPORT_DESIGN.md §4.3): both profiles keep exactly today's
        // knobs — 15s total timeout; the listen handle adds `redirect_none`
        // mapping to `reqwest::redirect::Policy::none()` so the 302 hop
        // stays readable. The build fallback shape lives in
        // `transport::build_client`.
        let http = Http::from_profile(&ClientProfile::provider_default());
        let listen_http = Http::from_profile(&ClientProfile {
            redirect_none: true,
            ..ClientProfile::provider_default()
        });
        Self { http, listen_http }
    }
    /// Test seam: inject the two transport handles (mocks in unit tests).
    /// The production constructor is [`MiguProvider::new`]. Test-only so
    /// non-test builds see no dead constructor.
    #[cfg(test)]
    pub(crate) fn with_transports(http: Http, listen_http: Http) -> Self {
        Self { http, listen_http }
    }
    /// Register all currently implemented Migu capabilities in one step.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_lyrics(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_playlist(provider.clone());
        registry.register_album(provider.clone());
        registry.register_chart(provider.clone());
        registry.register_link_resolver(provider);
    }
    /// Adds the mobile UA to a [`TransportRequest`]; kept provider-side
    /// (playing header set, TRANSPORT_DESIGN.md §1).
    fn request(&self, request: TransportRequest) -> TransportRequest {
        request.header("user-agent", UA_MOBILE)
    }
    /// Lyric/chart GET family through the unified transport layer
    /// (`send_raw` + `RetryPolicy::standard()`): migu was single-shot with
    /// no retry anywhere, so this is a declared behavior upgrade
    /// (TRANSPORT_DESIGN.md §5.5 rows 1/3/4) — transport errors, 429/5xx and
    /// mid-body resets now retry with exponential backoff + jitter +
    /// `Retry-After` (at most 5 sends). Today's error texts carry over:
    /// `classify_http` Fatal renders `HTTP {status}` inside the same
    /// `{label}: ...` prefix, and send failures keep `{label}: {error}`.
    async fn fetch_bytes(&self, url: &str, label: &str) -> Result<Vec<u8>> {
        let request = TransportRequest::get(url)
            .header("user-agent", LYRIC_UA)
            .header("referer", LYRIC_REFERER)
            .header("channel", "0146921");
        let response = self
            .http
            .send_raw(&request, &RetryPolicy::standard())
            .await
            .map_err(|e| perr(format!("{label}: {e}")))?;
        Ok(response.body)
    }
    async fn fetch_text(&self, url: &str, label: &str) -> Result<String> {
        let request = TransportRequest::get(url)
            .header("user-agent", LYRIC_UA)
            .header("referer", LYRIC_REFERER)
            .header("channel", "0146921");
        let response = self
            .http
            .send_raw(&request, &RetryPolicy::standard())
            .await
            .map_err(|e| perr(format!("{label}: {e}")))?;
        Ok(String::from_utf8_lossy(&response.body).into_owned())
    }
    /// GET plus JSON decode over the lyric header set (charts endpoints).
    async fn fetch_json(&self, url: &str, label: &str) -> Result<Value> {
        let text = self.fetch_text(url, label).await?;
        serde_json::from_str(&text).map_err(|e| perr(format!("{label}: parse failed: {e}")))
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}
/// `true` for the statuses the standard ladder already retried to exhaustion
/// (retry.rs:80-85): 429 and 5xx. When one of these still surfaces on the
/// stream faces, the pipeline stayed down through the whole ladder — that is
/// 链路不可用, not a missing resource, so it splits to
/// [`Error::Unavailable`] (AUTH_REGISTRY_DESIGN.md §9.5); every other
/// non-success keeps the upstream resource-missing mapping
/// ([`Error::NotFound`], mg/url.rs:384-386).
fn ladder_exhausted_status(code: u16) -> bool {
    code == 429 || (500..600).contains(&code)
}
/// The M2 error for a terminal non-success status on the stream faces. Pure so
/// the split is pinned without five scripted sends with real backoff sleeps
/// (the netease `api_code_error` seam, source_netease.rs:289-326); retry.rs
/// pins 429/5xx-exhaustion → `Fatal("HTTP {code}")` and the e2e 404 test
/// (`resource_post_status_error_maps_to_not_found`) pins the call-site wiring.
fn terminal_status_error(label: &str, code: u16) -> Error {
    if ladder_exhausted_status(code) {
        return Error::Unavailable {
            provider: Some(ID.into()),
            detail: Some(ErrorDetail::new(format!("{label}: HTTP {code}"))),
        };
    }
    Error::NotFound
}
/// Same mapping from the engine's `classify_http` Fatal text — the exact shape
/// [`TransportError::Status`] carries (transport.rs:511, retry.rs:84). A text
/// outside that contract keeps the upstream NotFound default (defensive: the
/// resource POST is `.idempotent(true)`, so the engine only ever hands this
/// face "HTTP {code}" texts).
fn status_error_from_text(label: &str, text: &str) -> Error {
    match text
        .trim()
        .strip_prefix("HTTP ")
        .and_then(|code| code.trim().parse::<u16>().ok())
    {
        Some(code) => terminal_status_error(label, code),
        None => Error::NotFound,
    }
}
/// Classify the listenSong refusal body (the non-3xx answer). Upstream
/// documents the body as the interface's way of explaining the refusal —
/// "接口用 JSON 里的 code 说明原因（VIP/无版权/参数问题）" (mg/url.rs:285) — and
/// collapses all of them into `FetchError::NotFound`. The rights refusal is
/// the one observable 版权拦截响应 (the pinned body shape is
/// `{"code":"406001","msg":"no rights"}`), so a rights/copyright refusal text
/// maps to [`Error::CopyrightRestricted`]; there is no code table anywhere
/// upstream (the §9.2 kugou precedent applies: no fixture-less code tables),
/// so classification keys on the refusal text and every other or unparseable
/// body keeps the upstream NotFound default. VIP refusals are deliberately not
/// split: no fixture pins a VIP body shape, and a keyword guess would
/// misclassify platform texts (recorded in AUTH_REGISTRY_DESIGN.md §9.5).
fn listen_refusal_error(body: &[u8]) -> Error {
    let refusal: Option<String> = serde_json::from_slice::<Value>(body).ok().and_then(|json| {
        let code = migu_value_string(&json["code"]);
        let message = json["msg"]
            .as_str()
            .or_else(|| json["info"].as_str())
            .unwrap_or_default()
            .trim()
            .to_owned();
        (!message.is_empty()).then(|| {
            if code.is_empty() {
                message
            } else {
                format!("code {code}, {message}")
            }
        })
    });
    let Some(detail_text) = refusal else {
        return Error::NotFound;
    };
    let lowered = detail_text.to_lowercase();
    if lowered.contains("right") || lowered.contains("版权") {
        return Error::CopyrightRestricted {
            provider: Some(ID.into()),
            detail: Some(ErrorDetail::new(format!("listen url: {detail_text}"))),
        };
    }
    Error::NotFound
}
/// Migu search signature: MD5 over keyword, two fixed salts, device id and
/// the millisecond timestamp (which is echoed in a header).
fn mg_sign(keyword: &str) -> (String, String) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string();
    let sign = mg_sign_with(keyword, &timestamp);
    (sign, timestamp)
}
/// Pure digest core so the fixture goldens can pin the exact concatenation
/// (see tests/fixtures/migu/mg_sign.json).
fn mg_sign_with(keyword: &str, timestamp: &str) -> String {
    format!(
        "{:x}",
        Md5::digest(format!(
            "{keyword}{SEARCH_SIGN_MD5}yyapp2d16148780a1dcc7408e06336b98cfd50{SEARCH_DEVICE_ID}{timestamp}"
        )
        .as_bytes())
    )
}
fn mg_value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|n| n.to_string()))
        .or_else(|| value.as_u64().map(|n| n.to_string()))
        .unwrap_or_default()
}
fn format_to_quality(format_type: &str) -> Option<StreamQuality> {
    match format_type {
        "PQ" => Some(StreamQuality::Low),
        "HQ" => Some(StreamQuality::High),
        "SQ" => Some(StreamQuality::Lossless),
        "ZQ" | "ZQ24" | "ZQ32" => Some(StreamQuality::HiRes),
        _ => None,
    }
}
fn quality_to_format(quality: StreamQuality) -> &'static str {
    match quality {
        StreamQuality::Low => "PQ",
        StreamQuality::High => "HQ",
        StreamQuality::Lossless => "SQ",
        StreamQuality::HiRes => "ZQ",
        _ => "PQ",
    }
}
fn format_type_quality(format_type: &str) -> Option<StreamQuality> {
    match format_type.trim() {
        "PQ" => Some(StreamQuality::Low),
        "HQ" => Some(StreamQuality::High),
        "SQ" => Some(StreamQuality::Lossless),
        "ZQ" | "ZQ24" => Some(StreamQuality::HiRes),
        _ => None,
    }
}
/// Duration arrives as seconds or as `mm:ss` / `hh:mm:ss` text.
fn duration_seconds(value: &Value) -> u64 {
    if let Some(seconds) = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    {
        return seconds;
    }
    let Some(text) = value.as_str() else {
        return 0;
    };
    let mut parts = text
        .split(':')
        .rev()
        .filter_map(|part| part.parse::<u64>().ok());
    let seconds = parts.next().unwrap_or(0);
    let minutes = parts.next().unwrap_or(0);
    let hours = parts.next().unwrap_or(0);
    hours.saturating_mul(3600) + minutes.saturating_mul(60) + seconds
}
fn first_string(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| value[*key].as_str().filter(|text| !text.is_empty()))
        .unwrap_or_default()
        .to_owned()
}
fn singer_names(value: &Value) -> Vec<String> {
    for key in ["singerList", "artists"] {
        if let Some(items) = value[key].as_array() {
            let names: Vec<String> = items
                .iter()
                .filter_map(|item| item["name"].as_str())
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect();
            if !names.is_empty() {
                return names;
            }
        }
    }
    first_string(value, &["singer", "author", "txt2"])
        .split('、')
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .collect()
}
fn mg_cover_url(value: &Value) -> Option<String> {
    let direct = first_string(value, &["img3", "img2", "img1"]);
    if !direct.is_empty() {
        return Some(direct);
    }
    value["albumImgs"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["img"].as_str().or_else(|| item["webpImg"].as_str()))
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
}
fn mg_parse_song(value: &Value) -> Option<Track> {
    let song_id = mg_value_string(&value["songId"]);
    if song_id.is_empty() {
        return None;
    }
    let title = first_string(value, &["songName", "name", "txt"]);
    if title.trim().is_empty() {
        return None;
    }
    let formats = value["audioFormats"]
        .as_array()
        .or_else(|| value["newRateFormats"].as_array())
        .or_else(|| value["rateFormats"].as_array());
    let mut qualities = Vec::new();
    if let Some(formats) = formats {
        for format in formats {
            if let Some(quality) =
                format_to_quality(format["formatType"].as_str().unwrap_or_default())
                && !qualities.contains(&quality)
            {
                qualities.push(quality);
            }
        }
    }
    let mut extra = BTreeMap::new();
    for key in [
        "copyrightId",
        "contentId",
        "lrcUrl",
        "mrcurl",
        "mrcUrl",
        "trcUrl",
    ] {
        if let Some(text) = value[key].as_str().filter(|text| !text.is_empty()) {
            let key = if key == "mrcurl" { "mrcUrl" } else { key };
            extra.insert(key.into(), text.to_owned());
        }
    }
    let album = first_string(value, &["album", "albumName", "txt3"]);
    let album_id = mg_value_string(&value["albumId"]);
    Some(Track {
        id: TrackId::new(ID, song_id),
        title,
        artists: singer_names(value),
        album: (!album.is_empty()).then_some(album),
        album_id: (!album_id.is_empty()).then_some(album_id),
        duration: {
            let seconds =
                duration_seconds(&value["duration"]).max(duration_seconds(&value["length"]));
            (seconds > 0).then(|| Duration::from_secs(seconds))
        },
        artwork_url: mg_cover_url(value),
        isrc: None,
        available_qualities: qualities,
        audio: None,
        extra,
    })
}
/// Same quality can ship several file variants (generic / Android / iOS) with
/// differently named size fields; a URL variant must pair with its own size
/// or integrity checks will misfire (SQ: flac 31529675 vs m4a 31931278).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileVariant {
    Generic,
    Android,
    Ios,
}
impl FileVariant {
    const ORDER: [FileVariant; 3] = [FileVariant::Generic, FileVariant::Android, FileVariant::Ios];
    fn url_key(self) -> &'static str {
        match self {
            FileVariant::Generic => "url",
            FileVariant::Android => "androidUrl",
            FileVariant::Ios => "iosUrl",
        }
    }
    fn size_keys(self) -> &'static [&'static str] {
        match self {
            FileVariant::Generic => &[
                "size",
                "fileSize",
                "filesize",
                "androidSize",
                "iosSize",
                "asize",
                "isize",
            ],
            FileVariant::Android => &["androidSize", "asize", "size", "fileSize", "filesize"],
            FileVariant::Ios => &["iosSize", "isize", "size", "fileSize", "filesize"],
        }
    }
}
fn format_type_matches(format: &Value, quality: StreamQuality) -> bool {
    let format_type = format["formatType"].as_str().unwrap_or("");
    format_type == quality_to_format(quality)
        || (quality == StreamQuality::HiRes && format_type == "ZQ24")
}
fn format_url(format: &Value, variant: FileVariant) -> Option<String> {
    format[variant.url_key()]
        .as_str()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
}
fn parse_size_text(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let digits_end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits_end);
    let value: f64 = number.trim().parse().ok()?;
    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = (value * multiplier).round();
    (bytes >= 1.0).then_some(bytes as u64)
}
fn extract_size(format: &Value, variant: FileVariant) -> Option<u64> {
    variant.size_keys().iter().find_map(|key| {
        let field = &format[*key];
        field
            .as_u64()
            .filter(|bytes| *bytes > 0)
            .or_else(|| field.as_str().and_then(parse_size_text))
    })
}
/// Migu tags preview/audition URLs in the path or the query; those are not
/// complete audio files and must be rejected.
fn explicit_preview_url(raw: &str) -> bool {
    let raw = raw.trim();
    if raw.is_empty() {
        return false;
    }
    let after_scheme = raw.split_once("://").map_or(raw, |(_, rest)| rest);
    let (path_with_host, query) = match after_scheme.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (after_scheme, None),
    };
    let path = path_with_host.split_once('/').map_or("", |(_, path)| path);
    for segment in path.split('/') {
        if matches!(
            segment.to_ascii_lowercase().as_str(),
            "preview" | "audition" | "trail_audio"
        ) {
            return true;
        }
    }
    let Some(query) = query else {
        return false;
    };
    query.split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if !matches!(
            key.to_ascii_lowercase().as_str(),
            "preview" | "is_preview" | "node_is_preview" | "audition" | "trail_audio"
        ) {
            return false;
        }
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}
/// Strategy responses carry an `\xab\xcd\x01<seed>` prefix; the body is
/// byte-wise `(byte + seed - key[i % 32])` over a fixed 32-byte key.
fn decrypt_strategy_response(raw: &[u8]) -> Result<Value> {
    let plain = if raw.starts_with(MIGU_MAGIC) && raw.len() >= 4 {
        let seed = raw[3];
        raw[4..]
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                byte.wrapping_add(seed)
                    .wrapping_sub(MIGU_KEY[index % MIGU_KEY.len()])
            })
            .collect::<Vec<u8>>()
    } else {
        raw.to_vec()
    };
    serde_json::from_slice(&plain).map_err(|e| perr(format!("strategy response parse failed: {e}")))
}
/// Map the actually served file size back to a declared format tier; Migu
/// anonymously serves PQ regardless of the requested toneFlag.
fn quality_for_size(formats: &[Value], served_size: u64) -> Option<StreamQuality> {
    for format in formats {
        let Some(quality) = format_type_quality(format["formatType"].as_str().unwrap_or_default())
        else {
            continue;
        };
        for variant in FileVariant::ORDER {
            if extract_size(format, variant) == Some(served_size) {
                return Some(quality);
            }
        }
    }
    None
}
/// resourceinfo.do needs the contentId; the copyrightId finds nothing, but is
/// kept as a fallback for older cached data.
fn resource_id(track: &Track) -> Option<&str> {
    track
        .extra
        .get("contentId")
        .or_else(|| track.extra.get("copyrightId"))
        .map(String::as_str)
        .filter(|id| !id.trim().is_empty())
}
/// Migu has no category-directory endpoint; the official search filters
/// playlists by keyword, so the categories are a fixed keyword table grouped
/// for UI purposes (mirrors music-lib).
const PLAYLIST_CATEGORY_KEYWORDS: &[(&str, &str, bool)] = &[
    ("华语", "语种", true),
    ("欧美", "语种", true),
    ("日语", "语种", false),
    ("韩语", "语种", false),
    ("粤语", "语种", false),
    ("流行", "风格", true),
    ("摇滚", "风格", true),
    ("民谣", "风格", true),
    ("电子", "风格", false),
    ("古典", "风格", false),
    ("爵士", "风格", false),
    ("乡村", "风格", false),
    ("轻音乐", "风格", false),
    ("影视原声", "场景", false),
    ("二次元", "场景", false),
    ("运动", "场景", false),
    ("睡前", "场景", false),
    ("驾车", "场景", false),
    ("怀旧", "场景", false),
    ("KTV", "场景", false),
];
fn migu_value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
        .unwrap_or_default()
}
fn migu_value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}
fn migu_first_string(item: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| item[*key].as_str().filter(|text| !text.is_empty()))
        .map(str::to_owned)
}
/// The square-recommend payload nests playlists (resType 2021) at arbitrary
/// depths; walk the whole tree and collect unique ids.
fn collect_square_playlists(value: &Value, seen: &mut HashSet<String>, out: &mut Vec<Playlist>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_square_playlists(item, seen, out);
            }
        }
        Value::Object(map) => {
            if map.get("resType").and_then(Value::as_str) == Some("2021") {
                let id = map.get("resId").map(migu_value_string).unwrap_or_default();
                let title = map
                    .get("txt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if !id.is_empty() && !title.is_empty() && seen.insert(id.clone()) {
                    out.push(Playlist {
                        id: TrackId::new(ID, id),
                        title,
                        description: map
                            .get("txt2")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                            .map(str::to_owned),
                        artwork_url: map
                            .get("img")
                            .and_then(Value::as_str)
                            .filter(|url| !url.is_empty())
                            .map(str::to_owned),
                        track_count: None,
                        tracks: vec![],
                        play_count: None,
                        creator: None,
                        link: None,
                        extra: Default::default(),
                    });
                }
            }
            for child in map.values() {
                if child.is_array() || child.is_object() {
                    collect_square_playlists(child, seen, out);
                }
            }
        }
        _ => {}
    }
}
fn migu_parse_searched_playlist(item: &Value) -> Option<Playlist> {
    let id = migu_value_string(&item["id"]);
    let title = item["name"].as_str()?.trim().to_owned();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    let cover = migu_first_string(item, &["musicListPicUrl"]).or_else(|| {
        item["imgItems"]
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|img| img["imgSizeType"].as_str() == Some("03"))
                    .or_else(|| items.first())
            })
            .and_then(|img| img["img"].as_str())
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
    });
    // Upstream parse_searched_playlist (mg/playlist.rs:148-154): playNum is
    // numeric or numeric-text, the creator is the first string among
    // userName/ownerName (find-then-filter: a blank first hit voids the whole
    // chain, an inherited upstream quirk), and the canonical web link rides
    // every row (M3 s4 slots, MODEL_ALIGNMENT_DESIGN.md §5.1).
    let creator = ["userName", "ownerName"]
        .iter()
        .find_map(|key| item[*key].as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    Some(Playlist {
        id: TrackId::new(ID, id.clone()),
        title,
        description: None,
        artwork_url: cover,
        track_count: migu_value_u64(&item["musicNum"]).map(|n| n as u32),
        tracks: vec![],
        play_count: migu_value_u64(&item["playNum"]),
        creator,
        link: Some(format!("https://music.migu.cn/v3/music/playlist/{id}")),
        extra: Default::default(),
    })
}
impl MiguProvider {
    /// The shared keyword-search entry: `searchSwitch` selects the type.
    async fn migu_search_all(
        &self,
        keyword: &str,
        page: u32,
        limit: u32,
        search_switch: &str,
    ) -> Result<Value> {
        let url = format!(
            "{SEARCH_ALL_URL}?ua=Android_migu&version=5.0.1&text={}&pageNo={}&pageSize={}&searchSwitch={}",
            urlencoding::encode(keyword.trim()),
            page.max(1),
            limit,
            urlencoding::encode(search_switch)
        );
        let request = self.request(TransportRequest::get(&url));
        let response = self
            .http
            .send_raw(&request, &RetryPolicy::standard())
            .await
            .map_err(|e| perr(format!("search all: {e}")))?;
        serde_json::from_slice(&response.body)
            .map_err(|e| perr(format!("search all parse failed: {e}")))
    }
    /// One playlist-detail page: header metadata plus rows.
    async fn fetch_playlist_page(&self, id: &str, page: u32) -> Result<PlaylistPage> {
        let url = format!(
            "{PLAYLIST_SONG_URL}?pageNo={page}&pageSize={PLAYLIST_PAGE_SIZE}&playlistId={id}"
        );
        // The playlist header set: today `request()` appended the mobile UA
        // first and the playlist UA second, both sent (reqwest header
        // append); the transport layer applies `TransportRequest.headers` in
        // the same build order, so the duplication carries over verbatim.
        let request = self
            .request(TransportRequest::get(&url))
            .header("referer", "https://m.music.migu.cn/")
            .header("channel", "0146921")
            .header("user-agent", PLAYLIST_UA);
        let response = self
            .http
            .send_raw(&request, &RetryPolicy::standard())
            .await
            .map_err(|e| perr(format!("playlist detail: {e}")))?;
        let json: Value = serde_json::from_slice(&response.body)
            .map_err(|e| perr(format!("playlist detail parse failed: {e}")))?;
        parse_playlist_page(&json)
    }
    /// One album-song page over the same header set as the playlist detail
    /// (upstream mg/parse.rs:90-102): plain anonymous GET, no signature.
    async fn fetch_album_page(&self, id: &str) -> Result<Value> {
        let url = format!("{ALBUM_SONG_URL}?albumId={id}&pageNo=1&pageSize={ALBUM_PAGE_SIZE}");
        let request = self
            .request(TransportRequest::get(&url))
            .header("referer", "https://m.music.migu.cn/")
            .header("channel", "0146921")
            .header("user-agent", PLAYLIST_UA);
        let response = self
            .http
            .send_raw(&request, &RetryPolicy::standard())
            .await
            .map_err(|e| perr(format!("album detail: {e}")))?;
        serde_json::from_slice(&response.body)
            .map_err(|e| perr(format!("album detail parse failed: {e}")))
    }
    /// The song-link metadata GET (upstream mg/parse.rs fetch_song, :107-117):
    /// `resourceType=2&resourceId={contentId}` over the playlist header set.
    /// Only metadata — upstream keeps the play URL out of link parsing on
    /// purpose (parse.rs:106: the stream address needs an extra key
    /// negotiation), so this Track carries no stream URL and streaming still
    /// goes through resolve(). The id must be the contentId: a copyrightId
    /// finds no resource (mg/song.rs:114-117). A missing/empty `resource`
    /// array or an unparseable row is the upstream NotFound.
    async fn fetch_song_link_info(&self, content_id: &str) -> Result<Track> {
        let url = format!("{LINK_SONG_INFO_URL}?resourceType=2&resourceId={content_id}");
        let request = self
            .request(TransportRequest::get(&url))
            .header("referer", "https://m.music.migu.cn/")
            .header("channel", "0146921")
            .header("user-agent", PLAYLIST_UA);
        let response = self
            .http
            .send_raw(&request, &RetryPolicy::standard())
            .await
            .map_err(|e| perr(format!("song link info: {e}")))?;
        let json: Value = serde_json::from_slice(&response.body)
            .map_err(|e| perr(format!("song link info parse failed: {e}")))?;
        json["resource"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(mg_parse_song)
            .ok_or(Error::NotFound)
    }
}
/// One parsed playlist-song page: the header chains and the rows.
///
/// The creator and cover chains port upstream `get_detail_with_meta`
/// (mg/playlist.rs:82-91, the M3 s4 producer slice): both use upstream's
/// find-then-filter shape — the first key holding *any* string wins, then one
/// filter applies (creator: non-blank after trim; cover: non-empty), so a
/// blank first hit voids the whole chain rather than falling through to the
/// next key. The M3 slots they feed live on `Playlist`
/// (MODEL_ALIGNMENT_DESIGN.md §5.1).
struct PlaylistPage {
    title: Option<String>,
    creator: Option<String>,
    cover: Option<String>,
    songs: Vec<Track>,
    total: Option<u64>,
}
/// Parse one playlist-song page: `code` gate, title fallback chain
/// (name -> title -> playlistName), creator chain userName -> ownerName ->
/// nickName, cover chain imgUrl -> picUrl -> cover, rows through the shared
/// song parser and the numeric-or-string `totalCount`
/// (see tests/fixtures/migu/playlist_pages.json).
fn parse_playlist_page(json: &Value) -> Result<PlaylistPage> {
    if json["code"].as_str() != Some("000000") {
        return Err(perr("playlist detail request failed"));
    }
    let data = &json["data"];
    let title = migu_first_string(data, &["name", "title", "playlistName"]);
    let creator = ["userName", "ownerName", "nickName"]
        .iter()
        .find_map(|key| data[*key].as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    let cover = ["imgUrl", "picUrl", "cover"]
        .iter()
        .find_map(|key| data[*key].as_str())
        .filter(|url| !url.is_empty())
        .map(str::to_owned);
    let songs = data["songList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(mg_parse_song_alias)
        .collect::<Vec<_>>();
    let total = migu_value_u64(&data["totalCount"])
        .or_else(|| migu_value_u64(&json["data"]["songList"]["totalCount"]));
    Ok(PlaylistPage {
        title,
        creator,
        cover,
        songs,
        total,
    })
}
/// Playlist pagination stop rule: an empty page, everything fetched, or the
/// defensive 40-page cap (upstream has no cap).
fn playlist_page_done(
    page_empty: bool,
    fetched: usize,
    total: Option<u64>,
    current_page: u32,
) -> bool {
    page_empty || fetched as u64 >= total.unwrap_or(u64::MAX) || current_page >= 40
}
/// Alias so both the song module and the playlist module share one parser.
fn mg_parse_song_alias(item: &Value) -> Option<Track> {
    mg_parse_song(item)
}
/// Parse one album-song page into (derived metadata, tracks).
///
/// Port of voicefox `mg/parse.rs` fetch_album: rows in `data.songList` go
/// through the shared song parser; there is no album-detail endpoint, so the
/// metadata is derived from the first track — title from its `album` field
/// (fallback 咪咕专辑 {id}), artists and cover from the row — plus the
/// numeric-or-string `totalCount` as the track count (upstream falls back to
/// the parsed row count when totalCount is absent).
///
/// The `code` gate is musid-dli convention (upstream fetch_album has none).
/// It cannot catch the live empty-answer paths anyway: an invalid albumId or
/// pageNo > 1 answers HTTP 200 with code "000000" and an empty `data`
/// object, so NotFound is derived from the empty songList exactly like
/// upstream (mg/parse.rs:125-133).
/// (see tests/fixtures/migu/album_songs.json).
fn parse_album_page(json: &Value, id: &str) -> Result<(Album, Vec<Track>)> {
    if json["code"].as_str() != Some("000000") {
        return Err(perr("album detail request failed"));
    }
    let data = &json["data"];
    let songs = data["songList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(mg_parse_song_alias)
        .collect::<Vec<_>>();
    let first = songs.first().ok_or(Error::NotFound)?;
    let title = first
        .album
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("咪咕专辑 {id}"));
    let album = Album {
        id: TrackId::new(ID, id.to_owned()),
        title,
        artists: first.artists.clone(),
        artwork_url: first.artwork_url.clone(),
        track_count: Some(migu_value_u64(&data["totalCount"]).unwrap_or(songs.len() as u64) as u32),
    };
    Ok((album, songs))
}

// ── Link parsing (share links; mg/parse.rs, audit 2026-11-18) ───────────

/// Link targets (mg/parse.rs:18-23): exactly three shapes exist — a song
/// contentId, a playlist id and an album id. No artist form is produced
/// upstream (the mg module has no artist function; ParsedLink carries
/// Song/Playlist/Album only).
#[derive(Debug, Clone, PartialEq, Eq)]
enum MgLink {
    Song { content_id: String },
    Playlist { id: String },
    Album { id: String },
}

/// Link target extraction (mg/parse.rs:25-56): the link is trimmed, then the
/// song pattern `music\.migu\.cn/v3/music/song/(\d+)` wins first, followed by
/// the playlist loop (`playlistId=`, `musicListId=`, `(?:playlist|songlist)/`)
/// and the album loop (`music\.migu\.cn/(?:v3|v5)/music/album/`, `albumId=`,
/// `resourceId=`); a bare all-digits string without `/` routes to Playlist
/// (shares commonly carry just the number, and never infer a song). Every
/// capture is unanchored — there is no scheme/host validation upstream, so a
/// `music.migu.cn/...` fragment anywhere in the string matches (documented
/// audit gap, inherited). Anything else is unrecognized.
fn mg_link_target(link: &str) -> Result<MgLink> {
    let trimmed = link.trim();
    if let Some(content_id) = mg_link_capture(r"music\.migu\.cn/v3/music/song/(\d+)", trimmed) {
        return Ok(MgLink::Song { content_id });
    }
    for pattern in [
        r"playlistId=(\d+)",
        r"musicListId=(\d+)",
        r"(?:playlist|songlist)/(\d+)",
    ] {
        if let Some(id) = mg_link_capture(pattern, trimmed) {
            return Ok(MgLink::Playlist { id });
        }
    }
    for pattern in [
        r"music\.migu\.cn/(?:v3|v5)/music/album/(\d+)",
        r"albumId=(\d+)",
        r"resourceId=(\d+)",
    ] {
        if let Some(id) = mg_link_capture(pattern, trimmed) {
            return Ok(MgLink::Album { id });
        }
    }
    // Bare numeric ids ride along: shares commonly carry just the number
    // (mg/parse.rs:48-54), and they always route to Playlist — a bare song
    // contentId is never inferred as a song.
    if !trimmed.is_empty() && !trimmed.contains('/') && trimmed.chars().all(|c| c.is_ascii_digit())
    {
        return Ok(MgLink::Playlist {
            id: trimmed.to_owned(),
        });
    }
    Err(Error::InvalidInput(
        "unrecognized Migu song/playlist/album link".into(),
    ))
}

/// The per-call regex compile mirrors upstream `capture` (mg/parse.rs:58-64);
/// captures are unanchored (first match anywhere).
fn mg_link_capture(pattern: &str, value: &str) -> Option<String> {
    Regex::new(pattern)
        .ok()?
        .captures(value)
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str().to_owned())
}
/// Rank-index catalogue gate: `code == "000000"` else the `info` text (the
/// fixed fallback matches upstream).
/// (see tests/fixtures/migu/chart_rank_index.json).
fn parse_rank_index(json: &Value) -> Result<Vec<Chart>> {
    if json["code"].as_str() != Some("000000") {
        return Err(perr(
            json["info"]
                .as_str()
                .unwrap_or("咪咕榜单目录请求失败")
                .to_owned(),
        ));
    }
    let mut seen = HashSet::new();
    let mut boards = Vec::new();
    collect_boards(&json["data"]["contents"], None, &mut seen, &mut boards);
    if boards.is_empty() {
        return Err(perr("咪咕未返回可用榜单"));
    }
    Ok(boards)
}
/// Boards hide at any depth inside `data.contents`: an object carrying both a
/// string `rankId` and a string `rankName` is one (deduped by id, so embedded
/// song previews without a `rankId` never enter). The update note inherits the
/// nearest ancestor's non-empty `desc`.
fn collect_boards(
    value: &Value,
    inherited_desc: Option<&str>,
    seen: &mut HashSet<String>,
    out: &mut Vec<Chart>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_boards(item, inherited_desc, seen, out);
            }
        }
        Value::Object(map) => {
            let own_desc = map
                .get("desc")
                .and_then(Value::as_str)
                .filter(|desc| !desc.is_empty())
                .or(inherited_desc);
            if let (Some(id), Some(name)) = (
                map.get("rankId").and_then(Value::as_str),
                map.get("rankName").and_then(Value::as_str),
            ) && seen.insert(id.to_owned())
            {
                out.push(Chart {
                    id: TrackId::new(ID, id.to_owned()),
                    title: name.to_owned(),
                    description: own_desc.map(str::to_owned),
                    // Upstream has one text slot and names it `update`
                    // (LeaderboardInfo.update, fed by the same desc chain at
                    // mg/leaderboard.rs:100-104,111), so the cadence text now
                    // rides the dedicated M3 slot too. The legacy description
                    // mapping stays: pre-M3 readers keyed on it, and the
                    // serde migration is additive (R1/R4).
                    updated_description: own_desc.map(str::to_owned),
                });
            }
            for child in map.values() {
                if child.is_array() || child.is_object() {
                    collect_boards(child, own_desc, seen, out);
                }
            }
        }
        _ => {}
    }
}
/// Board songs gate plus the `columnInfo.contents` shape check (upstream
/// errors when the array is missing).
/// (see tests/fixtures/migu/chart_board.json).
fn parse_chart_board(json: &Value) -> Result<Vec<ChartEntry>> {
    if json["code"].as_str() != Some("000000") {
        return Err(perr(
            json["info"]
                .as_str()
                .unwrap_or("咪咕榜单请求失败")
                .to_owned(),
        ));
    }
    let raw_items = json["columnInfo"]["contents"]
        .as_array()
        .ok_or_else(|| perr("咪咕榜单歌曲列表为空"))?;
    // The API delivers the whole board per call and the ChartProvider trait
    // returns it whole, so upstream's client-side skip/take pagination has no
    // target here. Ranks renumber 1..N over the retained items in array order
    // (the upstream filter_map order): a dropped entry shifts later ranks
    // forward. The wrapper's relationSort is ignored — the live board carries
    // relationSort 70,69,68,... which does not match array positions.
    Ok(raw_items
        .iter()
        .filter_map(|item| mg_parse_song(&item["objectInfo"]))
        .enumerate()
        .map(|(index, track)| ChartEntry {
            rank: index as u32 + 1,
            track,
        })
        .collect())
}
#[async_trait]
impl CollectionProvider for MiguProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist> {
        let page = self.fetch_playlist_page(id, 1).await?;
        if page.songs.is_empty() {
            return Err(Error::NotFound);
        }
        // creator/cover ride the detail-page chains (upstream
        // get_detail_with_meta, mg/playlist.rs:82-91); the link is the same
        // canonical web URL upstream stamps there (:92). play_count stays
        // None: the detail endpoint declares none and upstream fills none.
        Ok(Playlist {
            id: TrackId::new(ID, id.to_owned()),
            title: page.title.unwrap_or_else(|| format!("Migu playlist {id}")),
            description: None,
            artwork_url: page.cover,
            track_count: page.total.map(|n| n as u32),
            tracks: vec![],
            play_count: None,
            creator: page.creator,
            link: Some(format!("https://music.migu.cn/v3/music/playlist/{id}")),
            extra: Default::default(),
        })
    }
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let mut all = Vec::new();
        let mut current_page = 1u32;
        loop {
            let page = self.fetch_playlist_page(id, current_page).await?;
            let page_empty = page.songs.is_empty();
            all.extend(page.songs);
            let done = playlist_page_done(page_empty, all.len(), page.total, current_page);
            if done {
                break;
            }
            current_page += 1;
        }
        if all.is_empty() {
            return Err(Error::NotFound);
        }
        Ok(all)
    }
}
#[async_trait]
impl PlaylistProvider for MiguProvider {
    /// Fixed keyword table standing in for the missing category directory.
    async fn categories(&self) -> Result<Vec<PlaylistCategory>> {
        let mut result = vec![PlaylistCategory {
            id: String::new(),
            name: "全部".into(),
            group: Some("全部".into()),
            count: 0,
            hot: true,
            provider: Some(ID.to_owned()),
            extra: Default::default(),
        }];
        for (name, group, hot) in PLAYLIST_CATEGORY_KEYWORDS {
            result.push(PlaylistCategory {
                id: (*name).into(),
                name: (*name).into(),
                group: Some((*group).into()),
                count: 0,
                hot: *hot,
                provider: Some(ID.to_owned()),
                extra: Default::default(),
            });
        }
        Ok(result)
    }
    /// The category list is a keyword search; the empty category maps to
    /// "华语" (mirroring music-lib).
    async fn playlists(&self, category: &str, page: u32, page_size: u32) -> Result<Vec<Playlist>> {
        // Empty category: the playlist-square-recommend page (nested payload,
        // resType 2021 entries collected by walking the whole tree). Non-empty
        // categories fall through to the keyword search.
        if category.trim().is_empty() {
            let url = format!(
                "{PLAYLIST_SQUARE_URL}?templateVersion=2&pageNo={}",
                page.max(1)
            );
            let request = self
                .request(TransportRequest::get(&url))
                .header("referer", "https://m.music.migu.cn/")
                .header("channel", "0146921")
                .header("user-agent", PLAYLIST_UA);
            let response = self
                .http
                .send_raw(&request, &RetryPolicy::standard())
                .await
                .map_err(|e| perr(format!("hot playlists: {e}")))?;
            let json: Value = serde_json::from_slice(&response.body)
                .map_err(|e| perr(format!("hot playlists parse failed: {e}")))?;
            if json["code"].as_str() != Some("000000") {
                return Err(perr("hot playlists request failed"));
            }
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            collect_square_playlists(&json["data"]["contents"], &mut seen, &mut out);
            return Ok(out);
        }
        let keyword = if category.trim().is_empty() {
            "华语"
        } else {
            category.trim()
        };
        let json = self
            .migu_search_all(
                keyword,
                page.max(1),
                page_size.clamp(1, 50),
                r#"{"song":0,"album":0,"singer":0,"tagSong":0,"mvSong":0,"songlist":1,"bestShow":1}"#,
            )
            .await?;
        let mut result: Vec<Playlist> = json["songListResultData"]["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(migu_parse_searched_playlist)
            .collect();
        // Upstream get_category_list stamps the routing category onto every
        // row (mg/playlist.rs:227-231); the M3 s4 `extra` slot carries it now.
        // The empty-category branch above is the square-recommend page, which
        // upstream never stamps (get_list has no category context).
        for playlist in &mut result {
            playlist
                .extra
                .insert("category_id".to_owned(), category.to_owned());
        }
        Ok(result)
    }
    async fn search_playlists(&self, keyword: &str, page: u32) -> Result<Vec<Playlist>> {
        if keyword.trim().is_empty() {
            return Err(Error::InvalidInput(
                "playlist search keyword is empty".into(),
            ));
        }
        let json = self
            .migu_search_all(
                keyword,
                page.max(1),
                30,
                r#"{"song":0,"album":0,"singer":0,"tagSong":0,"mvSong":0,"songlist":1,"bestShow":1}"#,
            )
            .await?;
        Ok(json["songListResultData"]["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(migu_parse_searched_playlist)
            .collect())
    }
    async fn user_playlists(&self, _page: u32, _page_size: u32) -> Result<Vec<Playlist>> {
        // Migu user playlists need the mobile login session, which this
        // provider does not implement.
        Err(Error::Unsupported(ID.into()))
    }
}

/// Charts over the two rank endpoints (port of voicefox mg/leaderboard.rs).
/// Both are plain anonymous GETs through `fetch_json`, which already sends
/// the byte-identical Referer + `channel: 0146921` + Nexus-6 UA header set;
/// there is no signing or crypto on either endpoint.
#[async_trait]
impl ChartProvider for MiguProvider {
    async fn charts(&self) -> Result<Vec<Chart>> {
        let value = self
            .fetch_json(CHART_RANK_INDEX_URL, "migu chart catalogue")
            .await?;
        parse_rank_index(&value)
    }
    async fn chart_tracks(&self, id: &str) -> Result<Vec<ChartEntry>> {
        if id.trim().is_empty() {
            return Err(Error::InvalidInput("chart id is empty".into()));
        }
        let url = format!(
            "{CHART_SONGS_URL}?columnId={}&needAll=0",
            urlencoding::encode(id.trim())
        );
        let value = self.fetch_json(&url, "migu chart tracks").await?;
        parse_chart_board(&value)
    }
}

/// Albums over the anonymous queryAlbumSong track list (port of voicefox
/// mg/parse.rs fetch_album). Both methods ride one single-page request
/// (pageNo=1&pageSize=100, hardcoded upstream; live pageNo > 1 returns an
/// empty data object, so there is nothing to paginate into): >100-track
/// albums truncate at 100 exactly like upstream, and there is no live
/// sample to verify that edge. Album metadata is derived from the first
/// track plus totalCount (no album-detail endpoint exists upstream).
#[async_trait]
impl AlbumProvider for MiguProvider {
    async fn album(&self, id: &str) -> Result<Album> {
        let id = id.trim();
        if id.is_empty() {
            return Err(Error::InvalidInput("album id is empty".into()));
        }
        let json = self.fetch_album_page(id).await?;
        parse_album_page(&json, id).map(|(album, _)| album)
    }
    async fn album_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let id = id.trim();
        if id.is_empty() {
            return Err(Error::InvalidInput("album id is empty".into()));
        }
        let json = self.fetch_album_page(id).await?;
        parse_album_page(&json, id).map(|(_, songs)| songs)
    }
}

// ── Link resolution (share links; mg/parse.rs) ──────────────────────────
#[async_trait]
impl LinkResolver for MiguProvider {
    /// The three share-link shapes (mg/parse.rs parse, :66-88): a song link
    /// fetches metadata only via the GET resourceinfo.do route (no stream
    /// URL — upstream keeps the key-negotiated play address out of link
    /// parsing, parse.rs:106); a playlist link reuses the playlist-detail
    /// path with the first 50-row page (upstream get_detail_with_meta,
    /// playlist.rs:55-94, never paginates: `pageNo=1&pageSize=50` hardcoded)
    /// and an empty first page is the upstream NotFound (:69-71); an album
    /// link reuses parse_album_page (upstream fetch_album, one 100-row page).
    /// Upstream ParsedLink::Playlist carries playlist+songs and
    /// ParsedLink::Album album+songs; the musid-dli models attach `tracks`
    /// only to Playlist, so the album branch resolves metadata alone (the
    /// songs stay reachable through album_tracks()). The playlist branch
    /// carries the upstream creator/cover chains and the canonical web link
    /// since the M3 s4 slots landed (get_detail_with_meta, mg/playlist.rs:82-92).
    /// The album branch has no equivalent carrier: upstream derives the
    /// album-container creator from the first row's singers (mg/parse.rs:140-147)
    /// and stamps an album web link (:149), and musid-dli's `Album` model has
    /// neither slot — the singer derivation already lands in `Album.artists`
    /// (parse_album_page), the link stays dropped for lack of a model field.
    /// Upstream has no artist branch; unrecognized links are InvalidInput.
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink> {
        match mg_link_target(url)? {
            MgLink::Song { content_id } => Ok(ResolvedLink::Track(
                self.fetch_song_link_info(&content_id).await?,
            )),
            MgLink::Playlist { id } => {
                let page = self.fetch_playlist_page(&id, 1).await?;
                if page.songs.is_empty() {
                    return Err(Error::NotFound);
                }
                // This is upstream get_detail_with_meta (mg/playlist.rs:55-94)
                // one-to-one: creator/cover ride the detail-page chains
                // (:82-91) and the canonical web link rides the result (:92);
                // the M3 s4 slots finally carry what the migu-link-parsing
                // slice had to drop for lack of model slots.
                Ok(ResolvedLink::Playlist(Playlist {
                    id: TrackId::new(ID, id.to_owned()),
                    title: page.title.unwrap_or_else(|| format!("Migu playlist {id}")),
                    description: None,
                    artwork_url: page.cover,
                    track_count: page.total.map(|n| n as u32),
                    tracks: page.songs,
                    play_count: None,
                    creator: page.creator,
                    link: Some(format!("https://music.migu.cn/v3/music/playlist/{id}")),
                    extra: Default::default(),
                }))
            }
            MgLink::Album { id } => {
                let json = self.fetch_album_page(&id).await?;
                let (album, _) = parse_album_page(&json, &id)?;
                Ok(ResolvedLink::Album(album))
            }
        }
    }
}

impl MusicProvider for MiguProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        "咪咕音乐"
    }
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            lyrics: true,
            playlists: true,
            albums: true,
            charts: true,
            link_resolve: true,
            ..Default::default()
        }
    }
}
#[async_trait]
impl TrackSearch for MiguProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.keyword.trim().is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        let page = q.page.max(1);
        let limit = q.page_size.clamp(1, 100);
        let (sign, timestamp) = mg_sign(q.keyword.trim());
        let search_switch = r#"{"song":1,"album":0,"singer":0,"tagSong":1,"mvSong":0,"bestShow":1,"songlist":0,"lyricSong":0}"#;
        let url = format!(
            "{SEARCH_URL}?isCorrect=0&isCopyright=1&searchSwitch={}&pageSize={limit}&text={}&pageNo={page}&sort=0&sid=USS",
            urlencoding::encode(search_switch),
            urlencoding::encode(q.keyword.trim()),
        );
        // Signed search GET through the unified transport layer
        // (`send_raw` + `RetryPolicy::standard()`): declared upgrade from
        // today's single shot (§5.5 rows 1/3); the sign/deviceId/timestamp
        // header set is untouched and `mg_sign` stays provider-side.
        let request = self
            .request(TransportRequest::get(&url))
            .header("uiVersion", "A_music_3.6.1")
            .header("deviceId", SEARCH_DEVICE_ID)
            .header("timestamp", &timestamp)
            .header("sign", &sign)
            .header("channel", "0146921");
        let response = self
            .http
            .send_raw(&request, &RetryPolicy::standard())
            .await
            .map_err(|e| perr(format!("search: {e}")))?;
        let json: Value =
            serde_json::from_slice(&response.body).map_err(|e| perr(format!("search: {e}")))?;
        parse_search_page(&json, page, limit)
    }
}
/// Parse the searchAll response: `code` gate (errors surface `info`),
/// numeric-or-string `totalCount`, the two-dimensional `resultList` groups
/// merged and deduped on `songId:copyrightId`, and
/// `has_more = page * limit < total` (see tests/fixtures/migu/search_response.json).
fn parse_search_page(json: &Value, page: u32, limit: u32) -> Result<SearchPage> {
    if json["code"].as_str().unwrap_or_default() != "000000" {
        return Err(perr(format!(
            "search: {}",
            json["info"].as_str().unwrap_or("unknown error")
        )));
    }
    let total = mg_value_string(&json["songResultData"]["totalCount"])
        .parse()
        .unwrap_or(0);
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for group in json["songResultData"]["resultList"]
        .as_array()
        .into_iter()
        .flatten()
    {
        for item in group.as_array().into_iter().flatten() {
            if let Some(track) = mg_parse_song(item) {
                let key = format!(
                    "{}:{}",
                    track.id.id,
                    track.extra.get("copyrightId").cloned().unwrap_or_default()
                );
                if seen.insert(key) {
                    items.push(track);
                }
            }
        }
    }
    let has_more = u64::from(page) * u64::from(limit) < total;
    Ok(SearchPage {
        items,
        total: Some(total),
        has_more,
    })
}
/// Stream resolution over the unified transport layer.
///
/// Declared behavior upgrades and the resolve-timeout reconciliation
/// (TRANSPORT_DESIGN.md §5.4/§5.5, review P-1) for this slice:
/// - the resource-info POST and the strategy GET join the
///   `RetryPolicy::standard()` ladder (today single-shot): at most 5 sends
///   × 15s + 4 backoffs (0.5+1+2+4 + ≤1s jitter) ≈ 83.5s per call, ≈116s
///   when a server `Retry-After` rides every retry at the 10s cap;
/// - `fetch_listen_url` and the served-size probe are `send_once` single
///   shots and stay outside any ladder (15s each), so a streaming resolve
///   that walks strategy + listenSong + both probe legs has a worst case of
///   ≈ 116 + 116 + 15 + 15 + 15 ≈ 280s. Hosts tuned for today's single-shot
///   migu (≈15s per call) must raise the resolve-level
///   `per_provider_timeout` (resolve.rs) above that — or pass a reduced
///   `transport_retries` — or the ladder is cancelled mid-flight and masked
///   as "provider resolution timed out" plus a circuit-breaker failure.
#[async_trait]
impl StreamResolver for MiguProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to Migu".into()));
        }
        let resource = resource_id(track).ok_or(Error::NotFound)?.to_owned();
        // resource-info POST through the unified transport layer. P-1
        // decision (TRANSPORT_DESIGN.md §5.5 row 3, option two, declared
        // behavior upgrade): the form body is a read-only `resourceId`
        // query, so the slice marks it `.idempotent(true)` — today
        // single-shot, now the identical query body may be resent under
        // `standard()` on transport errors / 429-5xx. Today any non-success
        // status mapped to NotFound; the engine's terminal statuses arrive
        // as `TransportError::Status` — a terminal 429/5xx already burned
        // the whole ladder, so it splits to `Error::Unavailable` while other
        // statuses keep the upstream NotFound mapping (§9.5; the two send
        // error texts keep the `resource info: ...` shape).
        let request = TransportRequest::post(format!("{RESOURCE_INFO_URL}?resourceType=2"))
            .header("user-agent", UA_ANDROID)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("resourceId={resource}"))
            .idempotent(true);
        let response = match self.http.send_raw(&request, &RetryPolicy::standard()).await {
            Ok(response) => response,
            Err(TransportError::Status(text)) => {
                return Err(status_error_from_text("resource info", &text));
            }
            Err(error) => return Err(perr(format!("resource info: {error}"))),
        };
        let json: Value = serde_json::from_slice(&response.body)
            .map_err(|e| perr(format!("resource info parse failed: {e}")))?;
        let Some(resource_obj) = json["resource"].as_array().and_then(|items| items.first()) else {
            return Err(Error::NotFound);
        };
        let formats = resource_obj["newRateFormats"]
            .as_array()
            .ok_or(Error::NotFound)?;
        // Target quality first; the remaining formats become candidate URLs.
        let target_index = formats
            .iter()
            .position(|format| format_type_matches(format, options.quality));
        let mut ordered: Vec<usize> = target_index.into_iter().collect();
        ordered.extend((0..formats.len()).filter(|index| Some(*index) != target_index));
        // Candidates carry the tier and the paired size of the format node
        // they came from: the structured slot (FallbackEndpoint, §2.4) keeps
        // both, so a cross-tier candidate can never inherit the primary's
        // size gate. The legacy string slot mirrors the same URLs in the
        // same order (§2.4 defines the dual fill; cross-slot dedup keeps the
        // first occurrence).
        let mut candidates: Vec<(String, Option<StreamQuality>, Option<u64>)> = Vec::new();
        let mut primary: Option<(String, Option<u64>)> = None;
        for index in ordered {
            let format = &formats[index];
            let tier = format_type_quality(format["formatType"].as_str().unwrap_or_default());
            for variant in FileVariant::ORDER {
                let Some(url) = format_url(format, variant) else {
                    continue;
                };
                // Only the requested tier becomes the primary URL; other tiers
                // stay candidates so quality never silently downgrades first.
                if primary.is_none() && Some(index) == target_index {
                    primary = Some((url.clone(), extract_size(format, variant)));
                }
                if !url.trim().is_empty()
                    && candidates.iter().all(|(existing, _, _)| *existing != url)
                {
                    candidates.push((url, tier, extract_size(format, variant)));
                }
            }
        }
        let fallback_endpoints = candidates
            .iter()
            .map(|(url, quality, size)| FallbackEndpoint {
                url: url.clone(),
                headers: vec![],
                expires_at_unix: None,
                content_length: *size,
                quality: *quality,
            })
            .collect();
        let mut achieved_quality = options.quality;
        let (play_url, declared_size) = match primary {
            Some(resolved) => resolved,
            None => {
                // Current resourceinfo responses usually carry sizes only; ask
                // the H5 strategy endpoint, then the 302 listenSong endpoint.
                let format = target_index
                    .map(|index| &formats[index])
                    .ok_or(Error::NotFound)?;
                let tone_flag = format["formatType"]
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or(Error::NotFound)?
                    .to_owned();
                let resource_type = format["resourceType"]
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(match options.quality {
                        StreamQuality::Lossless | StreamQuality::HiRes => "E",
                        _ => "2",
                    })
                    .to_owned();
                let copyright_id = track
                    .extra
                    .get("copyrightId")
                    .map(String::as_str)
                    .unwrap_or("0");
                let strategy = self
                    .fetch_strategy_url(&resource, copyright_id, &tone_flag, &resource_type)
                    .await;
                let url = match strategy {
                    Ok(url) => url,
                    Err(_) => {
                        self.fetch_listen_url(&resource, &tone_flag, &resource_type)
                            .await?
                    }
                };
                let served_size = self.probe_served_size(&url).await;
                if let Some(size) = served_size
                    && let Some(actual) = quality_for_size(formats, size)
                {
                    achieved_quality = actual;
                }
                (
                    url,
                    served_size.or_else(|| extract_size(format, FileVariant::Android)),
                )
            }
        };
        if explicit_preview_url(&play_url) {
            return Err(perr("stream URL is a marked preview, not complete audio"));
        }
        Ok(StreamInfo {
            url: play_url,
            quality: achieved_quality,
            format: match achieved_quality {
                StreamQuality::Lossless | StreamQuality::HiRes => AudioFormat::Flac,
                _ => AudioFormat::Mp3,
            },
            duration: track.duration,
            expires_at_unix: None,
            content_length: declared_size,
            checksum: None,
            headers: vec![],
            fallback_urls: candidates.iter().map(|(url, _, _)| url.clone()).collect(),
            index_range: None,
            init_range: None,
            max_chunk_size: None,
            size_is_advisory: declared_size.is_some()
                && matches!(
                    options.quality,
                    StreamQuality::Lossless | StreamQuality::HiRes
                )
                && achieved_quality == options.quality,
            checksum_algorithm: None,
            fallback_endpoints,
        })
    }
}
impl MiguProvider {
    async fn fetch_strategy_url(
        &self,
        resource_id: &str,
        copyright_id: &str,
        tone_flag: &str,
        resource_type: &str,
    ) -> Result<String> {
        let url = format!(
            "{STRATEGY_URL}?contentId={resource_id}&copyrightId={copyright_id}&resourceType={resource_type}&netType=01&toneFlag={tone_flag}&scene=&lowerQualityContentId={resource_id}"
        );
        // Live-verified requirements (2026-11-17): the mobile playing-page
        // Referer and the `channel: 0146921` header. Without them the endpoint
        // answers "channel不能为空" / "PE参数格式错误". The GET goes through
        // the unified transport layer (`send_raw` + standard(), declared
        // upgrade): today single-shot with no status gate — the raw body went
        // straight into the decryptor whatever the status. 429/5xx now retry;
        // a terminal non-2xx surfaces as `TransportError::Status` and lands in
        // the same caller fallback as today's decrypt-parse failure
        // (listenSong takes over). The transport buffers the body, so today's
        // separate body-read error collapses into the send error path.
        let request = TransportRequest::get(&url)
            .header("user-agent", UA_IPHONE)
            .header("referer", MOBILE_PLAY_REFERER)
            .header("birth", "h5page")
            .header("signature", "1")
            .header("channel", "0146921");
        let response = self
            .http
            .send_raw(&request, &RetryPolicy::standard())
            .await
            .map_err(|e| perr(format!("strategy request failed: {e}")))?;
        let json = decrypt_strategy_response(&response.body)?;
        json["data"]["url"]
            .as_str()
            .map(str::trim)
            .filter(|url| url.starts_with("http"))
            .map(str::to_owned)
            .ok_or(Error::NotFound)
    }
    /// listenSong.do answers with a 302 whose Location is the CDN link, or a
    /// JSON body with a code (VIP / no rights / bad params, mg/url.rs:285).
    /// The 302-or-refusal contract stays; the refusal classification is
    /// [`listen_refusal_error`] (rights text → `Error::CopyrightRestricted`,
    /// everything else upstream NotFound) and a terminal 429/5xx status
    /// splits to `Error::Unavailable` like the resource POST (§9.5).
    async fn fetch_listen_url(
        &self,
        resource_id: &str,
        tone_flag: &str,
        resource_type: &str,
    ) -> Result<String> {
        let url = format!(
            "{LISTEN_SONG_URL}?toneFlag={tone_flag}&netType=00&userId={MAGIC_USER_ID}&ua=Android_migu&version=5.1&copyrightId=0&contentId={resource_id}&resourceType={resource_type}&channel=0"
        );
        // listenSong 302 harvest through the unified transport layer's
        // single-shot entry (`send_once` on the redirect-none handle,
        // TRANSPORT_DESIGN.md §4.1/§7.9): the 302 IS the success answer and
        // `classify_http` would collapse any 3xx into
        // Err(Status("HTTP 302")) with the response dropped, so this face
        // stays exactly today's one shot — no retry ladder. The Location
        // read keeps today's `to_str().ok()` drop semantics via the
        // buffered accessors (§5.5 row 9).
        let request = TransportRequest::get(&url)
            .header("user-agent", UA_IPHONE)
            .header("referer", LISTEN_SONG_REFERER);
        let response = self
            .listen_http
            .send_once(&request)
            .await
            .map_err(|e| perr(format!("listen url request failed: {e}")))?;
        // Byte-equivalent with today's `status().is_redirection()`
        // (http-1.x: `(300..400).contains(&code)`).
        if (300..400).contains(&response.status) {
            let location = response
                .header("location")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or(Error::NotFound)?
                .to_owned();
            if explicit_preview_url(&location) {
                return Err(Error::NotFound);
            }
            return Ok(location);
        }
        if ladder_exhausted_status(response.status) {
            return Err(terminal_status_error("listen url", response.status));
        }
        Err(listen_refusal_error(&response.body))
    }
    /// Probe the real served size: HEAD first, then a `Range: bytes=0-0` read.
    /// Both legs ride `send_once` on the follow-redirects handle: the probe
    /// measures what the server actually serves and must not be retried
    /// blindly (range semantics), so it stays today's two single shots
    /// (TRANSPORT_DESIGN.md §7.9). Sizes come from headers: the buffered
    /// response has no reqwest-style `content_length()` accessor for a
    /// body-less HEAD, so [`TransportResponse::content_length`] parses the
    /// header (review P-8) and the 206 leg splits `content-range` manually,
    /// exactly like today.
    async fn probe_served_size(&self, url: &str) -> Option<u64> {
        let decorate = |request: TransportRequest| {
            request
                .header("user-agent", UA_IPHONE)
                .header("referer", LISTEN_SONG_REFERER)
        };
        if let Ok(response) = self
            .http
            .send_once(&decorate(TransportRequest::head(url)))
            .await
            && response.is_success()
            && let Some(length) = response.content_length().filter(|length| *length > 0)
        {
            return Some(length);
        }
        let response = self
            .http
            .send_once(&decorate(TransportRequest::get(url)).header("range", "bytes=0-0"))
            .await
            .ok()?;
        if response.status == 206
            && let Some(total) = response
                .header("content-range")
                .and_then(|value| value.rsplit('/').next())
                .and_then(|value| value.trim().parse::<u64>().ok())
        {
            return Some(total);
        }
        response.content_length().filter(|length| *length > 0)
    }
}
#[async_trait]
impl LyricsProvider for MiguProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to Migu".into()));
        }
        // MRC first (word timing); fall back to plain LRC, then to nothing.
        let (original, lines, format) = match track.extra.get("mrcUrl") {
            Some(mrc_url) if !mrc_url.is_empty() => {
                match self
                    .fetch_bytes(mrc_url, "mrc fetch")
                    .await
                    .and_then(|content| decrypt_mrc(&content))
                {
                    Ok(mrc) => match parse_mrc(&mrc) {
                        Some(document) => {
                            return Ok(Some(document));
                        }
                        None => fetch_lrc(self, track).await?,
                    },
                    Err(_) if has_lrc_url(track) => fetch_lrc(self, track).await?,
                    Err(error) => return Err(error),
                }
            }
            _ => fetch_lrc(self, track).await?,
        };
        if original.trim().is_empty() {
            return Ok(None);
        }
        let translation = match track.extra.get("trcUrl") {
            Some(trc_url) if !trc_url.is_empty() => {
                self.fetch_text(trc_url, "trc fetch").await.ok()
            }
            _ => None,
        };
        Ok(Some(LyricDocument {
            format,
            lines,
            original,
            translation,
            romanization: None,
        }))
    }
}
async fn fetch_lrc(
    provider: &MiguProvider,
    track: &Track,
) -> Result<(String, Vec<LyricLine>, LyricFormat)> {
    let original = match track.extra.get("lrcUrl") {
        Some(lrc_url) if !lrc_url.is_empty() => provider.fetch_text(lrc_url, "lrc fetch").await?,
        _ => String::new(),
    };
    let lines = parse_lrc(&original);
    Ok((original, lines, LyricFormat::Lrc))
}
fn has_lrc_url(track: &Track) -> bool {
    track.extra.get("lrcUrl").is_some_and(|url| !url.is_empty())
}
// ── MRC decryption (TEA over UTF-16 words inside layered containers) ─────

const MRC_DELTA: i64 = 2_654_435_769;
const MRC_MIN_LENGTH: usize = 32;
const MRC_MAX_CONTAINER_DEPTH: usize = 4;
const MRC_KEY: [i64; 9] = [
    27_303_562_373_562_475,
    18_014_862_372_307_051,
    22_799_692_160_172_081,
    34_058_940_340_699_235,
    30_962_724_186_095_721,
    27_303_523_720_101_991,
    27_303_523_720_101_998,
    31_244_139_033_526_382,
    28_992_395_054_481_524,
];
fn decrypt_mrc(data: &[u8]) -> Result<String> {
    decode_mrc_container(data, 0)
}
fn decode_mrc_container(data: &[u8], depth: usize) -> Result<String> {
    if data.is_empty() {
        return Ok(String::new());
    }
    if depth > MRC_MAX_CONTAINER_DEPTH {
        return Err(perr("MRC container nesting is too deep"));
    }
    if let Some(text) = decode_text(data)
        && looks_like_mrc(&text)
    {
        return Ok(trim_text_padding(text));
    }
    if let Ok(text) = std::str::from_utf8(data) {
        let compact: Vec<u8> = text
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        if compact.len() >= MRC_MIN_LENGTH
            && compact.len().is_multiple_of(16)
            && compact.iter().all(u8::is_ascii_hexdigit)
        {
            return decrypt_mrc_hex(&compact);
        }
        if let Some(decoded) = decode_base64_container(&compact)
            && let Ok(text) = decode_mrc_container(&decoded, depth + 1)
        {
            return Ok(text);
        }
    }
    for decompressed in decompress_containers(data) {
        if let Ok(text) = decode_mrc_container(&decompressed, depth + 1) {
            return Ok(text);
        }
    }
    decode_text(data)
        .map(trim_text_padding)
        .ok_or_else(|| perr("unsupported MRC container"))
}
fn decrypt_mrc_hex(data: &[u8]) -> Result<String> {
    let mut words: Vec<i64> = data
        .as_chunks::<16>()
        .0
        .iter()
        .map(|chunk| {
            let text = std::str::from_utf8(chunk)
                .map_err(|e| perr(format!("MRC hex chunk invalid: {e}")))?;
            u64::from_str_radix(text, 16)
                .map(|value| value as i64)
                .map_err(|e| perr(format!("MRC hex parse failed: {e}")))
        })
        .collect::<Result<Vec<_>>>()?;
    tea_decrypt(&mut words);
    let utf16: Vec<u16> = words
        .into_iter()
        .flat_map(|word| {
            let bytes = word.to_le_bytes();
            [
                u16::from_le_bytes([bytes[0], bytes[1]]),
                u16::from_le_bytes([bytes[2], bytes[3]]),
                u16::from_le_bytes([bytes[4], bytes[5]]),
                u16::from_le_bytes([bytes[6], bytes[7]]),
            ]
        })
        .collect();
    let mut utf16 = utf16;
    while utf16.last() == Some(&0) {
        utf16.pop();
    }
    String::from_utf16(&utf16)
        .map(trim_text_padding)
        .map_err(|e| perr(format!("MRC utf16 decode failed: {e}")))
}
fn decode_base64_container(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 8
        || !data.len().is_multiple_of(4)
        || !data
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return None;
    }
    std::str::from_utf8(data)
        .ok()
        .and_then(|text| STANDARD.decode(text).ok())
        .filter(|decoded| !decoded.is_empty() && decoded.as_slice() != data)
}
fn decompress_containers(data: &[u8]) -> Vec<Vec<u8>> {
    let mut results = Vec::new();
    if data.starts_with(&[0x1f, 0x8b]) {
        if let Some(decoded) = read_compressed(GzDecoder::new(data)) {
            results.push(decoded);
        }
        return results;
    }
    if data.starts_with(&[0x78])
        && let Some(decoded) = read_compressed(ZlibDecoder::new(data))
    {
        results.push(decoded);
    }
    if let Some(decoded) = read_compressed(DeflateDecoder::new(data))
        && !results.iter().any(|value| value == &decoded)
    {
        results.push(decoded);
    }
    results
}
fn read_compressed(mut reader: impl Read) -> Option<Vec<u8>> {
    let mut decoded = Vec::new();
    reader
        .read_to_end(&mut decoded)
        .ok()
        .filter(|_| !decoded.is_empty())
        .map(|_| decoded)
}
fn decode_text(data: &[u8]) -> Option<String> {
    if data.starts_with(&[0xff, 0xfe]) {
        return decode_utf16(&data[2..], true);
    }
    if data.starts_with(&[0xfe, 0xff]) {
        return decode_utf16(&data[2..], false);
    }
    if let Ok(text) = std::str::from_utf8(data) {
        return Some(text.to_owned());
    }
    if data.len().is_multiple_of(2) {
        let even_zeroes = data.iter().step_by(2).filter(|byte| **byte == 0).count();
        let odd_zeroes = data
            .iter()
            .skip(1)
            .step_by(2)
            .filter(|byte| **byte == 0)
            .count();
        let pairs = data.len() / 2;
        if odd_zeroes > pairs / 4 {
            return decode_utf16(data, true);
        }
        if even_zeroes > pairs / 4 {
            return decode_utf16(data, false);
        }
    }
    None
}
fn decode_utf16(data: &[u8], little_endian: bool) -> Option<String> {
    if !data.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        })
        .collect();
    String::from_utf16(&units).ok()
}
fn looks_like_mrc(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with('[')
            && line
                .get(1..)
                .and_then(|value| value.split_once(','))
                .is_some_and(|(start, _)| start.bytes().all(|byte| byte.is_ascii_digit()))
    })
}
fn trim_text_padding(mut text: String) -> String {
    while text.ends_with('\0') {
        text.pop();
    }
    text
}
fn tea_decrypt(data: &mut [i64]) {
    if data.is_empty() {
        return;
    }
    let length = data.len();
    let mut next = data[0];
    let mut sum = (6i64 + 52i64 / length as i64).wrapping_mul(MRC_DELTA);
    while sum != 0 {
        let key_index = ((sum >> 2) & 3) as usize;
        for index in (1..length).rev() {
            let previous = data[index - 1];
            next = data[index].wrapping_sub(mix(
                previous,
                next,
                sum,
                MRC_KEY[(index & 3) ^ key_index],
            ));
            data[index] = next;
        }
        let previous = data[length - 1];
        next = data[0].wrapping_sub(mix(previous, next, sum, MRC_KEY[key_index]));
        data[0] = next;
        sum = sum.wrapping_sub(MRC_DELTA);
    }
}
fn mix(previous: i64, next: i64, sum: i64, key: i64) -> i64 {
    (next ^ sum).wrapping_add(previous ^ key)
        ^ ((previous >> 5) ^ next.wrapping_shl(2))
            .wrapping_add((next >> 3) ^ previous.wrapping_shl(4))
}
/// Parse MRC text: `[millis,duration]word(start,dur)word(start,dur)` lines.
/// Markers precede their syllable; the trailing text closes the last word.
fn parse_mrc(content: &str) -> Option<LyricDocument> {
    let mut lines = Vec::new();
    let mut plain = Vec::new();
    for value in content.lines() {
        let trimmed = value.trim_start();
        let Some(inner) = trimmed.strip_prefix('[').and_then(|rest| {
            let end = rest.find(']')?;
            Some((&rest[..end], &rest[end + 1..]))
        }) else {
            continue;
        };
        let (header, body) = inner;
        let Some(millis) = header.split(',').next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        // MRC markers trail their syllable: the text before each "(start,dur)"
        // marker is sung at that marker's time. Trailing text after the last
        // marker is untimed and, matching upstream, stays out of the words.
        // Chunks[i] is the text before markers[i]; the final chunk is the
        // untimed trailing text after the last marker.
        let mut markers: Vec<(u64, u64)> = Vec::new();
        let mut chunks: Vec<String> = Vec::new();
        let mut scan = 0usize;
        let mut last_end = 0usize;
        while let Some(open) = body[scan..].find('(') {
            let open_abs = scan + open;
            let after = &body[open_abs + 1..];
            let Some(close_rel) = after.find(')') else {
                break;
            };
            let marker_text = &after[..close_rel];
            let mut parts = marker_text.split(',');
            let start = parts.next().and_then(|v| v.parse::<u64>().ok());
            let duration = parts.next().and_then(|v| v.parse::<u64>().ok());
            match (start, duration) {
                (Some(start), Some(duration)) => {
                    markers.push((start, duration));
                    chunks.push(body[last_end..open_abs].to_owned());
                    last_end = open_abs + 1 + close_rel + 1;
                    scan = last_end;
                }
                _ => {
                    // A parenthesised non-marker, e.g. "(Live)": keep it in
                    // the text and keep scanning past this '('.
                    scan = open_abs + 1;
                }
            }
        }
        chunks.push(body[last_end..].to_owned());
        let text = decode_entities(&chunks.concat());
        let words: Vec<LyricWord> = markers
            .iter()
            .zip(chunks.iter())
            .map(|((start, duration), text)| LyricWord {
                text: decode_entities(text),
                start_ms: *start,
                duration_ms: *duration,
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
        plain.push(format!(
            "[{}]{}",
            format_timestamp(millis),
            lines.last().unwrap().text
        ));
    }
    if lines.is_empty() {
        return None;
    }
    Some(LyricDocument {
        format: LyricFormat::Mrc,
        lines,
        original: plain.join("\n"),
        translation: None,
        romanization: None,
    })
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
fn format_timestamp(millis: u64) -> String {
    let minutes = millis / 60_000;
    let seconds = (millis % 60_000) / 1_000;
    let fraction = millis % 1_000;
    format!("{minutes:02}:{seconds:02}.{fraction:03}")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, MockTransport, TransportResponse};
    use flate2::{Compression, write::ZlibEncoder};
    use serde_json::json;
    use std::io::Write;
    use std::sync::Arc;

    #[test]
    fn mg_sign_is_hex_and_timestamp_is_millis() {
        let (sign, timestamp) = mg_sign("晴天");
        assert_eq!(sign.len(), 32);
        assert!(sign.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(timestamp.len(), 13);
    }
    #[test]
    fn parses_mg_search_items() {
        let item = json!({
            "songId": 3790007,
            "songName": "晴天",
            "singerList": [{"name": "周杰伦"}],
            "album": "叶惠美",
            "albumId": 1000,
            "copyrightId": "60054701923",
            "contentId": "600902000006889366",
            "duration": 269,
            "img3": "https://img.example/c.jpg",
            "audioFormats": [{"formatType": "PQ"}, {"formatType": "SQ"}]
        });
        let track = mg_parse_song(&item).unwrap();
        assert_eq!(track.id.id, "3790007");
        assert_eq!(track.artists, vec!["周杰伦"]);
        assert_eq!(track.album.as_deref(), Some("叶惠美"));
        assert_eq!(
            track.extra.get("contentId").map(String::as_str),
            Some("600902000006889366")
        );
        assert!(track.available_qualities.contains(&StreamQuality::Low));
        assert!(track.available_qualities.contains(&StreamQuality::Lossless));
        assert!(mg_parse_song(&json!({"songName": "x"})).is_none());
    }
    #[test]
    fn duration_parses_seconds_and_mmss_text() {
        assert_eq!(duration_seconds(&json!(269)), 269);
        assert_eq!(duration_seconds(&json!("269")), 269);
        assert_eq!(duration_seconds(&json!("04:29")), 269);
        assert_eq!(duration_seconds(&json!("1:00:01")), 3601);
        assert_eq!(duration_seconds(&json!("")), 0);
    }
    #[test]
    fn size_is_paired_with_the_chosen_url_variant() {
        let format = json!({
            "formatType": "SQ",
            "size": "31529675",
            "androidSize": "31529675",
            "iosSize": "31931278",
            "url": "http://freetyst.nf.migu.cn/a.flac",
            "androidUrl": "http://freetyst.nf.migu.cn/a-android.flac",
            "iosUrl": "http://freetyst.nf.migu.cn/a-ios.m4a"
        });
        assert_eq!(extract_size(&format, FileVariant::Generic), Some(31529675));
        assert_eq!(extract_size(&format, FileVariant::Android), Some(31529675));
        assert_eq!(extract_size(&format, FileVariant::Ios), Some(31931278));
        assert_eq!(
            format_url(&format, FileVariant::Android).as_deref(),
            Some("http://freetyst.nf.migu.cn/a-android.flac")
        );
    }
    #[test]
    fn plain_byte_text_and_human_sizes_are_supported() {
        assert_eq!(parse_size_text("4317311"), Some(4317311));
        assert_eq!(parse_size_text(" 4317311 "), Some(4317311));
        assert_eq!(parse_size_text("3.5MB"), Some(3670016));
        assert_eq!(parse_size_text("1 GB"), Some(1073741824));
        assert_eq!(parse_size_text("0"), None);
        assert_eq!(parse_size_text(""), None);
        assert_eq!(parse_size_text("abc"), None);
    }
    #[test]
    fn flac24_accepts_the_zq24_format_type() {
        let format = json!({"formatType": "ZQ24"});
        assert!(format_type_matches(&format, StreamQuality::HiRes));
        assert!(!format_type_matches(&format, StreamQuality::Lossless));
    }
    #[test]
    fn served_size_maps_back_to_the_real_quality() {
        let formats = vec![
            json!({"formatType": "PQ", "size": "3997991"}),
            json!({"formatType": "HQ", "size": "9994661"}),
            json!({"formatType": "SQ", "size": "30477128", "androidSize": "30477128", "iosSize": "31246068"}),
        ];
        // Anonymous Migu only serves PQ even when HQ/SQ is requested.
        assert_eq!(
            quality_for_size(&formats, 3997991),
            Some(StreamQuality::Low)
        );
        assert_eq!(
            quality_for_size(&formats, 9994661),
            Some(StreamQuality::High)
        );
        assert_eq!(
            quality_for_size(&formats, 30477128),
            Some(StreamQuality::Lossless)
        );
        assert_eq!(quality_for_size(&formats, 12345), None);
    }
    #[test]
    fn preview_urls_are_recognised() {
        assert!(explicit_preview_url(
            "https://cdn.migu.cn/audition/2019/a.mp3"
        ));
        assert!(explicit_preview_url("https://cdn.migu.cn/a.mp3?preview=1"));
        assert!(explicit_preview_url(
            "https://cdn.migu.cn/a.mp3?node_is_preview=true"
        ));
        assert!(!explicit_preview_url("https://cdn.migu.cn/public/a.mp3"));
        assert!(!explicit_preview_url("https://cdn.migu.cn/a.mp3?preview=0"));
        assert!(!explicit_preview_url(""));
    }
    #[test]
    fn strategy_response_decrypts_with_magic_seed_roundtrip() {
        let payload = json!({"data": {"url": "http://cdn/x.mp3"}}).to_string();
        let seed = 0x42u8;
        let mut encrypted = MIGU_MAGIC.to_vec();
        encrypted.push(seed);
        for (index, byte) in payload.as_bytes().iter().enumerate() {
            // Inverse of decrypt: byte' = byte + key - seed (wrapping).
            encrypted.push(
                byte.wrapping_add(MIGU_KEY[index % MIGU_KEY.len()])
                    .wrapping_sub(seed),
            );
        }
        let value = decrypt_strategy_response(&encrypted).unwrap();
        assert_eq!(value["data"]["url"].as_str(), Some("http://cdn/x.mp3"));
        // Plain JSON without the magic prefix passes through.
        let value = decrypt_strategy_response(payload.as_bytes()).unwrap();
        assert_eq!(value["data"]["url"].as_str(), Some("http://cdn/x.mp3"));
    }
    #[test]
    fn decrypts_mrc_containers() {
        let plaintext = "[1000,500]你(1000,250)好(1250,250)\n";
        assert_eq!(decrypt_mrc(plaintext.as_bytes()).unwrap(), plaintext);
        // TEA-encrypted hex container round-trip.
        let mut utf16: Vec<u16> = plaintext.encode_utf16().collect();
        while !utf16.len().is_multiple_of(4) {
            utf16.push(0);
        }
        let mut words: Vec<i64> = utf16
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| {
                i64::from_le_bytes([
                    chunk[0] as u8,
                    (chunk[0] >> 8) as u8,
                    chunk[1] as u8,
                    (chunk[1] >> 8) as u8,
                    chunk[2] as u8,
                    (chunk[2] >> 8) as u8,
                    chunk[3] as u8,
                    (chunk[3] >> 8) as u8,
                ])
            })
            .collect();
        tea_encrypt_fixture(&mut words);
        let hex: String = words
            .into_iter()
            .map(|word| format!("{:016x}", word as u64))
            .collect();
        assert_eq!(decrypt_mrc(hex.as_bytes()).unwrap(), plaintext);
        // Base64 + zlib containers unwrap recursively.
        let encoded = STANDARD.encode(hex.as_bytes());
        assert!(decrypt_mrc(encoded.as_bytes()).is_ok());
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(plaintext.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(decrypt_mrc(&compressed).unwrap(), plaintext);
    }
    fn tea_encrypt_fixture(data: &mut [i64]) {
        let length = data.len();
        let mut previous = data[length - 1];
        let rounds = 6 + 52 / length as i64;
        let mut sum = 0i64;
        for _ in 0..rounds {
            sum = sum.wrapping_add(MRC_DELTA);
            let key_index = ((sum >> 2) & 3) as usize;
            for index in 0..length - 1 {
                let next = data[index + 1];
                previous = data[index].wrapping_add(mix(
                    previous,
                    next,
                    sum,
                    MRC_KEY[(index & 3) ^ key_index],
                ));
                data[index] = previous;
            }
            let next = data[0];
            previous = data[length - 1].wrapping_add(mix(
                previous,
                next,
                sum,
                MRC_KEY[((length - 1) & 3) ^ key_index],
            ));
            data[length - 1] = previous;
        }
    }
    #[test]
    fn parses_mrc_lines_with_word_timing() {
        let document =
            parse_mrc("[61234,1200]你(61234,600)好(61834,600)\n[62434,1200]再(62434,1200)见")
                .unwrap();
        assert_eq!(document.format, LyricFormat::Mrc);
        assert_eq!(document.lines.len(), 2);
        assert_eq!(document.lines[0].text, "你好");
        assert_eq!(document.lines[0].words.len(), 2);
        assert_eq!(document.lines[0].words[0].start_ms, 61_234);
        assert_eq!(document.lines[0].words[0].duration_ms, 600);
        assert_eq!(document.lines[0].words[1].start_ms, 61_834);
        assert_eq!(document.lines[1].words.len(), 1);
        assert_eq!(document.original, "[01:01.234]你好\n[01:02.434]再见");
        assert!(parse_mrc("no timed lines here").is_none());
    }
    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = MiguProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.lyrics);
        assert!(descriptor.capabilities.playlists);
        assert!(descriptor.capabilities.albums);
        assert!(descriptor.capabilities.charts);
        assert!(descriptor.capabilities.link_resolve);
        assert!(!descriptor.capabilities.authentication);
        assert!(registry.lyrics_provider(ID).is_some());
        assert!(registry.stream_provider(ID).is_some());
        assert!(registry.playlist_provider(ID).is_some());
        assert!(registry.collection_provider(ID).is_some());
        assert!(registry.album_provider(ID).is_some());
        assert!(registry.chart_provider(ID).is_some());
        assert!(registry.link_provider(ID).is_some());
    }

    // ── fixture-driven tests (see tests/fixtures/migu/README.md) ─────────

    /// Loads a reconstructed vector/response fixture from tests/fixtures/migu
    /// (see the README there for the derivation of these shapes and vectors).
    macro_rules! fixture {
        ($name:expr) => {
            serde_json::from_str::<Value>(include_str!(concat!("../tests/fixtures/migu/", $name)))
                .expect("migu fixture must be valid JSON")
        };
    }
    fn fixture_hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }
    fn quality_by_name(name: &str) -> StreamQuality {
        match name {
            "Low" => StreamQuality::Low,
            "High" => StreamQuality::High,
            "Lossless" => StreamQuality::Lossless,
            "HiRes" => StreamQuality::HiRes,
            other => panic!("unknown quality name {other}"),
        }
    }

    #[test]
    fn sign_goldens_pin_the_md5_concatenation() {
        let fixture = fixture!("mg_sign.json");
        // The constants double as a pin on the source literals.
        assert_eq!(SEARCH_SIGN_MD5, fixture["salt_md5"].as_str().unwrap());
        assert_eq!(SEARCH_DEVICE_ID, fixture["device_id"].as_str().unwrap());
        for case in fixture["cases"].as_array().unwrap() {
            let keyword = case["keyword"].as_str().unwrap();
            let timestamp = case["timestamp"].as_str().unwrap();
            let sign = mg_sign_with(keyword, timestamp);
            assert_eq!(sign, case["sign"].as_str().unwrap(), "keyword {keyword}");
            assert_eq!(sign.len(), 32);
            assert!(sign.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn mrc_tea_hex_goldens_decrypt_to_their_plaintexts() {
        let fixture = fixture!("mrc_tea_hex.json");
        for case in fixture["cases"].as_array().unwrap() {
            let hex = case["hex"].as_str().unwrap();
            assert_eq!(hex.len() % 16, 0, "container must be whole words");
            let plaintext = case["plaintext"].as_str().unwrap();
            assert_eq!(decrypt_mrc(hex.as_bytes()).unwrap(), plaintext);
        }
    }

    #[test]
    fn parse_mrc_goldens_match_the_upstream_contract() {
        let fixture = fixture!("mrc_parse.json");
        let golden = &fixture["single_line"];
        let document = parse_mrc(golden["original"].as_str().unwrap()).unwrap();
        assert_eq!(document.original, golden["lrc"].as_str().unwrap());
        assert_eq!(document.lines.len(), 1);
        let line = &document.lines[0];
        assert_eq!(line.text, golden["line_text"].as_str().unwrap());
        let words: Vec<(u64, u64, &str)> = line
            .words
            .iter()
            .map(|word| (word.start_ms, word.duration_ms, word.text.as_str()))
            .collect();
        let expected: Vec<(u64, u64, &str)> = golden["words"]
            .as_array()
            .unwrap()
            .iter()
            .map(|word| {
                (
                    word[0].as_u64().unwrap(),
                    word[1].as_u64().unwrap(),
                    word[2].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(words, expected);
        // The lx form: word times become offsets from the line head.
        let lx = format!(
            "[{}]{}",
            format_timestamp(line.start_ms),
            line.words
                .iter()
                .map(|word| format!(
                    "<{},{}>{}",
                    word.start_ms - line.start_ms,
                    word.duration_ms,
                    word.text
                ))
                .collect::<Vec<_>>()
                .join("")
        );
        assert_eq!(lx, golden["lxlyric"].as_str().unwrap());
        // Two lines keep independent word timing.
        let two = &fixture["two_lines"];
        let document = parse_mrc(two["original"].as_str().unwrap()).unwrap();
        assert_eq!(
            document.lines.len(),
            two["line_count"].as_u64().unwrap() as usize
        );
        assert_eq!(document.original, two["lrc"].as_str().unwrap());
        assert_eq!(document.lines[1].words.len(), 1);
        assert_eq!(document.lines[1].words[0].start_ms, 62_434);
        assert_eq!(document.lines[1].words[0].duration_ms, 1_200);
        // No timed line: None hands over to the LRC fallback.
        let none = &fixture["no_timed_lines"];
        assert_eq!(
            parse_mrc(none["original"].as_str().unwrap()).is_some(),
            none["parses"].as_bool().unwrap()
        );
    }

    #[test]
    fn strategy_golden_hex_decrypts_to_the_recorded_payload() {
        let fixture = fixture!("strategy_decrypt.json");
        assert_eq!(
            MIGU_MAGIC,
            fixture_hex(fixture["magic_hex"].as_str().unwrap()).as_slice()
        );
        assert_eq!(MIGU_KEY, fixture["key_ascii"].as_str().unwrap().as_bytes());
        let encrypted = fixture_hex(fixture["encrypted_hex"].as_str().unwrap());
        assert_eq!(encrypted[3], fixture["seed"].as_u64().unwrap() as u8);
        let value = decrypt_strategy_response(&encrypted).unwrap();
        let payload: Value = serde_json::from_str(fixture["payload"].as_str().unwrap()).unwrap();
        assert_eq!(value, payload);
    }

    #[test]
    fn stream_tier_fixture_pins_sizes_text_quality_resource_and_previews() {
        let fixture = fixture!("stream_tiers.json");
        let sq = &fixture["sq_variant_pairing"];
        assert_eq!(extract_size(sq, FileVariant::Generic), Some(31529675));
        assert_eq!(extract_size(sq, FileVariant::Android), Some(31529675));
        assert_eq!(extract_size(sq, FileVariant::Ios), Some(31931278));
        assert_eq!(
            format_url(sq, FileVariant::Android).as_deref(),
            Some("http://freetyst.nf.migu.cn/a-android.flac")
        );
        for case in fixture["size_text_cases"].as_array().unwrap() {
            assert_eq!(
                parse_size_text(case[0].as_str().unwrap()),
                case[1].as_u64(),
                "size text {:?}",
                case[0]
            );
        }
        let tiers = &fixture["quality_for_size"];
        let formats: Vec<Value> = tiers["formats"].as_array().unwrap().clone();
        for case in tiers["cases"].as_array().unwrap() {
            assert_eq!(
                quality_for_size(&formats, case[0].as_u64().unwrap()),
                case[1].as_str().map(quality_by_name),
                "served size {}",
                case[0]
            );
        }
        let zq = &fixture["zq24_format"];
        assert!(format_type_matches(zq, StreamQuality::HiRes));
        assert!(!format_type_matches(zq, StreamQuality::Lossless));
        for case in fixture["resource_id_cases"].as_array().unwrap() {
            let track = mg_parse_song(&case["song"]).unwrap();
            assert_eq!(
                resource_id(&track),
                case["expected"].as_str(),
                "resource id for {:?}",
                case["song"]
            );
        }
        for case in fixture["preview_cases"].as_array().unwrap() {
            assert_eq!(
                explicit_preview_url(case[0].as_str().unwrap()),
                case[1].as_bool().unwrap(),
                "preview {:?}",
                case[0]
            );
        }
    }

    #[test]
    fn search_response_fixture_parses_dedup_totals_and_errors() {
        let fixture = fixture!("search_response.json");
        let ok = &fixture["ok_numeric_total"];
        // page 2 of a 50-per-page window has more pages (2*50 < 113).
        let result = parse_search_page(&ok["response"], 2, 50).unwrap();
        assert_eq!(result.total, Some(113));
        let ids: Vec<&str> = result.items.iter().map(|t| t.id.id.as_str()).collect();
        let expected: Vec<&str> = ok["expected_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(ids, expected);
        // The dedup keeps the first occurrence (the plain title).
        assert_eq!(result.items[0].title, "晴天");
        assert!(result.has_more);
        // page 3 is the last one (3*50 >= 113).
        let result = parse_search_page(&ok["response"], 3, 50).unwrap();
        assert!(!result.has_more);
        // String totals normalize like numeric ones.
        let result = parse_search_page(&fixture["ok_string_total"]["response"], 1, 50).unwrap();
        assert_eq!(result.total, Some(50));
        assert!(!result.has_more);
        // A non-000000 code surfaces the info text.
        let error = parse_search_page(&fixture["error_code"]["response"], 1, 30).unwrap_err();
        match error {
            Error::Provider { message, .. } => {
                assert_eq!(message, "search: 未登录或无权限");
            }
            _ => panic!("expected a provider error for the error_code fixture"),
        }
        // A missing/non-array resultList stays a valid empty page carrying
        // the parsed total (upstream returns total 0 here instead).
        let result = parse_search_page(&fixture["missing_result_list"]["response"], 1, 30).unwrap();
        assert!(result.items.is_empty());
        assert_eq!(result.total, Some(7));
    }

    #[test]
    fn playlist_page_fixture_parses_rows_totals_and_titles() {
        let fixture = fixture!("playlist_pages.json");
        let page = parse_playlist_page(&fixture["page_113_1"]).unwrap();
        assert_eq!(page.title.as_deref(), Some("示例歌单"));
        assert_eq!(page.total, Some(113));
        assert_eq!(page.songs.len(), 50);
        assert_eq!(page.songs[0].id.id, "37900070001");
        assert_eq!(page.songs[0].artists, vec!["示例歌手", "客座歌手"]);
        assert_eq!(
            page.songs[0].available_qualities,
            vec![StreamQuality::Low, StreamQuality::Lossless]
        );
        assert_eq!(
            page.songs[0].extra.get("contentId").map(String::as_str),
            Some("6009020000000000001")
        );
        // Row 2 carries the 顿号-separated plain singer fallback.
        assert_eq!(page.songs[1].artists, vec!["示例歌手", "客座歌手"]);
        // Row 3 carries an "MM:SS" duration text.
        assert_eq!(page.songs[2].duration, Some(Duration::from_secs(240)));
        // M3 s4 fills (mg/playlist.rs:82-91): creator rides the first chain
        // key (userName) and cover the first cover key (imgUrl).
        assert_eq!(page.creator.as_deref(), Some("示例歌主"));
        assert_eq!(
            page.cover.as_deref(),
            Some("https://img.example/cover-113.jpg")
        );
        // The title falls back name -> title -> playlistName.
        let page = parse_playlist_page(&fixture["title_fallback_title"]).unwrap();
        assert_eq!(page.title.as_deref(), Some("备用标题"));
        let page = parse_playlist_page(&fixture["title_fallback_playlist_name"]).unwrap();
        assert_eq!(page.title.as_deref(), Some("歌单名标题"));
        // The creator/cover chains land on their second keys when the first
        // is absent (ownerName / picUrl).
        assert_eq!(page.creator.as_deref(), Some("备用歌主"));
        assert_eq!(
            page.cover.as_deref(),
            Some("https://img.example/cover-fb.jpg")
        );
        // The find-then-filter quirk (upstream keeps one filter after the
        // first string hit): a whitespace userName voids the whole creator
        // chain even though ownerName/nickName carry names, and an empty
        // picUrl voids the cover chain without falling through to cover.
        let page = parse_playlist_page(&fixture["creator_blank_first_hit"]).unwrap();
        assert_eq!(page.creator, None);
        assert_eq!(page.cover, None);
        // String totalCount normalizes.
        let page = parse_playlist_page(&fixture["string_total"]).unwrap();
        assert_eq!(page.total, Some(113));
        // An empty or missing songList is a valid empty page; upstream would
        // error on the missing variant.
        let page = parse_playlist_page(&fixture["empty_songlist"]).unwrap();
        assert!(page.songs.is_empty());
        let page = parse_playlist_page(&fixture["missing_songlist"]).unwrap();
        assert!(page.songs.is_empty());
        assert_eq!(page.total, Some(5));
        // Non-000000 code fails the page.
        assert!(parse_playlist_page(&fixture["code_error"]).is_err());
    }

    #[test]
    fn playlist_pagination_fixture_walks_the_boundary() {
        let fixture = fixture!("playlist_pages.json");
        let pages = [
            &fixture["page_113_1"],
            &fixture["page_113_2"],
            &fixture["page_113_3"],
        ];
        let mut all = Vec::new();
        let mut fetched = 0usize;
        for (index, value) in pages.iter().enumerate() {
            fetched += 1;
            let page = parse_playlist_page(value).unwrap();
            let page_empty = page.songs.is_empty();
            all.extend(page.songs);
            if playlist_page_done(page_empty, all.len(), page.total, (index + 1) as u32) {
                break;
            }
        }
        // 113 tracks at 50 per page: page 3 completes the walk.
        assert_eq!(fetched, 3);
        assert_eq!(all.len(), 113);
        let ids: std::collections::HashSet<&str> =
            all.iter().map(|track| track.id.id.as_str()).collect();
        assert_eq!(ids.len(), 113);
        // A 50-total playlist stops right after the first full page.
        assert!(playlist_page_done(false, 50, Some(50), 1));
        // 113-total boundary: continue at 50/100 fetched, stop at 113.
        assert!(!playlist_page_done(false, 50, Some(113), 1));
        assert!(!playlist_page_done(false, 100, Some(113), 2));
        assert!(playlist_page_done(false, 113, Some(113), 3));
        // An empty page stops regardless of the remaining total.
        assert!(playlist_page_done(true, 50, Some(113), 1));
        // The defensive 40-page cap (upstream has none).
        assert!(playlist_page_done(false, 2000, Some(4000), 40));
        assert!(!playlist_page_done(false, 2000, Some(4000), 39));
        // A missing total keeps walking until an empty page (upstream stops
        // after the first non-empty page instead).
        assert!(!playlist_page_done(false, 50, None, 1));
    }

    // ── M3 s4 collection-field producers (MODEL_ALIGNMENT_DESIGN.md §5.1) ──

    /// Searched playlists carry the upstream parse_searched_playlist fills
    /// (mg/playlist.rs:147-154): play_count from numeric-or-string playNum,
    /// creator from the userName/ownerName chain with the find-then-filter
    /// quirk, and the canonical web link. Pin data:
    /// tests/fixtures/migu/search_response.json#playlist_search.
    #[test]
    fn searched_playlist_fixture_pins_play_count_creator_and_link() {
        let fixture = fixture!("search_response.json");
        let case = &fixture["playlist_search"];
        let rows = case["response"]["songListResultData"]["result"]
            .as_array()
            .unwrap();
        // The empty-id row is dropped, leaving the three expected playlists.
        let parsed: Vec<Playlist> = rows
            .iter()
            .filter_map(migu_parse_searched_playlist)
            .collect();
        assert_eq!(parsed.len(), 3);
        for (playlist, expected) in parsed.iter().zip(case["expected"].as_array().unwrap()) {
            assert_eq!(playlist.id.id, expected["id"].as_str().unwrap());
            assert_eq!(playlist.title, expected["title"].as_str().unwrap());
            assert_eq!(playlist.artwork_url.as_deref(), expected["cover"].as_str());
            assert_eq!(
                playlist.track_count,
                expected["track_count"].as_u64().map(|n| n as u32)
            );
            assert_eq!(playlist.play_count, expected["play_count"].as_u64());
            assert_eq!(playlist.creator.as_deref(), expected["creator"].as_str());
            assert_eq!(playlist.link.as_deref(), expected["link"].as_str());
            assert!(
                playlist.extra.is_empty(),
                "the stamp lives on the category branch"
            );
        }
    }

    /// The category branch rides the same searched-playlist parser and stamps
    /// the routing category onto every row's `extra` (upstream
    /// get_category_list, mg/playlist.rs:227-231).
    #[tokio::test]
    async fn category_playlists_stamp_category_id_and_carry_the_searched_fills() {
        let (provider, business, listen) = mocked(
            MockTransport::script([Ok(json_body(
                200,
                r#"{"code":"000000","songListResultData":{"result":[{"id":"135423084","name":"示例搜索歌单","musicNum":"12","playNum":34567,"userName":"搜索歌主"}]}}"#,
            ))]),
            MockTransport::single(json_body(200, "{}")),
        );
        let lists = provider.playlists("流行", 1, 30).await.unwrap();
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].play_count, Some(34567));
        assert_eq!(lists[0].creator.as_deref(), Some("搜索歌主"));
        assert_eq!(
            lists[0].link.as_deref(),
            Some("https://music.migu.cn/v3/music/playlist/135423084")
        );
        assert_eq!(
            lists[0].extra.get("category_id").map(String::as_str),
            Some("流行")
        );
        let recorded = business.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0].url.contains("songlist"),
            "the category branch searches playlists: {}",
            recorded[0].url
        );
        assert!(listen.recorded().is_empty());
    }

    /// The playlist() face threads the detail-page creator/cover chains and
    /// the canonical web link (upstream get_detail_with_meta,
    /// mg/playlist.rs:82-92); play_count stays None because the detail
    /// endpoint declares none and upstream fills none.
    #[tokio::test]
    async fn playlist_face_carries_creator_cover_and_link_from_the_detail_page() {
        let (provider, business, listen) = mocked(
            MockTransport::script([Ok(json_body(
                200,
                r#"{"code":"000000","data":{"name":"示例歌单","userName":"示例歌主","imgUrl":"https://img.example/cover-113.jpg","totalCount":1,"songList":[{"songId":37900070001,"songName":"示例歌曲1","singerList":[{"name":"示例歌手"}]}]}}"#,
            ))]),
            MockTransport::single(json_body(200, "{}")),
        );
        let playlist = provider.playlist("135423084").await.unwrap();
        assert_eq!(playlist.creator.as_deref(), Some("示例歌主"));
        assert_eq!(
            playlist.artwork_url.as_deref(),
            Some("https://img.example/cover-113.jpg")
        );
        assert_eq!(
            playlist.link.as_deref(),
            Some("https://music.migu.cn/v3/music/playlist/135423084")
        );
        assert_eq!(playlist.play_count, None);
        assert_eq!(playlist.track_count, Some(1));
        assert_eq!(playlist.id.id, "135423084");
        assert!(playlist.extra.is_empty());
        assert_eq!(business.recorded().len(), 1);
        assert!(listen.recorded().is_empty());
    }

    #[test]
    fn rank_index_synthetic_vector_pins_desc_inheritance_and_skips() {
        let fixture = fixture!("chart_rank_index.json");
        let case = &fixture["synthetic_desc_inheritance"];
        let boards = parse_rank_index(&case["response"]).unwrap();
        let actual: Vec<(&str, &str, Option<&str>, Option<&str>)> = boards
            .iter()
            .map(|board| {
                (
                    board.id.id.as_str(),
                    board.title.as_str(),
                    board.description.as_deref(),
                    board.updated_description.as_deref(),
                )
            })
            .collect();
        let expected: Vec<(&str, &str, Option<&str>, Option<&str>)> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|board| {
                let desc = board["description"].as_str();
                // Upstream names this text slot `update` (mg/leaderboard.rs:111),
                // so the M3 updated_description slot carries the same desc.
                (
                    board["id"].as_str().unwrap(),
                    board["title"].as_str().unwrap(),
                    desc,
                    desc,
                )
            })
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn rank_index_live_shape_yields_nine_boards_with_inherited_descs() {
        let fixture = fixture!("chart_rank_index.json");
        let case = &fixture["live_shape_2026_11_18"];
        let boards = parse_rank_index(&case["response"]).unwrap();
        let ids: Vec<&str> = boards.iter().map(|b| b.id.id.as_str()).collect();
        let expected_ids: Vec<&str> = case["expected_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(ids, expected_ids);
        let titles: Vec<&str> = boards.iter().map(|b| b.title.as_str()).collect();
        let expected_titles: Vec<&str> = case["expected_titles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(titles, expected_titles);
        // Only the 咪咕尖叫榜 subtree carries the hourly-update desc; the five
        // 咪咕特色榜 boards have none (description stays None).
        let hourly: std::collections::HashSet<&str> = case["hourly_updated_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        for board in &boards {
            assert_eq!(
                board.description.as_deref() == Some("每小时更新"),
                hourly.contains(board.id.id.as_str()),
                "desc for board {} ({:?})",
                board.id.id,
                board.description
            );
            // The same desc chain feeds the M3 updated_description slot
            // (upstream LeaderboardInfo.update, mg/leaderboard.rs:111).
            assert_eq!(
                board.updated_description.as_deref(),
                board.description.as_deref()
            );
        }
    }

    #[test]
    fn chart_error_gates_surface_the_info_text() {
        let index = fixture!("chart_rank_index.json");
        let error = parse_rank_index(&index["error_gate"]).unwrap_err();
        match error {
            Error::Provider { message, .. } => assert_eq!(message, "找不到资源"),
            _ => panic!("expected a provider error for the rank index error gate"),
        }
        // Missing info falls back to the fixed upstream text.
        let error = parse_rank_index(&index["error_gate_missing_info"]).unwrap_err();
        match error {
            Error::Provider { message, .. } => assert_eq!(message, "咪咕榜单目录请求失败"),
            _ => panic!("expected the fixed rank index fallback text"),
        }
        let board = fixture!("chart_board.json");
        let error = parse_chart_board(&board["error_gate"]).unwrap_err();
        match error {
            Error::Provider { message, .. } => assert_eq!(message, "找不到资源"),
            _ => panic!("expected a provider error for the board error gate"),
        }
    }

    #[test]
    fn rank_index_empty_contents_is_an_upstream_error() {
        let fixture = fixture!("chart_rank_index.json");
        let error = parse_rank_index(&fixture["empty_contents"]).unwrap_err();
        match error {
            Error::Provider { message, .. } => assert_eq!(message, "咪咕未返回可用榜单"),
            _ => panic!("expected a provider error for empty contents"),
        }
    }

    #[test]
    fn chart_board_fixture_pins_ranks_fields_and_drops() {
        let fixture = fixture!("chart_board.json");
        let entries = parse_chart_board(&fixture["ok"]).unwrap();
        // Entry 4 (missing songId) and entry 6 (objectInfo as a JSON string)
        // drop; ranks renumber 1..N over the retained items in array order.
        assert_eq!(entries.len(), 4);
        let ranks: Vec<u32> = entries.iter().map(|entry| entry.rank).collect();
        assert_eq!(ranks, vec![1, 2, 3, 4]);
        let ids: Vec<&str> = entries
            .iter()
            .map(|entry| entry.track.id.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["1142811892", "900000000002", "900000000003", "900000000005"]
        );
        // Live entry 1 golden (trimmed capture of board 83048887, 2026-11-18).
        let first = &entries[0].track;
        assert_eq!(first.title, "甲乙丙丁 (你我怎么两清)");
        assert_eq!(first.artists, vec!["李佳薇"]);
        assert_eq!(first.album.as_deref(), Some("甲乙丙丁"));
        // duration is absent; length "00:03:30" parses to 210 seconds.
        assert_eq!(first.duration, Some(Duration::from_secs(210)));
        assert_eq!(
            first.available_qualities,
            vec![
                StreamQuality::Low,
                StreamQuality::High,
                StreamQuality::Lossless,
                StreamQuality::HiRes
            ]
        );
        assert_eq!(
            first.artwork_url.as_deref(),
            Some(
                "https://d.musicapp.migu.cn/data/oss/resource/00/5x/2z/42cb740d6ce443aaa6abaa570c864815.webp"
            )
        );
        assert_eq!(
            first.extra.get("copyrightId").map(String::as_str),
            Some("6005974L6ZL")
        );
        assert_eq!(
            first.extra.get("contentId").map(String::as_str),
            Some("600929000008420331")
        );
        assert_eq!(
            first.extra.get("lrcUrl").map(String::as_str),
            Some(
                "https://d.musicapp.migu.cn/data/oss/resource/00/5x/9i/0849dd8352e043c9b01526991276cff9"
            )
        );
        assert_eq!(
            first.extra.get("mrcUrl").map(String::as_str),
            Some(
                "https://d.musicapp.migu.cn/data/oss/resource/00/5x/9i/3d4ff21bce78416aa96864350dfb7f32"
            )
        );
        // The empty trcUrl never enters extra.
        assert!(!first.extra.contains_key("trcUrl"));
        // Array position, not relationSort, is the rank source: entry 3 sits
        // at array position 3 while carrying relationSort 68.
        assert_eq!(entries[2].track.id.id, "900000000003");
        assert_eq!(entries[2].rank, 3);
    }

    #[test]
    fn chart_board_missing_contents_is_an_upstream_error() {
        let fixture = fixture!("chart_board.json");
        let error = parse_chart_board(&fixture["missing_contents"]).unwrap_err();
        match error {
            Error::Provider { message, .. } => assert_eq!(message, "咪咕榜单歌曲列表为空"),
            _ => panic!("expected a provider error for missing contents"),
        }
    }

    #[test]
    fn album_page_fixture_parses_rows_metadata_and_traps() {
        let fixture = fixture!("album_songs.json");
        let (album, songs) = parse_album_page(&fixture["ok"]["response"], "10927").unwrap();
        assert_eq!(songs.len(), 12);
        assert_eq!(album.id.id, "10927");
        // There is no album-detail endpoint upstream: the metadata is derived
        // from the first track (title from its album field, artists and cover
        // from the row) plus the totalCount (mg/parse.rs fetch_album).
        assert_eq!(album.title, "逆光");
        assert_eq!(album.artists, vec!["孙燕姿"]);
        assert_eq!(album.track_count, Some(12));
        assert!(
            album
                .artwork_url
                .as_deref()
                .unwrap_or_default()
                .ends_with(".webp")
        );
        // Live row 0 golden (trimmed capture of album 10927, 2026-11-18):
        // string songId, no duration key (length "hh:mm:ss" carries the
        // duration), PQ/HQ/SQ formats, the cover only from albumImgs[0].img,
        // artists with singerList absent, empty-string mrcUrl/trcUrl dropped
        // from extra and no lowercase mrcurl, alphanumeric copyrightId,
        // numeric contentId, and the row-level albumId kept on the track.
        let first = &songs[0];
        assert_eq!(first.id.id, "1001464322");
        assert_eq!(first.title, "In the Beginning");
        assert_eq!(first.duration, Some(Duration::from_secs(59)));
        assert_eq!(first.artists, vec!["孙燕姿"]);
        assert_eq!(first.album.as_deref(), Some("逆光"));
        assert_eq!(first.album_id.as_deref(), Some("10927"));
        assert_eq!(
            first.available_qualities,
            vec![
                StreamQuality::Low,
                StreamQuality::High,
                StreamQuality::Lossless
            ]
        );
        assert_eq!(
            first.artwork_url.as_deref(),
            Some(
                "https://d.musicapp.migu.cn/data/oss/resource/00/5t/0w/679316c4e8bb47a7bedc10b5e0384068.webp"
            )
        );
        assert_eq!(
            first.extra.get("copyrightId").map(String::as_str),
            Some("6005751F411")
        );
        assert_eq!(
            first.extra.get("contentId").map(String::as_str),
            Some("600910000003611567")
        );
        assert!(
            first
                .extra
                .get("lrcUrl")
                .map(String::as_str)
                .unwrap_or_default()
                .starts_with("https://")
        );
        // The empty-string mrcUrl/trcUrl keys never enter extra.
        assert!(!first.extra.contains_key("mrcUrl"));
        assert!(!first.extra.contains_key("trcUrl"));
        // A later row carries ZQ → HiRes.
        assert!(songs[1].available_qualities.contains(&StreamQuality::HiRes));
        // A string totalCount normalizes like the numeric one.
        let (album, _) =
            parse_album_page(&fixture["string_total_count"]["response"], "10927").unwrap();
        assert_eq!(album.track_count, Some(12));
        // The live error path: an invalid albumId (and pageNo > 1 alike)
        // answers code 000000 with an empty data object, so the code gate
        // never fires and NotFound is derived from the empty songList.
        let error =
            parse_album_page(&fixture["empty_data"]["response"], "999999999999").unwrap_err();
        assert!(matches!(error, Error::NotFound));
        // The defensive code gate is musid-dli convention; upstream
        // fetch_album has none (see the fixture derivation).
        assert!(parse_album_page(&fixture["code_error"]["response"], "10927").is_err());
    }

    // --- link-parsing fixtures: the three share-link shapes (mg/parse.rs,
    // audit 2026-11-18), see tests/fixtures/migu/link_targets.json.

    #[test]
    fn link_target_fixtures_follow_upstream_parse() {
        let fixture = fixture!("link_targets.json");
        for case in fixture["positive"].as_array().unwrap() {
            let link = case["link"].as_str().unwrap();
            let target = mg_link_target(link)
                .unwrap_or_else(|e| panic!("link {link} should be recognized: {e}"));
            let expected_kind = case["kind"].as_str().unwrap();
            let expected_id = case["id"].as_str().unwrap();
            match (&target, expected_kind) {
                (MgLink::Song { content_id }, "song") => assert_eq!(
                    content_id, expected_id,
                    "link {link} should target song {expected_id}"
                ),
                (MgLink::Playlist { id }, "playlist") => assert_eq!(
                    id, expected_id,
                    "link {link} should target playlist {expected_id}"
                ),
                (MgLink::Album { id }, "album") => assert_eq!(
                    id, expected_id,
                    "link {link} should target album {expected_id}"
                ),
                _ => panic!("link {link} should target {expected_kind}"),
            }
        }
        for link in fixture["rejected"].as_array().unwrap() {
            let link = link.as_str().unwrap();
            assert!(
                matches!(mg_link_target(link), Err(Error::InvalidInput(_))),
                "link {link} should be rejected"
            );
        }
    }

    // ── transport-layer migration tests (slice migu, TRANSPORT_DESIGN.md
    // §7.9): every migrated HTTP face rides the unified layer — the GET
    // family plus resource POST and strategy GET through `send_raw` with the
    // declared standard() upgrade, the listen 302 harvest and the served-size
    // probe through `send_once` (never retried). Mocks stand in for the two
    // ClientProfile handles (business follow-redirects, listen redirect-none).

    fn json_body(status: u16, body: &str) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: body.as_bytes().to_vec(),
        }
    }
    fn bytes_body(status: u16, body: Vec<u8>) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body,
        }
    }
    fn header_value<'a>(request: &'a TransportRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
    /// Provider over two shared mocks: (business handle, listen handle).
    fn mocked(
        business: MockTransport,
        listen: MockTransport,
    ) -> (MiguProvider, Arc<MockTransport>, Arc<MockTransport>) {
        let business = Arc::new(business);
        let listen = Arc::new(listen);
        let provider = MiguProvider::with_transports(
            Http::from_shared_mock(business.clone()),
            Http::from_shared_mock(listen.clone()),
        );
        (provider, business, listen)
    }
    fn stream_track() -> Track {
        let mut extra = BTreeMap::new();
        extra.insert("contentId".into(), "600902000006889366".into());
        extra.insert("copyrightId".into(), "60054701923".into());
        Track {
            id: TrackId::new(ID, "3790007"),
            title: "晴天".into(),
            artists: vec!["周杰伦".into()],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra,
        }
    }
    /// Encryption inverse of `decrypt_strategy_response` (same shape as the
    /// `strategy_response_decrypts_with_magic_seed_roundtrip` unit test).
    fn encrypt_strategy_payload(payload: &str, seed: u8) -> Vec<u8> {
        let mut encrypted = MIGU_MAGIC.to_vec();
        encrypted.push(seed);
        for (index, byte) in payload.as_bytes().iter().enumerate() {
            encrypted.push(
                byte.wrapping_add(MIGU_KEY[index % MIGU_KEY.len()])
                    .wrapping_sub(seed),
            );
        }
        encrypted
    }
    /// resourceinfo answer whose formats carry sizes but no URLs: pushes
    /// resolve into the strategy → listenSong → probe fallback chain.
    const SIZES_ONLY_FORMATS: &str =
        r#"{"resource":[{"newRateFormats":[{"formatType":"HQ","size":"9994661"}]}]}"#;

    #[tokio::test]
    async fn fetch_json_sends_the_lyric_header_set_and_retries_503() {
        let (provider, business, listen) = mocked(
            MockTransport::script([
                Ok(json_body(503, "")),
                Ok(json_body(
                    200,
                    r#"{"code":"000000","data":{"contents":[]}}"#,
                )),
            ]),
            MockTransport::single(json_body(200, "{}")),
        );
        let value = provider
            .fetch_json(CHART_RANK_INDEX_URL, "migu chart catalogue")
            .await
            .unwrap();
        assert_eq!(value["code"], "000000");
        let recorded = business.recorded();
        assert_eq!(
            recorded.len(),
            2,
            "declared upgrade: the GET family retries the 503 under standard()"
        );
        let request = &recorded[0];
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.url, CHART_RANK_INDEX_URL);
        assert!(request.idempotent, "GET defaults to idempotent");
        assert_eq!(header_value(request, "user-agent"), Some(LYRIC_UA));
        assert_eq!(header_value(request, "referer"), Some(LYRIC_REFERER));
        assert_eq!(header_value(request, "channel"), Some("0146921"));
        assert!(
            listen.recorded().is_empty(),
            "charts never touch the listen handle"
        );
    }

    #[tokio::test]
    async fn search_sends_the_signed_header_set() {
        let (provider, business, listen) = mocked(
            MockTransport::new(|request| {
                assert_eq!(request.method, HttpMethod::Get);
                assert!(
                    request.url.starts_with(SEARCH_URL),
                    "unexpected search url: {}",
                    request.url
                );
                assert_eq!(header_value(request, "uiVersion"), Some("A_music_3.6.1"));
                assert_eq!(header_value(request, "deviceId"), Some(SEARCH_DEVICE_ID));
                assert_eq!(header_value(request, "channel"), Some("0146921"));
                assert_eq!(
                    header_value(request, "user-agent"),
                    Some(UA_MOBILE),
                    "request() adds the mobile UA"
                );
                assert_eq!(
                    header_value(request, "timestamp").unwrap_or_default().len(),
                    13,
                    "millisecond timestamp echoed in a header"
                );
                assert_eq!(header_value(request, "sign").unwrap_or_default().len(), 32);
                Ok(json_body(
                    200,
                    r#"{"code":"000000","songResultData":{"totalCount":0,"resultList":[]}}"#,
                ))
            }),
            MockTransport::single(json_body(200, "{}")),
        );
        let page = provider.search(&SearchQuery::new("晴天")).await.unwrap();
        assert_eq!(page.total, Some(0));
        assert!(!page.has_more);
        assert_eq!(business.recorded().len(), 1, "single send: 2xx is terminal");
        assert!(listen.recorded().is_empty());
    }

    #[tokio::test]
    async fn playlist_page_keeps_the_duplicated_ua_header_set() {
        let (provider, business, listen) = mocked(
            MockTransport::script([Ok(json_body(
                200,
                r#"{"code":"000000","data":{"name":"示例歌单","songList":[],"totalCount":0}}"#,
            ))]),
            MockTransport::single(json_body(200, "{}")),
        );
        let page = provider.fetch_playlist_page("135423084", 1).await.unwrap();
        assert_eq!(page.title.as_deref(), Some("示例歌单"));
        assert!(page.songs.is_empty());
        assert_eq!(page.total, Some(0));
        let recorded = business.recorded();
        assert_eq!(recorded.len(), 1);
        let request = &recorded[0];
        assert!(request.url.starts_with(PLAYLIST_SONG_URL));
        // Today `request()` appended the mobile UA first and the page call
        // appended the playlist UA afterwards, both sent (reqwest header
        // append). The transport applies `TransportRequest.headers` in the
        // same build order, so the duplication carries over verbatim.
        let user_agents: Vec<&str> = request
            .headers
            .iter()
            .filter(|(key, _)| key == "user-agent")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(user_agents, vec![UA_MOBILE, PLAYLIST_UA]);
        assert_eq!(
            header_value(request, "referer"),
            Some("https://m.music.migu.cn/")
        );
        assert_eq!(header_value(request, "channel"), Some("0146921"));
        assert!(listen.recorded().is_empty());
    }

    #[tokio::test]
    async fn resource_post_is_idempotent_and_retries_503() {
        let (provider, business, listen) = mocked(
            MockTransport::script([
                Ok(json_body(503, "")),
                Ok(json_body(
                    200,
                    r#"{"resource":[{"newRateFormats":[{"formatType":"HQ","size":"9994661","url":"http://freetyst.nf.migu.cn/hq.mp3","androidUrl":"http://freetyst.nf.migu.cn/hq-android.mp3","androidSize":"9994661"}]}]}"#,
                )),
            ]),
            MockTransport::single(json_body(200, "{}")),
        );
        let info = provider
            .resolve(&stream_track(), &ResolveOptions::default())
            .await
            .unwrap();
        // The requested HQ tier wins the primary URL; no strategy, listen or
        // probe traffic because the format carries URLs.
        assert_eq!(info.url, "http://freetyst.nf.migu.cn/hq.mp3");
        assert_eq!(info.quality, StreamQuality::High);
        assert_eq!(info.content_length, Some(9_994_661));
        // Structured slot (MODEL_ALIGNMENT_DESIGN.md §2.4): every candidate
        // keeps its format tier and the size paired with its own URL variant
        // (androidUrl pairs with androidSize). The legacy string slot mirrors
        // the same URLs in the same order.
        assert_eq!(
            info.fallback_urls,
            vec![
                "http://freetyst.nf.migu.cn/hq.mp3".to_owned(),
                "http://freetyst.nf.migu.cn/hq-android.mp3".to_owned(),
            ]
        );
        assert_eq!(info.fallback_endpoints.len(), 2);
        assert_eq!(
            info.fallback_endpoints[0].url,
            "http://freetyst.nf.migu.cn/hq.mp3"
        );
        assert_eq!(
            info.fallback_endpoints[0].quality,
            Some(StreamQuality::High)
        );
        assert_eq!(info.fallback_endpoints[0].content_length, Some(9_994_661));
        assert_eq!(
            info.fallback_endpoints[1].url,
            "http://freetyst.nf.migu.cn/hq-android.mp3"
        );
        assert_eq!(
            info.fallback_endpoints[1].quality,
            Some(StreamQuality::High)
        );
        assert_eq!(info.fallback_endpoints[1].content_length, Some(9_994_661));
        assert_eq!(info.ordered_fallback_urls().len(), 2, "cross-slot dedup");
        let recorded = business.recorded();
        assert_eq!(
            recorded.len(),
            2,
            "P-1 declared upgrade: the read-only query POST is resent on 503"
        );
        for request in &recorded {
            assert_eq!(request.method, HttpMethod::Post);
            assert!(
                request.idempotent,
                ".idempotent(true) declared at the call site"
            );
            assert!(
                request.url.starts_with(RESOURCE_INFO_URL)
                    && request.url.ends_with("resourceType=2")
            );
            assert_eq!(
                request.body.as_deref(),
                Some(&b"resourceId=600902000006889366"[..])
            );
            assert_eq!(header_value(request, "user-agent"), Some(UA_ANDROID));
            assert_eq!(
                header_value(request, "content-type"),
                Some("application/x-www-form-urlencoded")
            );
        }
        assert!(listen.recorded().is_empty());
    }

    #[tokio::test]
    async fn resolve_carries_cross_tier_candidates_with_their_own_metadata() {
        // Cross-tier candidate construction (MODEL_ALIGNMENT_DESIGN.md §2.4):
        // the requested SQ tier wins the primary; the remaining HQ format
        // stays a candidate carrying its own tier and size in the structured
        // slot — a candidate must never inherit the primary's size gate.
        let (provider, _business, listen) = mocked(
            MockTransport::script([Ok(json_body(
                200,
                r#"{"resource":[{"newRateFormats":[
                    {"formatType":"SQ","size":"31529675","url":"http://freetyst.nf.migu.cn/sq.flac"},
                    {"formatType":"HQ","size":"9994661","url":"http://freetyst.nf.migu.cn/hq.mp3"}
                ]}]}"#,
            ))]),
            MockTransport::single(json_body(200, "{}")),
        );
        let options = ResolveOptions {
            quality: StreamQuality::Lossless,
        };
        let info = provider.resolve(&stream_track(), &options).await.unwrap();
        assert_eq!(info.url, "http://freetyst.nf.migu.cn/sq.flac");
        assert_eq!(info.quality, StreamQuality::Lossless);
        assert_eq!(info.fallback_urls.len(), 2);
        assert_eq!(info.fallback_endpoints.len(), 2);
        assert_eq!(
            info.fallback_endpoints[0].url,
            "http://freetyst.nf.migu.cn/sq.flac"
        );
        assert_eq!(
            info.fallback_endpoints[0].quality,
            Some(StreamQuality::Lossless)
        );
        assert_eq!(info.fallback_endpoints[0].content_length, Some(31_529_675));
        assert_eq!(
            info.fallback_endpoints[1].url,
            "http://freetyst.nf.migu.cn/hq.mp3"
        );
        assert_eq!(
            info.fallback_endpoints[1].quality,
            Some(StreamQuality::High)
        );
        assert_eq!(info.fallback_endpoints[1].content_length, Some(9_994_661));
        assert!(
            listen.recorded().is_empty(),
            "both formats carry URLs, no strategy/listen traffic"
        );
    }

    #[tokio::test]
    async fn resource_post_status_error_maps_to_not_found() {
        let (provider, business, _listen) = mocked(
            MockTransport::script([Ok(json_body(404, ""))]),
            MockTransport::single(json_body(200, "{}")),
        );
        let error = provider
            .resolve(&stream_track(), &ResolveOptions::default())
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::NotFound),
            "today's non-success mapping carries over for statuses outside the \
             429/5xx ladder family, got: {error}"
        );
        assert_eq!(
            business.recorded().len(),
            1,
            "404 is terminal under classify_http"
        );
    }

    // ── M2 error-migration tests (slice provider-error-migu,
    // AUTH_REGISTRY_DESIGN.md §9.5) ─────────────────────────────────────────

    #[test]
    fn terminal_status_error_splits_unavailable_from_not_found() {
        // A terminal 429/5xx has already burned the whole standard ladder, so
        // what surfaces is a pipeline that stayed down (链路不可用) —
        // Unavailable, not the upstream resource-missing NotFound. End-to-end
        // exhaustion would need five scripted sends with real backoff sleeps,
        // so the mapping is pinned at this pure seam (the netease
        // `api_code_error` precedent, source_netease.rs:289-326): retry.rs
        // pins 429/5xx-exhaustion → Fatal("HTTP {code}") and the e2e 404 test
        // above pins the call-site wiring.
        for code in [429u16, 500, 502, 503, 504] {
            let error = terminal_status_error("resource info", code);
            assert_eq!(
                error.category(),
                crate::ErrorCategory::Unavailable,
                "HTTP {code}: {error}"
            );
            assert_eq!(error.provider_id(), Some("migu"), "HTTP {code}");
            assert!(error.is_retryable(), "HTTP {code}");
            assert_eq!(
                error.detail(),
                Some(format!("resource info: HTTP {code}").as_str()),
                "HTTP {code}"
            );
            assert_eq!(
                error.to_string(),
                format!("provider unavailable: resource info: HTTP {code}"),
                "HTTP {code}"
            );
        }
        // Every other non-success keeps the upstream resource-missing
        // NotFound (mg/url.rs:384-386); 404 is the e2e-pinned representative.
        for code in [400u16, 403, 404, 410, 418] {
            let error = terminal_status_error("resource info", code);
            assert!(
                matches!(error, Error::NotFound),
                "HTTP {code} must keep NotFound, got: {error}"
            );
        }
        // Texts outside the classify_http contract ("HTTP {code}") stay
        // NotFound — defensive: the resource POST is `.idempotent(true)`, so
        // the engine only ever hands this face "HTTP {code}" texts.
        for text in ["", "HTTP", "HTTP x", "API code 881: 参数异常", "nope"] {
            let error = status_error_from_text("resource info", text);
            assert!(
                matches!(error, Error::NotFound),
                "{text:?} must keep NotFound, got: {error}"
            );
        }
        // The listen face uses the numeric-status entry with the same split.
        let error = terminal_status_error("listen url", 429);
        assert_eq!(error.category(), crate::ErrorCategory::Unavailable);
        assert_eq!(error.detail(), Some("listen url: HTTP 429"));
    }

    #[test]
    fn listen_refusal_classification_keys_on_the_rights_text() {
        // The pinned body shape is the copyright interception the slice maps
        // (no code table exists upstream, so classification keys on the
        // refusal text, AUTH_REGISTRY_DESIGN.md §9.5).
        let error = listen_refusal_error(br#"{"code":"406001","msg":"no rights"}"#);
        assert_eq!(error.category(), crate::ErrorCategory::CopyrightRestricted);
        assert_eq!(error.provider_id(), Some("migu"));
        assert!(!error.is_retryable());
        assert_eq!(error.detail(), Some("listen url: code 406001, no rights"));
        // The Chinese refusal variant keys the same way.
        let error =
            listen_refusal_error(r#"{"code":"406001","msg":"无版权，无法播放"}"#.as_bytes());
        assert_eq!(error.category(), crate::ErrorCategory::CopyrightRestricted);
        assert_eq!(
            error.detail(),
            Some("listen url: code 406001, 无版权，无法播放")
        );
        // The `info` key is accepted as a fallback message field.
        let error = listen_refusal_error(r#"{"code":"406001","info":"版权受限"}"#.as_bytes());
        assert_eq!(error.category(), crate::ErrorCategory::CopyrightRestricted);
        // VIP and parameter refusals have no fixture-pinned shape and no migu
        // landing in this slice: they keep the upstream NotFound default
        // instead of a keyword guess (§9.5).
        for body in [
            r#"{"code":"401001","msg":"VIP用户专享"}"#.as_bytes(),
            r#"{"code":"400001","msg":"参数错误"}"#.as_bytes(),
            r#"{"code":"406001"}"#.as_bytes(),
            r#"{}"#.as_bytes(),
            br#"PE-format-error"#,
            b"",
        ] {
            let error = listen_refusal_error(body);
            assert!(
                matches!(error, Error::NotFound),
                "body {:?} must keep NotFound, got: {error}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[tokio::test]
    async fn missing_resources_keep_not_found_on_the_playlist_link_and_record_faces() {
        // The NotFound-dense faces whose semantics the slice keeps (the §9.5
        // inventory): a migu track record without contentId/copyrightId
        // addresses no platform resource, an empty `resource` array on the
        // link-metadata GET is the upstream NotFound (mg/song.rs:114-117),
        // and an empty first playlist page is the upstream NotFound
        // (mg/playlist.rs:69-71) on both the playlist() face and the playlist
        // share link. None of these is 链路不可用, so none splits to
        // Unavailable; the empty-album face is separately pinned by
        // `album_page_fixture_parses_rows_metadata_and_traps`.
        let (provider, business, listen) = mocked(
            MockTransport::new(|request| {
                let base = request.url.split('?').next().unwrap_or("");
                match base {
                    LINK_SONG_INFO_URL => Ok(json_body(200, r#"{"resource":[]}"#)),
                    PLAYLIST_SONG_URL => Ok(json_body(
                        200,
                        r#"{"code":"000000","data":{"songList":[],"totalCount":0}}"#,
                    )),
                    other => panic!("unexpected url base: {other}"),
                }
            }),
            MockTransport::single(json_body(200, "{}")),
        );
        let mut bare = stream_track();
        bare.extra.clear();
        let error = provider
            .resolve(&bare, &ResolveOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound), "got: {error}");
        let error = provider
            .resolve_link("https://music.migu.cn/v3/music/song/600902000006889366")
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound), "got: {error}");
        let error = provider.playlist("135423084").await.unwrap_err();
        assert!(matches!(error, Error::NotFound), "got: {error}");
        let error = provider
            .resolve_link("https://music.migu.cn/v3/lib/playlist/135423084")
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound), "got: {error}");
        // resolve() never reached the transport for the bare record; the
        // three other faces made exactly one GET each.
        assert_eq!(business.recorded().len(), 3);
        assert!(listen.recorded().is_empty());
    }

    #[tokio::test]
    async fn resolve_streams_via_strategy_decrypt_and_head_probe() {
        let payload = r#"{"data":{"url":"http://freetyst.nf.migu.cn/hq.mp3"}}"#;
        let (provider, business, listen) = mocked(
            MockTransport::new(move |request| {
                let base = request.url.split('?').next().unwrap_or("");
                match (request.method, base) {
                    (HttpMethod::Post, RESOURCE_INFO_URL) => Ok(json_body(200, SIZES_ONLY_FORMATS)),
                    (HttpMethod::Get, STRATEGY_URL) => {
                        // The playing header set survives the migration.
                        assert_eq!(header_value(request, "user-agent"), Some(UA_IPHONE));
                        assert_eq!(header_value(request, "referer"), Some(MOBILE_PLAY_REFERER));
                        assert_eq!(header_value(request, "birth"), Some("h5page"));
                        assert_eq!(header_value(request, "signature"), Some("1"));
                        assert_eq!(header_value(request, "channel"), Some("0146921"));
                        Ok(bytes_body(200, encrypt_strategy_payload(payload, 0x42)))
                    }
                    // The probe's HEAD leg reads content-length from the
                    // header (buffered model, review P-8).
                    (HttpMethod::Head, _) => Ok(TransportResponse {
                        status: 200,
                        headers: vec![("content-length".to_owned(), "9994661".to_owned())],
                        url: None,
                        body: Vec::new(),
                    }),
                    other => panic!("unexpected request: {other:?}"),
                }
            }),
            MockTransport::single(json_body(200, "{}")),
        );
        let info = provider
            .resolve(&stream_track(), &ResolveOptions::default())
            .await
            .unwrap();
        assert_eq!(info.url, "http://freetyst.nf.migu.cn/hq.mp3");
        // The served size maps back onto the declared HQ tier.
        assert_eq!(info.quality, StreamQuality::High);
        assert_eq!(info.format, AudioFormat::Mp3);
        assert_eq!(info.content_length, Some(9_994_661));
        let methods: Vec<HttpMethod> = business
            .recorded()
            .iter()
            .map(|request| request.method)
            .collect();
        assert_eq!(
            methods,
            vec![HttpMethod::Post, HttpMethod::Get, HttpMethod::Head]
        );
        assert!(
            listen.recorded().is_empty(),
            "the strategy path never touches the listen handle"
        );
    }

    #[tokio::test]
    async fn resolve_falls_back_to_listen_302_and_range_probe() {
        let (provider, business, listen) = mocked(
            MockTransport::new(|request| {
                let base = request.url.split('?').next().unwrap_or("");
                match (request.method, base) {
                    (HttpMethod::Post, RESOURCE_INFO_URL) => Ok(json_body(200, SIZES_ONLY_FORMATS)),
                    // The live anonymous shape: the strategy endpoint answers
                    // plain text, the decrypt-JSON step fails and listenSong
                    // takes over (the error is swallowed by the caller).
                    (HttpMethod::Get, STRATEGY_URL) => {
                        Ok(bytes_body(200, b"PE-format-error".to_vec()))
                    }
                    // The probe HEAD leg fails and is swallowed; the Range
                    // GET answers the 206 total.
                    (HttpMethod::Head, _) => {
                        Err(TransportError::Transport("probe head dead".into()))
                    }
                    (HttpMethod::Get, _) => {
                        assert_eq!(header_value(request, "range"), Some("bytes=0-0"));
                        Ok(TransportResponse {
                            status: 206,
                            headers: vec![(
                                "content-range".to_owned(),
                                "bytes 0-0/4317311".to_owned(),
                            )],
                            url: None,
                            body: b"x".to_vec(),
                        })
                    }
                    // Const patterns never cover every base; anything else
                    // is a test bug.
                    _ => panic!(
                        "unexpected request: {} {}",
                        request.method.as_str(),
                        request.url
                    ),
                }
            }),
            // The redirect-none listen hop answers the 302 itself.
            MockTransport::new(|request| {
                assert_eq!(request.method, HttpMethod::Get);
                assert!(request.url.starts_with(LISTEN_SONG_URL));
                assert_eq!(header_value(request, "user-agent"), Some(UA_IPHONE));
                assert_eq!(header_value(request, "referer"), Some(LISTEN_SONG_REFERER));
                Ok(TransportResponse {
                    status: 302,
                    headers: vec![(
                        "location".to_owned(),
                        "http://freetyst.nf.migu.cn/pq.mp3".to_owned(),
                    )],
                    url: Some(LISTEN_SONG_URL.to_owned()),
                    body: Vec::new(),
                })
            }),
        );
        let info = provider
            .resolve(&stream_track(), &ResolveOptions::default())
            .await
            .unwrap();
        assert_eq!(info.url, "http://freetyst.nf.migu.cn/pq.mp3");
        // The 206 total wins the served size; it matches no declared size, so
        // the requested quality stands.
        assert_eq!(info.content_length, Some(4_317_311));
        assert_eq!(info.quality, StreamQuality::High);
        // Handle separation: resource + strategy + both probe legs on the
        // business face, exactly one listen hop on the redirect-none face,
        // and no retry anywhere (send_once faces).
        let methods: Vec<HttpMethod> = business
            .recorded()
            .iter()
            .map(|request| request.method)
            .collect();
        assert_eq!(
            methods,
            vec![
                HttpMethod::Post,
                HttpMethod::Get,
                HttpMethod::Head,
                HttpMethod::Get
            ]
        );
        assert_eq!(listen.recorded().len(), 1);
    }

    #[tokio::test]
    async fn listen_face_stays_single_shot_and_never_retries() {
        // Non-3xx JSON body with the pinned no-rights shape (`{"code":
        // "406001","msg":"no rights"}`): the documented 版权拦截响应
        // (mg/url.rs:285) maps to CopyrightRestricted after exactly one send
        // — send_once never retries. The pre-migration pinned assertion
        // (Error::NotFound) is explicitly updated for the M2 migration
        // (AUTH_REGISTRY_DESIGN.md §9.5, test-renovation record).
        let (provider, _business, listen) = mocked(
            MockTransport::new(|request| {
                let base = request.url.split('?').next().unwrap_or("");
                match (request.method, base) {
                    (HttpMethod::Post, RESOURCE_INFO_URL) => Ok(json_body(200, SIZES_ONLY_FORMATS)),
                    (HttpMethod::Get, STRATEGY_URL) => Ok(bytes_body(200, b"not json".to_vec())),
                    other => panic!("unexpected business request: {other:?}"),
                }
            }),
            MockTransport::single(json_body(200, r#"{"code":"406001","msg":"no rights"}"#)),
        );
        let error = provider
            .resolve(&stream_track(), &ResolveOptions::default())
            .await
            .unwrap_err();
        assert_eq!(
            error.category(),
            crate::ErrorCategory::CopyrightRestricted,
            "got: {error}"
        );
        assert_eq!(error.provider_id(), Some("migu"));
        assert!(!error.is_retryable());
        assert_eq!(error.detail(), Some("listen url: code 406001, no rights"));
        assert_eq!(
            error.to_string(),
            "copyright restricted: listen url: code 406001, no rights"
        );
        assert_eq!(
            listen.recorded().len(),
            1,
            "the listen hop is one send_once attempt"
        );

        // Any other refusal reason keeps the upstream NotFound default, also
        // after exactly one send.
        let (provider, _business, listen) = mocked(
            MockTransport::new(|request| {
                let base = request.url.split('?').next().unwrap_or("");
                match (request.method, base) {
                    (HttpMethod::Post, RESOURCE_INFO_URL) => Ok(json_body(200, SIZES_ONLY_FORMATS)),
                    (HttpMethod::Get, STRATEGY_URL) => Ok(bytes_body(200, b"not json".to_vec())),
                    other => panic!("unexpected business request: {other:?}"),
                }
            }),
            MockTransport::single(json_body(200, r#"{"code":"400001","msg":"参数错误"}"#)),
        );
        let error = provider
            .resolve(&stream_track(), &ResolveOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound), "got: {error}");
        assert_eq!(
            listen.recorded().len(),
            1,
            "the listen hop is one send_once attempt"
        );

        // A 302 whose Location is a marked preview stays NotFound, one send.
        let (provider, _business, listen) = mocked(
            MockTransport::new(|request| {
                let base = request.url.split('?').next().unwrap_or("");
                match (request.method, base) {
                    (HttpMethod::Post, RESOURCE_INFO_URL) => Ok(json_body(200, SIZES_ONLY_FORMATS)),
                    (HttpMethod::Get, STRATEGY_URL) => Ok(bytes_body(200, b"not json".to_vec())),
                    other => panic!("unexpected business request: {other:?}"),
                }
            }),
            MockTransport::single(TransportResponse {
                status: 302,
                headers: vec![(
                    "location".to_owned(),
                    "https://cdn.migu.cn/audition/a.mp3".to_owned(),
                )],
                url: None,
                body: Vec::new(),
            }),
        );
        let error = provider
            .resolve(&stream_track(), &ResolveOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound), "got: {error}");
        assert_eq!(listen.recorded().len(), 1);
    }

    #[tokio::test]
    async fn link_and_album_pages_ride_the_playlist_header_set() {
        let song_payload = r#"{"resource":[{"songId":3790007,"songName":"晴天","contentId":"600902000006889366","copyrightId":"60054701923"}]}"#;
        let album_payload = r#"{"code":"000000","data":{"songList":[{"songId":1,"songName":"In the Beginning","album":"逆光","albumId":"10927"}],"totalCount":1}}"#;
        let (provider, business, listen) = mocked(
            MockTransport::new(move |request| {
                let base = request.url.split('?').next().unwrap_or("");
                assert_eq!(request.method, HttpMethod::Get);
                let user_agents: Vec<&str> = request
                    .headers
                    .iter()
                    .filter(|(key, _)| key == "user-agent")
                    .map(|(_, value)| value.as_str())
                    .collect();
                assert_eq!(user_agents, vec![UA_MOBILE, PLAYLIST_UA]);
                assert_eq!(header_value(request, "channel"), Some("0146921"));
                match base {
                    LINK_SONG_INFO_URL => Ok(json_body(200, song_payload)),
                    ALBUM_SONG_URL => Ok(json_body(200, album_payload)),
                    other => panic!("unexpected url base: {other}"),
                }
            }),
            MockTransport::single(json_body(200, "{}")),
        );
        let track = provider
            .fetch_song_link_info("600902000006889366")
            .await
            .unwrap();
        assert_eq!(track.id.id, "3790007");
        assert_eq!(track.title, "晴天");
        let album = provider.album("10927").await.unwrap();
        assert_eq!(album.title, "逆光");
        assert_eq!(album.track_count, Some(1));
        assert_eq!(business.recorded().len(), 2);
        assert!(listen.recorded().is_empty());
    }

    #[tokio::test]
    async fn hot_playlists_square_uses_the_playlist_header_set() {
        let (provider, business, listen) = mocked(
            MockTransport::script([Ok(json_body(
                200,
                r#"{"code":"000000","data":{"contents":[{"resType":"2021","resId":"123","txt":"热歌榜","txt2":"desc","img":"https://img.example/p.jpg"}]}}"#,
            ))]),
            MockTransport::single(json_body(200, "{}")),
        );
        let playlists = provider.playlists("", 1, 30).await.unwrap();
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].id.id, "123");
        assert_eq!(playlists[0].title, "热歌榜");
        let recorded = business.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].url.starts_with(PLAYLIST_SQUARE_URL));
        assert_eq!(header_value(&recorded[0], "user-agent"), Some(UA_MOBILE));
        assert_eq!(
            header_value(&recorded[0], "referer"),
            Some("https://m.music.migu.cn/")
        );
        assert_eq!(header_value(&recorded[0], "channel"), Some("0146921"));
        assert!(listen.recorded().is_empty());
    }
}
