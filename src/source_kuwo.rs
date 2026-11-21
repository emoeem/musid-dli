//! Kuwo Music provider migrated from VoiceFox's `lx-source::kw`.
//! Protocol origin: lx-music `musicSdk/kw/`. Anonymous search, mobile stream
//! URLs, XOR/zlib lyric decryption (plain GB18030 LRC plus the lrcx=1
//! word-by-word variant, upstream kw/lyric.rs), the public playlist module
//! (category directory, hot/tag lists, keyword search, paginated detail), the
//! album module (r.s `stype=albuminfo`: metadata + track list) and the chart
//! module (qukudata board tree + kbangserver board songs) are implemented.
//! The web play URL and cover endpoints use the web session signing flow
//! (locally generated Hm_Iuvt_ cookie plus the LCG-XOR `Secret` header);
//! lossless tiers are unavailable on the
//! anonymous mobile endpoint and downgrade to 320k/128k. Paid songs return
//! an 11-second placeholder audio with a mismatched rid: that response is
//! rejected, never handed out. Every playlist endpoint hardcodes
//! `loginUid=0&loginSid=0` upstream, so user playlists have no reference and
//! stay Unsupported.
use crate::{
    Error, ErrorDetail, Result,
    lrc::parse_lrc,
    models::{
        Album, AudioFormat, Chart, ChartEntry, FallbackEndpoint, LyricDocument, LyricFormat,
        LyricLine, LyricWord, Playlist, PlaylistCategory, StreamInfo, StreamQuality, Track,
        TrackId,
    },
    provider::{
        AlbumProvider, AuthCapability, AuthState, AuthenticationProvider, ChartProvider,
        CollectionProvider, LinkResolver, LoginStatus, LyricsProvider, MusicProvider,
        PlaylistProvider, ResolveOptions, ResolvedLink, SearchPage, SearchQuery, StreamResolver,
        TrackSearch,
    },
    retry::KUWO_CODES,
    session::SessionStore,
    transport::{
        ClientProfile, Http, RetryPolicy, TransportError, TransportRequest, TransportResponse,
    },
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::read::ZlibDecoder;
use md5::Digest;
use regex::Regex;
use serde_json::Value;
use std::{collections::BTreeMap, collections::HashSet, io::Read, sync::OnceLock, time::Duration};

const ID: &str = "kuwo";
const UA_MOBILE: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36";
/// The media CDN 403s unless the player presents the okhttp UA.
const UA_MEDIA: &str = "okhttp/3.10.0";
const SEARCH_URL: &str = "http://search.kuwo.cn/r.s";
const MOBILE_PLAY_URL: &str = "https://mobi.kuwo.cn/mobi.s";
const LYRIC_URL: &str = "http://newlyric.kuwo.cn/newlyric.lrc";
// Web (www.kuwo.cn) endpoints: every call needs a `Secret` header derived
// from a locally generated Hm_Iuvt_ cookie plus a `reqId` query parameter.
const WEB_PLAY_URL: &str = "https://www.kuwo.cn/api/v1/www/music/playUrl";
/// Cover tier one (kw/url.rs:14,25): the web-signed musicInfo endpoint; the
/// cover rides on `data.pic` of the code-200 body, no size parameter.
const MUSIC_INFO_URL: &str = "https://www.kuwo.cn/api/www/music/musicInfo";
/// Cover tier two (kw/url.rs:120-124): the anonymous artistpicserver
/// redirect service — plain http, no signing, no Referer, no special
/// headers; the body is one line of plain text with the real image URL.
const ARTIST_PIC_URL: &str = "http://artistpicserver.kuwo.cn/pic.web";
const WEB_UA: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36";
const WEB_REFERER: &str = "https://www.kuwo.cn/";
/// Site-convention cookie name for the web signing session.
const SESSION_COOKIE: &str = "Hm_Iuvt_cdb524f42f23cer9b268564v7y735ewrq2324";
const SECRET_SEED: i64 = 59_910_100;
const SECRET_MULTIPLIER: i64 = 9253;
const SECRET_INCREMENT: i64 = 23;
const SECRET_MODULUS: i64 = 2_147_483_647;
/// Mobile endpoint client identity (anonymous, from lx-music).
const MOBILE_SOURCE: &str = "kwplayer_ar_5.1.0.0_B_jiakong_vh.apk";
const MOBILE_USER: &str = "359307055300426";
const LYRIC_XOR_KEY: &[u8] = b"yeelion";
// ── Playlist endpoints (all anonymous; loginUid=0/loginSid=0 hardcoded
// upstream, kw/session.rs signing is not part of the playlist paths) ──
/// Logical hot-list page size. Kuwo sometimes ignores the requested `rn` and
/// answers with only ten rows, so the hot list pulls up to
/// `MAX_PLAYLIST_REQUESTS` server pages and dedupes by id to fill one page.
const PLAYLIST_PAGE_SIZE: u32 = 36;
const MAX_PLAYLIST_REQUESTS: u32 = 4;
/// Detail pages request 1000 rows; some nodes cap the response at ~10 rows,
/// so the ladder continues with the first response's effective page size.
const DETAIL_REQUEST_SIZE: u32 = 1000;
/// Playlists longer than MAX_DETAIL_PAGES pages (~100k songs) are truncated.
const MAX_DETAIL_PAGES: u32 = 100;
/// The wapi tag/category and pl.svc playlist endpoints require this Referer
/// upstream; the hot list, qukudata node resolve and r.s search do not.
const PLAYLIST_REFERER: &str = "http://www.kuwo.cn/";
const WAPI_PLAYLIST_URL: &str = "http://wapi.kuwo.cn/api/pc/classify/playlist";
const PLAYLIST_DETAIL_URL: &str = "http://nplserver.kuwo.cn/pl.svc";
const PLAYLIST_NODE_URL: &str = "http://qukudata.kuwo.cn/q.k";
/// Board songs (charts): plain JSON over plain http, no signature, no
/// cookie, no Referer (kw/leaderboard.rs reads neither).
const CHART_SONGS_URL: &str = "http://kbangserver.kuwo.cn/ksong.s";
/// musid-dli's `chart_tracks` has no page parameters, so one server page of
/// 300 rows is pinned: the live board `num` is "300" and the QQ toplist
/// adapter pins the same size; upstream has no pull-the-whole-chart call.
const CHART_PAGE_SIZE: u32 = 300;
/// Song-link metadata endpoint (kw/parse.rs:62-72): plaintext HTTP, no
/// cookie, no signature, no encryption; the only header is the web Referer.
/// Metadata only — paid songs have no stream URL here but must still be
/// listable (upstream kw/parse.rs:57-60).
const SONG_INFO_URL: &str = "http://m.kuwo.cn/newh5/singles/songinfoandlrc";

#[derive(Clone)]
pub struct KuwoProvider {
    http: Http,
    sessions: SessionStore,
}
impl std::fmt::Debug for KuwoProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KuwoProvider")
            .field("session_store", &"redacted")
            .finish()
    }
}
impl Default for KuwoProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl KuwoProvider {
    pub fn new() -> Self {
        // Client construction moved onto the unified transport layer: the old
        // builder set exactly a 15s total timeout, which is what
        // `provider_default()` carries (TRANSPORT_DESIGN.md §4.3); the build
        // fallback shape lives in `build_client`.
        let http = Http::from_profile(&ClientProfile::provider_default());
        Self {
            http,
            sessions: SessionStore::default(),
        }
    }
    /// Test seam: inject a transport handle (a mock in unit tests). The
    /// production constructor is [`KuwoProvider::new`]. Test-only so
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
    /// Register all currently implemented Kuwo capabilities in one step.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_lyrics(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_album(provider.clone());
        registry.register_playlist(provider.clone());
        registry.register_link_resolver(provider.clone());
        registry.register_chart(provider);
    }
    /// Adds the request-level mobile UA and the session cookies to a
    /// [`TransportRequest`]. Platform-specific and kept in the provider: the
    /// anonymous mobile client identity travels per request, exactly like the
    /// previous `RequestBuilder` helper.
    fn request(&self, request: TransportRequest) -> TransportRequest {
        let mut request = request.header("user-agent", UA_MOBILE);
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
    /// GET request skeleton shared by `raw_get` and `get_json_with`: the
    /// mobile UA + session cookies (via `request`) plus the optional Referer.
    /// The Referer covers the playlist endpoints that require
    /// `http://www.kuwo.cn/` (wapi tag/category and pl.svc detail calls); the
    /// unified layer carries it as a per-request header.
    fn get_request(&self, url: &str, referer: Option<&str>) -> TransportRequest {
        let mut request = self.request(TransportRequest::get(url));
        if let Some(referer) = referer {
            request = request.header("referer", referer);
        }
        request
    }
    /// GET through the unified transport layer (`send_raw`): transport
    /// errors, 429 and 5xx retry under `RetryPolicy::standard()` with the
    /// engine's exponential backoff + jitter + `Retry-After`, replacing the
    /// fixed `[600,1200,2400,4000]` table (declared delta,
    /// TRANSPORT_DESIGN.md §5.5 row 1). The send budget is unchanged (at
    /// most 5 sends); classification stays in `retry` (`classify_http` via
    /// the engine). Serves the raw (non-JSON) faces — the yeelion lyric
    /// chain, the artistpicserver plain text and the r.s JS-literal routes —
    /// which read the buffered body directly.
    async fn raw_get(
        &self,
        url: &str,
        label: &str,
        referer: Option<&str>,
    ) -> Result<TransportResponse> {
        let request = self.get_request(url, referer);
        match self.http.send_raw(&request, &RetryPolicy::standard()).await {
            Ok(response) => Ok(response),
            Err(error) => {
                tracing::debug!("Kuwo {label} failed: {error}");
                Err(perr(format!("{label}: {error}")))
            }
        }
    }
    async fn get_json(&self, url: &str, label: &str) -> Result<Value> {
        self.get_json_with(url, label, None).await
    }
    /// Same gate as `get_json` with an optional Referer; the playlist
    /// endpoints that need it pass `Some(PLAYLIST_REFERER)`. One
    /// `send_api_json` call: the engine classifies the HTTP status, decodes
    /// the buffered JSON body and classifies the API code with
    /// `api_retries: 0`, keeping today's body-code semantics (§5.3/§5.5 row
    /// 6: kuwo's `classify_api` ran with budget 0, so codes never retried).
    async fn get_json_with(&self, url: &str, label: &str, referer: Option<&str>) -> Result<Value> {
        let request = self.get_request(url, referer);
        let policy = RetryPolicy {
            api_retries: 0,
            ..RetryPolicy::standard()
        };
        match self
            .http
            .send_api_json(&request, &policy, &KUWO_CODES, kuwo_code_of)
            .await
        {
            Ok(value) => Ok(value),
            Err(TransportError::SessionExpired) => {
                // Defensive arm (see `mark_session_expired`): KUWO_CODES
                // has an empty expired table (retry.rs), so the engine
                // cannot produce this verdict today — the classified
                // error and the token discard keep the M2 landing spot
                // ready should a fixture ever pin a code.
                Err(self.mark_session_expired(Some(ErrorDetail::new(label))))
            }
            Err(error) => {
                tracing::debug!("Kuwo {label} failed: {error}");
                Err(perr(format!("{label}: {error}")))
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
/// `(code, message)` extractor handed to `Http::send_api_json`; extraction
/// stays provider-side, classification lives in `retry` (TRANSPORT_DESIGN.md
/// §4.1). Kuwo reads `code`/`msg`, and the field only exists on the play
/// endpoints — search and playlist bodies carry no `code` at all, so a
/// missing code falls back to `KUWO_CODES.ok` (review P-7), exactly like the
/// previous `unwrap_or(KUWO_CODES.ok)`; falling back to `0` instead would
/// flip every missing-code body into `Fatal("API code 0")`.
fn kuwo_code_of(value: &Value) -> (i64, String) {
    (
        value["code"].as_i64().unwrap_or(KUWO_CODES.ok),
        value["msg"].as_str().unwrap_or("unknown error").to_owned(),
    )
}
fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}
/// Kuwo JSON fields alternate between strings and numbers (`rid` arrives as a
/// number in practice); unify to text.
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}
fn scalar_u64(value: &Value) -> u64 {
    match value {
        Value::Number(number) => number.as_u64().unwrap_or(0),
        Value::String(text) => text.trim().parse().unwrap_or(0),
        _ => 0,
    }
}
/// Bitrate to quality mapping: 4000 = hires, 2000 = lossless.
fn bitrate_to_quality(bitrate: u32) -> Option<StreamQuality> {
    match bitrate {
        4000 => Some(StreamQuality::HiRes),
        2000 => Some(StreamQuality::Lossless),
        320 => Some(StreamQuality::High),
        128 => Some(StreamQuality::Low),
        _ => None,
    }
}
/// Parse the `N_MINFO` quality string:
/// `level:xx,bitrate:4000,format:flac,size:50.1M;level:yy,bitrate:320,...`
fn parse_n_minfo_qualities(n_minfo: &str) -> Vec<StreamQuality> {
    let mut qualities = Vec::new();
    for part in n_minfo.split(';') {
        let bitrate = part
            .split(',')
            .filter_map(|kv| kv.trim().split_once(':'))
            .find(|(key, _)| *key == "bitrate")
            .and_then(|(_, value)| value.trim().parse::<u32>().ok());
        if let Some(q) = bitrate.and_then(bitrate_to_quality)
            && !qualities.contains(&q)
        {
            qualities.push(q);
        }
    }
    qualities
}
/// Anonymous mobile tiers; lossless tiers would return placeholder audio, so
/// hi-res and lossless requests downgrade to 320k then 128k.
struct MobileTier {
    br: &'static str,
    quality: StreamQuality,
}
const TIER_320: MobileTier = MobileTier {
    br: "320kmp3",
    quality: StreamQuality::High,
};
const TIER_128: MobileTier = MobileTier {
    br: "128kmp3",
    quality: StreamQuality::Low,
};
fn mobile_tiers(quality: StreamQuality) -> &'static [MobileTier] {
    match quality {
        StreamQuality::Low => &[TIER_128],
        _ => &[TIER_320, TIER_128],
    }
}
/// The web signed URL as a structured fallback candidate (kw/url.rs:240-256
/// parity): upstream applies the okhttp UA to the whole `SongUrl`, candidate
/// included, and the web API declares no tier (kw/url.rs:244-247), so
/// `quality: None` keeps the same-tier-as-primary assumption the legacy
/// string slot already encoded. A URL identical to the primary is dropped
/// (the engine would dedup it anyway, MODEL_ALIGNMENT_DESIGN.md §2.4).
fn kuwo_web_candidates(web_url: Option<&str>, primary: &str) -> Vec<FallbackEndpoint> {
    web_url
        .into_iter()
        .filter(|candidate| *candidate != primary)
        .map(|candidate| FallbackEndpoint {
            url: candidate.to_owned(),
            headers: vec![("User-Agent".to_owned(), UA_MEDIA.to_owned())],
            expires_at_unix: None,
            content_length: None,
            quality: None,
        })
        .collect()
}
/// Encrypt lyric request parameters: XOR with the "yeelion" key then base64.
fn encrypt_lyric_params(params: &str) -> String {
    let encrypted: Vec<u8> = params
        .as_bytes()
        .iter()
        .enumerate()
        .map(|(i, byte)| byte ^ LYRIC_XOR_KEY[i % LYRIC_XOR_KEY.len()])
        .collect();
    STANDARD.encode(encrypted)
}
/// Skip the optional `tp=content\r\n\r\n` header frame (4-byte window scan;
/// when absent the whole body is the zlib stream) and zlib-inflate — zlib,
/// not gzip (upstream kw/lyric.rs:58-71).
fn inflate_lyric_body(raw: &[u8]) -> Result<Vec<u8>> {
    let compressed = match raw.windows(4).position(|window| window == b"\r\n\r\n") {
        Some(position) => &raw[position + 4..],
        None => raw,
    };
    let mut decoder = ZlibDecoder::new(compressed);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| perr(format!("lyric zlib decompress failed: {e}")))?;
    Ok(decompressed)
}

/// Decode the plain lyric body: skip the `tp=content` header, zlib-inflate,
/// then GB18030-decode (the lyric payload is not UTF-8).
fn decode_lyric_body(raw: &[u8]) -> Result<String> {
    let decompressed = inflate_lyric_body(raw)?;
    let (decoded, _, _) = encoding_rs::GB18030.decode(&decompressed);
    Ok(decoded.into_owned())
}

/// Decode the lrcx word-lyric body (upstream kw/lyric.rs:78-90): inflate,
/// keep ASCII bytes only, base64 decode, XOR with the same `yeelion` key,
/// then GB18030-decode.
///
/// One deliberate divergence from the upstream Rust text: the trailing wire
/// junk layout has no live evidence (no captured lrcx body exists anywhere),
/// and the audit's verified end-to-end vector carries four junk bytes — two
/// non-ASCII (`0x9f 0x8e`, dropped by upstream's `is_ascii()` filter) plus
/// two ASCII (`\x00\x7f`), which upstream's strict STANDARD decode would
/// choke on after its ASCII filter kept them. Non-alphabet characters are
/// therefore ignored before the strict canonical decode, matching the
/// audit's verification semantics (Python `b64decode(validate=False)`).
/// On a clean canonical payload the filter is a no-op, so bodies upstream
/// accepts decode to byte-identical results here; the strict decode can
/// still fail (e.g. an odd-length remainder) and the caller treats that as
/// "no word lyrics".
fn decode_lrcx_body(raw: &[u8]) -> Result<String> {
    let decompressed = inflate_lyric_body(raw)?;
    let base64_text: String = decompressed
        .iter()
        .filter(|byte| byte.is_ascii())
        .map(|&byte| byte as char)
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '=')
        .collect();
    let ciphertext = STANDARD
        .decode(base64_text.as_bytes())
        .map_err(|e| perr(format!("lyric base64 decode failed: {e}")))?;
    let decrypted: Vec<u8> = ciphertext
        .iter()
        .enumerate()
        .map(|(i, byte)| byte ^ LYRIC_XOR_KEY[i % LYRIC_XOR_KEY.len()])
        .collect();
    let (decoded, _, _) = encoding_rs::GB18030.decode(&decrypted);
    Ok(decoded.into_owned())
}

/// Parse decrypted lrcx text, the lx-music unified karaoke format: line
/// heads `[start_ms,dur]` or `[mm:ss.xxx]`, word tags `<rel_start,dur[,0]>`
/// whose starts are offsets relative to the line head. Port of the lx branch
/// of upstream voicefox `lyric/src/parser/yrc.rs` (`parse`,
/// `parse_line_timestamp`, `parse_prefixed_words` with
/// `starts_are_relative = true`); the absolute `(...)` YRC word branch is
/// not ported — no upstream evidence says a Kuwo lrcx payload carries it,
/// and such a text parses to no lines (the document builder then falls back
/// to the plain lyric). Line heads that match neither form (metadata tags,
/// plain text) and lines without words are dropped, like upstream.
fn parse_lrcx(content: &str) -> Vec<LyricLine> {
    static NUMERIC_LINE: OnceLock<Regex> = OnceLock::new();
    static TIMESTAMP_LINE: OnceLock<Regex> = OnceLock::new();
    static LX_WORD: OnceLock<Regex> = OnceLock::new();
    let numeric_line = NUMERIC_LINE
        .get_or_init(|| Regex::new(r"^\[\s*(\d+)\s*,\s*\d+\s*\]").expect("valid lrcx line regex"));
    let timestamp_line = TIMESTAMP_LINE.get_or_init(|| {
        Regex::new(r"^\[(\d+):(\d{1,2})[.:](\d{1,3})\]").expect("valid lrcx line regex")
    });
    let lx_word = LX_WORD.get_or_init(|| {
        Regex::new(r"<(-?\d+),(-?\d+)(?:,-?\d+)?>").expect("valid lrcx word regex")
    });
    let mut lines = Vec::new();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        let Some((timestamp, body_start)) =
            parse_lrcx_line_timestamp(line, numeric_line, timestamp_line)
        else {
            continue;
        };
        let words = parse_lx_words(&line[body_start..], timestamp, lx_word);
        if words.is_empty() {
            continue;
        }
        let text = words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<String>();
        lines.push(LyricLine {
            start_ms: timestamp,
            end_ms: None,
            text,
            translations: BTreeMap::new(),
            romanization: None,
            words,
        });
    }
    lines.sort_by_key(|line| line.start_ms);
    lines
}

/// Line-head timestamp of an lrcx line: the numeric `[1234,3000]` form (the
/// line duration is not consumed — `LyricLine.end_ms` stays unset, like the
/// kugou KRC document) or the `[mm:ss.xxx]` timestamp form with a 1-3 digit
/// fraction (upstream yrc.rs:53-79).
fn parse_lrcx_line_timestamp(
    line: &str,
    numeric_line: &Regex,
    timestamp_line: &Regex,
) -> Option<(u64, usize)> {
    if let Some(captures) = numeric_line.captures(line) {
        return Some((
            captures.get(1)?.as_str().parse().ok()?,
            captures.get(0)?.end(),
        ));
    }
    let captures = timestamp_line.captures(line)?;
    let minutes: u64 = captures.get(1)?.as_str().parse().ok()?;
    let seconds: u64 = captures.get(2)?.as_str().parse().ok()?;
    let fraction = captures.get(3)?.as_str();
    let value: u64 = fraction.parse().ok()?;
    let millis = match fraction.len() {
        1 => value * 100,
        2 => value * 10,
        _ => value,
    };
    Some((
        minutes
            .saturating_mul(60_000)
            .saturating_add(seconds.saturating_mul(1_000))
            .saturating_add(millis),
        captures.get(0)?.end(),
    ))
}

/// Word tags of one lrcx line body: each `<rel_start,dur[,0]>` marker owns
/// the text up to the next marker (or the end of the body); empty texts are
/// skipped, negative starts clamp to the line head and word starts are
/// offsets relative to `line_timestamp` (upstream yrc.rs:81-128,
/// `starts_are_relative = true`).
fn parse_lx_words(body: &str, line_timestamp: u64, lx_word: &Regex) -> Vec<LyricWord> {
    let matches: Vec<_> = lx_word.captures_iter(body).collect();
    let mut words = Vec::with_capacity(matches.len());
    for (index, captures) in matches.iter().enumerate() {
        let Some(marker) = captures.get(0) else {
            continue;
        };
        let text_end = matches
            .get(index + 1)
            .and_then(|next| next.get(0))
            .map_or(body.len(), |next| next.start());
        let text = body[marker.end()..text_end].to_owned();
        if text.is_empty() {
            continue;
        }
        let raw_start = captures
            .get(1)
            .and_then(|value| value.as_str().parse::<i64>().ok())
            .unwrap_or(0)
            .max(0) as u64;
        let duration = captures
            .get(2)
            .and_then(|value| value.as_str().parse::<i64>().ok())
            .unwrap_or(0)
            .unsigned_abs();
        words.push(LyricWord {
            text,
            start_ms: line_timestamp.saturating_add(raw_start),
            duration_ms: duration,
        });
    }
    words
}

/// `[mm:ss.mmm]` plain-LRC timestamp used for the karaoke document's
/// `original` rendering (same helper as the kugou/migu providers).
fn format_timestamp(millis: u64) -> String {
    let minutes = millis / 60_000;
    let seconds = (millis % 60_000) / 1_000;
    let fraction = millis % 1_000;
    format!("{minutes:02}:{seconds:02}.{fraction:03}")
}
/// Validate one mobi.s response body: the play URL must be a real http(s)
/// URL, the returned rid must match the requested song and the duration must
/// be non-zero. Paid songs return an ~11-second placeholder with a mismatched
/// rid: rejected here, never served.
fn validate_mobile_play(data: &Value, song_id: &str) -> Result<String> {
    let play_url = data["url"]
        .as_str()
        .map(str::trim)
        .filter(|url| is_http_url(url))
        .ok_or(Error::NotFound)?;
    let returned_rid = scalar_text(&data["rid"]);
    let duration = scalar_u64(&data["duration"]);
    if returned_rid != song_id || duration == 0 {
        tracing::debug!(
            "Kuwo returned a placeholder track (requested rid={song_id}, returned rid={returned_rid}, duration={duration}s)"
        );
        return Err(Error::NotFound);
    }
    Ok(play_url.to_owned())
}
impl KuwoProvider {
    async fn fetch_search_page(&self, keyword: &str, page: u32, limit: u32) -> Result<Value> {
        let url = format!(
            "{SEARCH_URL}?client=kt&all={}&pn={}&rn={}&uid=794762570&ver=kwplayer_ar_9.2.2.1&vipver=1&show_copyright_off=1&newver=1&ft=music&cluster=0&strategy=2012&encoding=utf8&rformat=json&vermerge=1&mobi=1&issubtitle=1",
            urlencoding::encode(keyword),
            page,
            limit
        );
        self.get_json(&url, "search").await
    }
    fn parse_track(&self, item: &Value) -> Result<Track> {
        let music_rid = item["MUSICRID"].as_str().unwrap_or_default();
        let song_id = music_rid.strip_prefix("MUSIC_").unwrap_or(music_rid);
        if song_id.is_empty() {
            return Err(perr("search item has no rid"));
        }
        let title = item["SONGNAME"].as_str().unwrap_or_default().trim();
        if title.is_empty() {
            return Err(perr("search item has no title"));
        }
        // Artists are joined with `&` upstream; split them back apart.
        let artists: Vec<String> = item["ARTIST"]
            .as_str()
            .unwrap_or_default()
            .split('&')
            .filter(|artist| !artist.trim().is_empty())
            .map(|artist| artist.trim().to_owned())
            .collect();
        let n_minfo = item["N_MINFO"].as_str().unwrap_or_default();
        let duration = scalar_u64(&item["DURATION"]);
        Ok(Track {
            id: TrackId::new(ID, song_id),
            title: title.into(),
            artists,
            album: item["ALBUM"]
                .as_str()
                .filter(|album| !album.trim().is_empty())
                .map(str::to_owned),
            album_id: item["ALBUMID"]
                .as_str()
                .filter(|album| !album.trim().is_empty())
                .map(str::to_owned),
            duration: (duration > 0).then(|| Duration::from_secs(duration)),
            artwork_url: None,
            isrc: None,
            available_qualities: parse_n_minfo_qualities(n_minfo),
            audio: None,
            extra: BTreeMap::new(),
        })
    }
    /// Parse one r.s search page: TOTAL/SHOW counters, abslist items (bad
    /// entries are skipped without failing the page) and the has_more rule
    /// (`items >= limit` and `SHOW >= limit`).
    fn parse_search_page(&self, json: &Value, limit: u32) -> Result<SearchPage> {
        let total = scalar_u64(&json["TOTAL"]);
        let show_count = scalar_u64(&json["SHOW"]);
        let mut items = Vec::new();
        for item in json["abslist"].as_array().into_iter().flatten() {
            match self.parse_track(item) {
                Ok(track) => items.push(track),
                // Skip malformed entries instead of failing the whole page.
                Err(_) => continue,
            }
        }
        let has_more = items.len() as u64 >= u64::from(limit) && show_count >= u64::from(limit);
        Ok(SearchPage {
            items,
            total: Some(total),
            has_more,
        })
    }
    /// One mobile-tier play URL request. Paid songs return an 11-second
    /// placeholder with a mismatched rid: rejected here, never served.
    async fn fetch_mobile_url(&self, song_id: &str, tier: &MobileTier) -> Result<String> {
        let url = format!(
            "{MOBILE_PLAY_URL}?f=web&source={MOBILE_SOURCE}&type=convert_url_with_sign&sig=0&network=WIFI&br={}&format=mp3&rid={}&user={MOBILE_USER}",
            tier.br, song_id
        );
        let value = self.get_json(&url, "stream resolve").await?;
        validate_mobile_play(&value["data"], song_id)
    }
    /// The mobile tier ladder: first tier that serves a real track wins, and
    /// the winner's tier is reported (`mobile_tiers` order, pre-race loop
    /// shape). Failures collapse into the last tier's error so a full-ladder
    /// exhaustion surfaces the deepest attempt.
    async fn fetch_mobile_url_ladder(
        &self,
        song_id: &str,
        quality: StreamQuality,
    ) -> Result<(String, &'static MobileTier)> {
        let mut last_error = Error::NotFound;
        for tier in mobile_tiers(quality) {
            match self.fetch_mobile_url(song_id, tier).await {
                Ok(url) => return Ok((url, tier)),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }
    /// One newlyric.lrc request. `with_lrcx = false` fetches the plain lyric
    /// (GB18030 LRC); `with_lrcx = true` appends `&lrcx=1` for the
    /// word-by-word version and decodes the lrcx chain. Same params shape
    /// and headers for both (upstream kw/lyric.rs:27-46).
    async fn fetch_raw_lyric(&self, song_id: &str, with_lrcx: bool) -> Result<String> {
        let mut params =
            format!("user=12345,web,web,web&requester=localhost&req=1&rid=MUSIC_{song_id}");
        if with_lrcx {
            params.push_str("&lrcx=1");
        }
        let url = format!("{LYRIC_URL}?{}", encrypt_lyric_params(&params));
        let response = self.raw_get(&url, "lyric", None).await?;
        // The transport adapter fully buffers the body, so today's separate
        // read step is gone; a mid-body reset now surfaces (and retries) as
        // `TransportError::Transport` instead of a terminal read error
        // (TRANSPORT_DESIGN.md §5.5 row 4).
        let raw = response.body;
        if raw.is_empty() {
            return Err(Error::NotFound);
        }
        if with_lrcx {
            decode_lrcx_body(&raw)
        } else {
            decode_lyric_body(&raw)
        }
    }
}
/// `Secret` header: XOR each cookie byte with an LCG-derived mask, hex it,
/// append the 8-digit nonce. Verified against the MusicBot-Go reference
/// vectors ("abc123"/12345678 -> 4232e893231400bc614e).
fn build_secret(cookie: &str, nonce: u32) -> String {
    let mut state = SECRET_SEED;
    let mut encoded = Vec::with_capacity(cookie.len());
    for byte in cookie.as_bytes() {
        state = (state * SECRET_MULTIPLIER + SECRET_INCREMENT) % SECRET_MODULUS;
        let mask = (state * 255 / SECRET_MODULUS) as u8;
        encoded.push(byte ^ mask);
    }
    format!("{}{nonce:08x}", hex::encode(encoded))
}
fn random_byte(index: usize) -> u8 {
    let seed = format!(
        "{}-{}-{index}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
        std::process::id()
    );
    let digest = md5::Md5::digest(seed.as_bytes());
    digest[index % digest.len()]
}
fn random_alnum(length: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    (0..length)
        .map(|index| char::from(ALPHABET[(random_byte(index) as usize) % ALPHABET.len()]))
        .collect()
}
fn is_illegal_session(body: &str) -> bool {
    body.contains("request is illegal") || body.contains("The request is illegal")
}
fn with_request_id(url: &str, request_id: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}reqId={request_id}")
}
fn request_id() -> String {
    let mut bytes = [0u8; 16];
    for (chunk_index, chunk) in bytes.chunks_mut(8).enumerate() {
        let value = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(chunk_index as u64))
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(chunk.as_ptr() as u64);
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let as_hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &as_hex[0..8],
        &as_hex[8..12],
        &as_hex[12..16],
        &as_hex[16..20],
        &as_hex[20..32]
    )
}
impl KuwoProvider {
    /// Locally generated 32-char token used as the Hm_Iuvt_ cookie value.
    /// The server only validates that the Secret decodes to the same value as
    /// the cookie (live-verified upstream 2026-09), so a random token passes.
    fn session_token(&self) -> String {
        self.sessions
            .get(ID)
            .and_then(|session| session.tokens.get("web_token").cloned())
            .unwrap_or_else(|| {
                let token = random_alnum(32);
                let mut session = self.sessions.get(ID).unwrap_or_default();
                session.tokens.insert("web_token".into(), token.clone());
                let _ = self.sessions.set(ID, session);
                token
            })
    }
    fn invalidate_web_token(&self) {
        if let Some(mut session) = self.sessions.get(ID) {
            session.tokens.remove("web_token");
            let _ = self.sessions.set(ID, session);
        }
    }
    /// M2 landing spot for the `retry` classifier's `SessionExpired`
    /// verdict (AUTH_REGISTRY_DESIGN.md §1.1, §8 slice 4). Kuwo has no
    /// account login: the only session is the anonymous web signing token,
    /// so the verdict lands as [`Error::AuthenticationRequired`] — Display
    /// body "login required", byte-identical to the pre-migration
    /// `{label}: login required` tail — instead of
    /// [`Error::SessionExpired`], which would claim an account session
    /// kuwo never had. The marking itself is the token discard
    /// ([`Self::invalidate_web_token`]): the state returns to `LoggedOut`
    /// and the next web-signed call mints a fresh token, the documented
    /// recovery (there is no re-login). `KUWO_CODES.expired` is empty
    /// today (retry.rs), so the `TransportError::SessionExpired` arm in
    /// `get_json_with` is defensive rather than reachable, exactly like
    /// kugou's.
    fn mark_session_expired(&self, detail: Option<ErrorDetail>) -> Error {
        self.invalidate_web_token();
        Error::AuthenticationRequired {
            provider: Some(ID.into()),
            detail,
        }
    }
    /// Derive the unified [`AuthState`] from the stored session (no network
    /// I/O). Kuwo's session shape is the anonymous `web_token`
    /// ([`Self::session_token`] mints one on demand): a stored token is
    /// `Authenticated`, no token is `LoggedOut` — the token-presence
    /// predicate replaces the previous hardcoded `LoggedOut` that
    /// contradicted the token the provider writes. This is the AuthState
    /// wiring for the M2 state machine (AUTH_REGISTRY_DESIGN.md §2, §8
    /// slice 4): `login_status` reports through this state instead of
    /// hand-classifying; the anonymous web identity is described by the
    /// fold (`to_login_status(None)` -> `LoggedIn { user_id: None, .. }`)
    /// and by [`MusicProvider::auth_capability`] returning
    /// [`AuthCapability::Anonymous`], never by an account claim.
    fn auth_state(&self) -> AuthState {
        match self.sessions.get(ID) {
            Some(session) if session.tokens.contains_key("web_token") => AuthState::Authenticated,
            _ => AuthState::LoggedOut,
        }
    }
    /// Applies the web signing headers to a [`TransportRequest`]: the web UA,
    /// web Referer, the LCG-XOR `Secret` header and the session cookie. The
    /// Secret derivation (`build_secret`) and the `reqId` query signing
    /// (`with_request_id` + `request_id`) are kuwo platform specifics and
    /// stay in the provider (TRANSPORT_DESIGN.md §7.4).
    fn sign_web(&self, request: TransportRequest, cookie: &str, nonce: u32) -> TransportRequest {
        request
            .header("user-agent", WEB_UA)
            .header("referer", WEB_REFERER)
            .header("Secret", build_secret(cookie, nonce))
            .header("cookie", format!("{SESSION_COOKIE}={cookie}"))
    }
    /// Signed GET returning the raw body text (illegal-session detection
    /// needs the text shape). Single-shot through `send_once`: exactly one
    /// attempt today and still one after migration — the response status is
    /// not classified (an illegal-session rejection answers HTTP 200 with a
    /// text marker; only the body shape reveals it), and there is no retry
    /// ladder; on rejection the stored token is invalidated and the caller
    /// retries with a fresh token on its next call (TRANSPORT_DESIGN.md
    /// §7.4).
    async fn web_get(&self, url: &str, label: &str) -> Result<String> {
        let cookie = self.session_token();
        let nonce = (random_byte(0) as u32) * 1_000_000 + (random_byte(1) as u32) * 10_000;
        let url = with_request_id(url, &request_id());
        let request = self.sign_web(TransportRequest::get(&url), &cookie, nonce);
        let response = self
            .http
            .send_once(&request)
            .await
            .map_err(|e| perr(format!("{label}: {e}")))?;
        let body = response.text();
        if is_illegal_session(&body) {
            self.invalidate_web_token();
            return Err(perr(format!(
                "{label}: session rejected, retry with a fresh token"
            )));
        }
        Ok(body.into_owned())
    }
    /// Web play URL (web-signed); lossless tiers work here where the mobile
    /// endpoint serves placeholders. Returns the URL only.
    async fn fetch_web_url(&self, song_id: &str) -> Result<String> {
        let url = format!("{WEB_PLAY_URL}?mid={song_id}&type=music&httpsStatus=1");
        let body = self.web_get(&url, "web play url").await?;
        parse_web_play_url(&body)
    }
    /// Song cover URL: the two-level serial fallback of kw/url.rs:84-89.
    /// Tier one is the web-signed musicInfo endpoint, tier two the anonymous
    /// artistpicserver redirect service; tier two only runs when tier one
    /// yields nothing, and an exhausted ladder is `NotFound` (upstream
    /// kw/mod.rs:79-83 maps its `None` to FetchError::NotFound).
    pub async fn resolve_cover_url(&self, song_id: &str) -> Result<String> {
        if let Some(url) = self.fetch_cover_url(song_id).await {
            return Ok(url);
        }
        if let Some(url) = self.fetch_artist_pic_url(song_id).await {
            return Ok(url);
        }
        Err(Error::NotFound)
    }
    /// Cover tier one (kw/url.rs:92-118): web-signed musicInfo. The
    /// response's HTTP status is always 200 — including the illegal-session
    /// rejection, which only the body shape reveals — so every failure here
    /// (transport, illegal session, non-200 code, unusable pic) becomes
    /// `None` and hands over to tier two, like upstream. An illegal session
    /// invalidates the stored token without retrying tier one; the fresh
    /// token is used on the next call.
    async fn fetch_cover_url(&self, song_id: &str) -> Option<String> {
        let body = self
            .web_get(&music_info_url(song_id), "music info")
            .await
            .ok()?;
        parse_music_info_pic(&body)
    }
    /// Cover tier two (kw/url.rs:120-137): plain-http artistpicserver. The
    /// endpoint is a redirect service, not an image host — the body is one
    /// line of plain text carrying the real image URL, and an invalid rid
    /// still answers HTTP 200 with the plain text `NO_PIC`, so the
    /// `is_http_url` filter is the only defense.
    async fn fetch_artist_pic_url(&self, song_id: &str) -> Option<String> {
        let response = self
            .raw_get(&artist_pic_url(song_id), "artist pic", None)
            .await
            .ok()?;
        // The body is already buffered by the transport; the plain-text
        // decode is lossy and cannot fail, so a read failure today (None)
        // becomes a raw_get failure tomorrow (None via the same `?`).
        parse_artist_pic_body(&response.text())
    }
}
/// Web playUrl body gate: `code == 200` yields `data.url`; paid content is
/// `code = -1`, other codes are rights/region blocks. All of these become
/// `NotFound`.
fn parse_web_play_url(body: &str) -> Result<String> {
    let json: Value =
        serde_json::from_str(body).map_err(|e| perr(format!("web play url parse failed: {e}")))?;
    if json["code"].as_i64() != Some(200) {
        return Err(Error::NotFound);
    }
    json["data"]["url"]
        .as_str()
        .map(str::trim)
        .filter(|url| is_http_url(url))
        .map(str::to_owned)
        .ok_or(Error::NotFound)
}

// ── Cover parsing (ports of kw/url.rs:92-137) ───────────────────────────

/// Cover tier one route (kw/url.rs:25): web-signed musicInfo; `reqId` is
/// appended later by `web_get` via `with_request_id`.
fn music_info_url(song_id: &str) -> String {
    format!("{MUSIC_INFO_URL}?mid={song_id}&httpsStatus=1")
}
/// Cover tier two route (kw/url.rs:120-124): plain http with the hardcoded
/// 500px params; pictype/size are pinned upstream with no size negotiation.
fn artist_pic_url(song_id: &str) -> String {
    format!("{ARTIST_PIC_URL}?corp=kuwo&type=rid_pic&pictype=500&size=500&rid={song_id}")
}
/// musicInfo body gate (kw/url.rs:108-117): `code == 200` yields `data.pic`;
/// the trimmed string must be an absolute http(s) URL. A code -1 body
/// (歌曲不存在), any other non-200 code, an empty/missing pic and malformed
/// JSON all yield `None`.
fn parse_music_info_pic(body: &str) -> Option<String> {
    let json: Value = serde_json::from_str(body).ok()?;
    if json["code"].as_i64() != Some(200) {
        return None;
    }
    json["data"]["pic"]
        .as_str()
        .map(str::trim)
        .filter(|pic| is_http_url(pic))
        .map(str::to_owned)
}
/// artistpicserver body gate (kw/url.rs:131-136): trim the single-line plain
/// text body and accept it only when it is an absolute http(s) URL — this
/// filter is what turns the `NO_PIC` answer of an invalid rid into `None`.
fn parse_artist_pic_body(body: &str) -> Option<String> {
    let pic = body.trim();
    is_http_url(pic).then(|| pic.to_owned())
}

// ── Playlist parsing (ports of kw/playlist.rs + kw/leaderboard.rs) ──────

/// `search.kuwo.cn/r.s` replies with a **JS object literal** (bare keys,
/// single-quoted strings, trailing semicolon), not JSON — the song search,
/// playlist search and albuminfo routes all share this legacy shape. Minimal
/// rewrite before handing the text to serde: quote bare keys, convert
/// single-quoted strings to double quotes, drop the trailing semicolon. The
/// rewrite must track string state: a regex pass that quotes everything
/// after a `{`/`,` boundary also hits the `foo:bar` pairs **inside** string
/// values (every `N_MINFO` value is
/// `level:x,bitrate:2000,format:flac,...`), producing invalid JSON — the
/// upstream regex normalizer fails on every live albuminfo body for exactly
/// that reason. A `rformat=json` parameter does not make every CDN node
/// answer valid JSON.
fn parse_kuwo_json(text: &str) -> Result<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Ok(value);
    }
    let normalized = normalize_kuwo_literal(text);
    let trimmed = normalized.trim().trim_end_matches(';');
    serde_json::from_str(trimmed).map_err(|e| perr(format!("kuwo r.s literal parse failed: {e}")))
}

/// Character-level JS-object-literal → JSON rewrite behind
/// `parse_kuwo_json`: bare identifiers directly after `{` or `,` followed by
/// `:` get double quotes (keys); single-quoted spans become double-quoted
/// strings with embedded `"`/`\` re-escaped; everything inside a string is
/// passed through verbatim, so value-internal `foo:bar` pairs survive.
fn normalize_kuwo_literal(text: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        SingleQuoted,
        DoubleQuoted,
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut state = State::Normal;
    // Last significant (non-space) character seen outside a string: a bare
    // key can only start right after `{` or `,`.
    let mut prev_sig = '\0';
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match state {
            State::Normal => {
                if c == '\'' {
                    state = State::SingleQuoted;
                    out.push('"');
                    prev_sig = '"';
                } else if c == '"' {
                    state = State::DoubleQuoted;
                    out.push('"');
                    prev_sig = '"';
                } else if (c.is_ascii_alphabetic() || c == '_')
                    && (prev_sig == '{' || prev_sig == ',')
                {
                    // Bare-key candidate: consume the identifier, then
                    // require a `:` (after optional spaces) to quote it.
                    let mut key = String::new();
                    key.push(c);
                    while let Some(&next) = chars.peek() {
                        if next.is_ascii_alphanumeric() || next == '_' {
                            key.push(next);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    let mut spaces = String::new();
                    while let Some(&next) = chars.peek() {
                        if next.is_whitespace() {
                            spaces.push(next);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if chars.peek() == Some(&':') {
                        chars.next();
                        out.push('"');
                        out.push_str(&key);
                        out.push_str("\":");
                        prev_sig = ':';
                    } else {
                        out.push_str(&key);
                        out.push_str(&spaces);
                        prev_sig = '_';
                    }
                } else {
                    if !c.is_whitespace() {
                        prev_sig = c;
                    }
                    out.push(c);
                }
            }
            State::SingleQuoted => match c {
                '\'' => {
                    state = State::Normal;
                    out.push('"');
                    prev_sig = '"';
                }
                '\\' => {
                    out.push('\\');
                    if let Some(&next) = chars.peek() {
                        chars.next();
                        match next {
                            // JS `\'` needs no escape inside a JSON
                            // double-quoted string.
                            '\'' => out.push('\''),
                            '"' => out.push_str("\\\""),
                            _ => out.push(next),
                        }
                    }
                }
                // A raw double quote must be escaped inside the JSON string.
                '"' => out.push_str("\\\""),
                _ => out.push(c),
            },
            State::DoubleQuoted => match c {
                '"' => {
                    state = State::Normal;
                    out.push('"');
                    prev_sig = '"';
                }
                '\\' => {
                    out.push('\\');
                    if let Some(&next) = chars.peek() {
                        chars.next();
                        out.push(next);
                    }
                }
                _ => out.push(c),
            },
        }
    }
    out
}

/// Synthetic playlist ids look like `digest-{n}__{id}`. A digest of `5`
/// marks a node id that must be resolved through qukudata before the pl.svc
/// detail call can use it.
fn parse_id(raw_id: &str) -> (Option<&str>, &str) {
    let Some((prefix, id)) = raw_id.split_once("__") else {
        return (None, raw_id);
    };
    (prefix.strip_prefix("digest-"), id)
}

/// Option-flavoured `scalar_u64` for fields where "missing" must be
/// distinguished from zero (the detail `total` falls back to the page length).
fn scalar_u64_opt(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn non_empty_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

/// Detail ladder page count: ceil(total/effective) capped at MAX_DETAIL_PAGES
/// (playlists beyond ~100k songs are silently truncated upstream too).
fn detail_page_count(total: u64, effective: u64) -> u64 {
    if effective == 0 {
        return 0;
    }
    total.div_ceil(effective).min(u64::from(MAX_DETAIL_PAGES))
}

/// Pipe-separated `formats` token map shared by the pl.svc music rows; an
/// empty or unrecognized set falls back to Low128 upstream.
fn parse_formats_qualities(formats: &str) -> Vec<StreamQuality> {
    let mut qualities: Vec<StreamQuality> = Vec::new();
    for format in formats.split('|') {
        let quality = match format {
            "MP3128" | "WMA128" => StreamQuality::Low,
            "MP3H" => StreamQuality::High,
            "ALFLAC" => StreamQuality::Lossless,
            "HIRFLAC" | "HIRES" => StreamQuality::HiRes,
            _ => continue,
        };
        if !qualities.contains(&quality) {
            qualities.push(quality);
        }
    }
    if qualities.is_empty() {
        qualities.push(StreamQuality::Low);
    }
    qualities
}

/// wapi playlist rows (`parse_playlist`): id/name/digest/img/total/desc/
/// listencnt. A non-empty digest composes the synthetic
/// `digest-{digest}__{id}` id; entries without id or name are dropped.
/// `listencnt` (play count) and the upstream `link` field have no slot in
/// the musid-dli `Playlist` model and are dropped.
fn parse_playlist_item(item: &Value) -> Option<Playlist> {
    let id = scalar_text(&item["id"]);
    let title = item["name"].as_str()?.trim().to_owned();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    let digest = scalar_text(&item["digest"]);
    Some(Playlist {
        id: TrackId::new(
            ID,
            if digest.is_empty() {
                id
            } else {
                format!("digest-{digest}__{id}")
            },
        ),
        title,
        description: non_empty_string(&item["desc"]),
        artwork_url: non_empty_string(&item["img"]),
        track_count: scalar_u64_opt(&item["total"]).map(|total| total as u32),
        tracks: vec![],
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
    })
}

/// r.s search rows: field names vary in case between nodes
/// (`playlistid`/`PLAYLISTID`/`playlistID`), so each field is probed as
/// written, upper-cased and lower-cased.
fn field<'a>(item: &'a Value, name: &str) -> Option<&'a Value> {
    item.get(name)
        .or_else(|| item.get(name.to_ascii_uppercase()))
        .or_else(|| item.get(name.to_ascii_lowercase()))
}

/// Playlist-search result rows (`parse_searched_playlist`): fields
/// playlistid/name/pic/songnum/nickname/intro; the `_150.` cover URL is
/// rewritten to `_700.`. Entries without id or name are dropped; `nickname`
/// has no slot in the musid-dli model.
fn parse_searched_playlist(item: &Value) -> Option<Playlist> {
    let id = field(item, "playlistid").map(scalar_text)?;
    let title = field(item, "name")?.as_str().map(str::trim)?;
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(Playlist {
        id: TrackId::new(ID, id.clone()),
        title: title.to_owned(),
        description: field(item, "intro").and_then(non_empty_string),
        artwork_url: field(item, "pic")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(|url| url.replace("_150.", "_700.")),
        track_count: scalar_u64_opt(field(item, "songnum").unwrap_or(&Value::Null))
            .map(|count| count as u32),
        tracks: vec![],
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
    })
}

/// getTagList rows: groups of `{name, data[]}` whose items carry
/// id/name/digest/extend. Legacy categories (digest set to anything but
/// "10000") are kept by the server but have no content: skipped. A synthetic
/// leading 全部 category (empty id) fronts the list.
fn parse_playlist_categories(value: &Value) -> Vec<PlaylistCategory> {
    let mut categories = vec![PlaylistCategory {
        id: String::new(),
        name: "全部".into(),
        group: Some("全部".into()),
        count: 0,
        hot: true,
        provider: Some(ID.to_owned()),
        extra: Default::default(),
    }];
    for group in value["data"].as_array().into_iter().flatten() {
        let group_name = group["name"].as_str().unwrap_or_default().trim().to_owned();
        for item in group["data"].as_array().into_iter().flatten() {
            let id = scalar_text(&item["id"]);
            let name = item["name"].as_str().unwrap_or_default().trim().to_owned();
            let digest = scalar_text(&item["digest"]);
            if id.is_empty() || name.is_empty() || (!digest.is_empty() && digest != "10000") {
                continue;
            }
            categories.push(PlaylistCategory {
                id,
                name,
                group: (!group_name.is_empty()).then(|| group_name.clone()),
                count: 0,
                hot: item["extend"]
                    .as_str()
                    .is_some_and(|text| text.to_ascii_uppercase().contains("HOT")),
                provider: Some(ID.to_owned()),
                extra: Default::default(),
            });
        }
    }
    categories
}

/// pl.svc music rows (the `kw/leaderboard.rs` parse_song shape): id, name,
/// artist (`&`-joined upstream), album, albumid, song_duration|duration,
/// pic and the pipe-separated `formats` tokens. Rows without an id are
/// dropped; the artist list is split to fit the musid-dli `Track` model.
fn parse_detail_song(item: &Value) -> Option<Track> {
    let song_id = scalar_text(&item["id"]);
    if song_id.is_empty() {
        return None;
    }
    let artists: Vec<String> = item["artist"]
        .as_str()
        .unwrap_or_default()
        .split('&')
        .filter(|artist| !artist.trim().is_empty())
        .map(|artist| artist.trim().to_owned())
        .collect();
    let duration = match scalar_u64_opt(&item["song_duration"]) {
        Some(duration) => duration,
        None => scalar_u64(&item["duration"]),
    };
    Some(Track {
        id: TrackId::new(ID, song_id),
        title: item["name"].as_str().unwrap_or_default().to_owned(),
        artists,
        album: non_empty_string(&item["album"]),
        album_id: non_empty_string(&item["albumid"]),
        duration: (duration > 0).then(|| Duration::from_secs(duration)),
        artwork_url: non_empty_string(&item["pic"]),
        isrc: None,
        available_qualities: parse_formats_qualities(item["formats"].as_str().unwrap_or_default()),
        audio: None,
        extra: BTreeMap::new(),
    })
}

fn append_unique_songs(songs: &mut Vec<Track>, seen: &mut HashSet<String>, items: &[Value]) {
    for song in items.iter().filter_map(parse_detail_song) {
        if seen.insert(song.id.id.clone()) {
            songs.push(song);
        }
    }
}

/// qukudata node-info reply: only `child[0].sourceid` is read; empty child,
/// blank sourceid or a missing `child` array are all NotFound upstream (no
/// fallback path exists).
fn parse_node_sourceid(json: &Value) -> Option<String> {
    json["child"]
        .as_array()
        .and_then(|items| items.first())
        .map(|item| scalar_text(&item["sourceid"]))
        .filter(|value| !value.is_empty())
}

// ── Chart parsing (qukudata board tree + kbangserver board songs;
//    kw/leaderboard.rs get_boards/get_list, audit 2026-11-18) ─────────────

/// Board catalogue: qukudata tree node 2, one rn=1000 page pinned upstream —
/// more than 1000 boards truncate (32 live children, not triggered).
fn chart_catalogue_url() -> String {
    format!("{PLAYLIST_NODE_URL}?op=query&cont=tree&node=2&pn=0&rn=1000&fmt=json&level=2")
}

/// Board songs: `pn` is a 0-based server offset (upstream sends
/// `page.saturating_sub(1)`), `rn` the row cap.
fn chart_songs_url(board_id: &str, pn: u32, rn: u32) -> String {
    format!(
        "{CHART_SONGS_URL}?from=pc&fmt=json&pn={pn}&rn={rn}&type=bang&data=content&id={board_id}&show_copyright_off=0&pcmp4=1&isbang=1"
    )
}

/// Catalogue rows: only `source == "1"` (Kuwo's own boards; the live
/// source=="27" row is an external link whose `sourceid` is a tencentmusic
/// URL) and the board id is the `sourceid` field, not the tree `id`. Blank
/// sourceid or name drops the row; `info` (free text like 更新于2024-08-15)
/// maps to Chart.description. A body without `child` and a catalogue with no
/// Kuwo board left are the upstream get_boards parse failures.
fn parse_chart_catalogue(json: &Value) -> Result<Vec<Chart>> {
    let children = json["child"]
        .as_array()
        .ok_or_else(|| perr("chart catalogue is missing the child tree"))?;
    let boards: Vec<Chart> = children
        .iter()
        .filter(|board| board["source"].as_str() == Some("1"))
        .filter_map(|board| {
            let id = scalar_text(&board["sourceid"]);
            let title = board["name"].as_str()?.trim().to_owned();
            if id.is_empty() || title.is_empty() {
                return None;
            }
            Some(Chart {
                id: TrackId::new(ID, id),
                title,
                description: non_empty_string(&board["info"]),
                updated_description: None,
            })
        })
        .collect();
    if boards.is_empty() {
        return Err(perr("catalogue carries no usable Kuwo board"));
    }
    Ok(boards)
}

/// Board rows reuse the pl.svc row shape (`parse_detail_song`, the
/// kw/leaderboard.rs parse_song shape): rows without an id are dropped,
/// `song_duration` wins over `duration` (live rows carry a bogus
/// `duration`), artists split on `&`, `formats` tokens map to quality
/// tiers with the Low128 floor. Entries keep list order and are ranked
/// 1..N afterwards, so ranks stay contiguous. A body without `musiclist`
/// is NotFound: live bad board ids answer HTTP 200 + `{}` (upstream
/// misclassifies that as a parse failure; a missing board is genuinely
/// not-found). A present-but-empty `musiclist` stays a valid empty page.
/// The top-level `num` and the upstream `has_more` flag have no slot in the
/// musid-dli chart model and are dropped, like the board name/info/pub
/// metadata.
fn parse_chart_entries(json: &Value) -> Result<Vec<ChartEntry>> {
    let rows = json["musiclist"].as_array().ok_or(Error::NotFound)?;
    Ok(rows
        .iter()
        .filter_map(parse_detail_song)
        .enumerate()
        .map(|(index, track)| ChartEntry {
            rank: (index + 1) as u32,
            track,
        })
        .collect())
}

/// pl.svc gate: the body carries `result=="ok"` instead of a `code` field.
fn ensure_detail_ok(json: &Value) -> Result<()> {
    if json["result"].as_str() != Some("ok") {
        return Err(perr("playlist detail request failed"));
    }
    Ok(())
}

/// Metadata half of the detail body (`get_detail_with_meta`): a missing
/// title falls back to `酷我歌单 {id}`, `total` falls back to the parsed
/// song count. `playnum` and `uname` have no slot in the musid-dli
/// `Playlist` model and are dropped.
fn parse_playlist_meta(json: &Value, id: &str, song_count: usize) -> Playlist {
    let track_count = scalar_u64_opt(&json["total"]).unwrap_or(song_count as u64);
    Playlist {
        id: TrackId::new(ID, id.to_owned()),
        title: json["title"]
            .as_str()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("酷我歌单 {id}")),
        description: non_empty_string(&json["info"]),
        artwork_url: non_empty_string(&json["pic"]),
        track_count: Some(track_count as u32),
        // The full track list comes from `playlist_tracks`.
        tracks: vec![],
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
    }
}

// ── Album parsing (r.s stype=albuminfo; audit 2026-11-18) ───────────────

/// The albuminfo route: `stype=albuminfo` on the same search.kuwo.cn/r.s
/// host as the song/playlist search. `alflac=1&pcmp4=1` request the
/// FLAC/MP4 entries inside `N_MINFO`; `pn=0&rn=1000` are pinned upstream
/// with no pagination reference, so albums beyond 1000 rows truncate.
fn album_info_url(album_id: &str) -> String {
    format!(
        "{SEARCH_URL}?pn=0&rn=1000&stype=albuminfo&albumid={album_id}&alflac=1&pcmp4=1&encoding=utf8&vipver=MUSIC_8.7.7.0_W4"
    )
}

/// Shape gate: a recognizable albuminfo body carries at least the top-level
/// `name` or the `musiclist` rows. There is no upstream reference for the
/// not-found body shape (the audit never captured one), so anything without
/// either field is treated as a failed lookup instead of an empty album.
fn ensure_album_shape(json: &Value) -> Result<()> {
    if json.get("name").is_none() && json.get("musiclist").is_none() {
        return Err(perr("album info response carries no album fields"));
    }
    Ok(())
}

/// Album metadata from the albuminfo top level. No upstream code consumes
/// these fields (kw/playlist.rs reads only `musiclist`), so this mapping is
/// musid-dli's own design over the live-observed top-level schema
/// (albumid/name/artist/artistid/songnum/pub/company/lang/pic/hts_img/...):
/// `name`/`artist`/`hts_img`/`songnum` map onto the `Album` model.
/// `pub`/`company`/`lang`/`artistid`/`pay`/`vip` have no `Album` slot and
/// are dropped, as is the relative-path `pic` (no upstream cover-URL
/// composition exists; `hts_img` is the only observed absolute URL). A body
/// without `name` falls back to `酷我专辑 {id}`.
fn parse_album_meta(json: &Value, requested_id: &str) -> Album {
    let top_album_id = scalar_text(&json["albumid"]);
    let album_id = if top_album_id.is_empty() {
        requested_id.to_owned()
    } else {
        top_album_id
    };
    let artists: Vec<String> = json["artist"]
        .as_str()
        .unwrap_or_default()
        .split('&')
        .filter(|artist| !artist.trim().is_empty())
        .map(|artist| artist.trim().to_owned())
        .collect();
    Album {
        id: TrackId::new(ID, album_id.clone()),
        title: json["name"]
            .as_str()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("酷我专辑 {album_id}")),
        artists,
        artwork_url: non_empty_string(&json["hts_img"]).filter(|url| is_http_url(url)),
        track_count: scalar_u64_opt(&json["songnum"]).map(|count| count as u32),
    }
}

/// One albuminfo `musiclist` row. The live schema carries two naming
/// families on the same row — the pl.svc family (`id`/`name`/`artist`/
/// `album`/`albumId`/`formats`) and the r.s family (`musicrid`/`songname`/
/// `aartist`/`N_MINFO`/`pic120`). Unlike the r.s music-search rows, the
/// albuminfo `musicrid` is a **bare** id without the `MUSIC_` prefix (live
/// albumid=1293: every row bare), so the prefix is stripped when present and
/// the id used as-is otherwise; upstream's
/// `strip_prefix("MUSIC_").unwrap_or_default()` emptied every live row and
/// silently dropped them all. The row's album id is the camelCase numeric
/// `albumId` (the three-way `albumid` case probe misses it), falling back to
/// the requested album id. `artist` wins over `aartist`, which may carry raw
/// HTML entities (`Jay&nbsp;Chou`) that are passed through undecoded (no
/// upstream decode exists). Qualities come from `N_MINFO` with the
/// pipe-separated `formats` tokens as fallback, floored at Low like the
/// pl.svc rows.
fn parse_album_row(item: &Value, album_id_fallback: &str) -> Option<Track> {
    let raw_rid = field(item, "musicrid").map(scalar_text).unwrap_or_default();
    let mut song_id = raw_rid
        .strip_prefix("MUSIC_")
        .unwrap_or(&raw_rid)
        .to_owned();
    if song_id.is_empty() {
        song_id = scalar_text(&item["id"]);
    }
    if song_id.is_empty() {
        return None;
    }
    let title = field(item, "songname")
        .or_else(|| field(item, "name"))
        .map(scalar_text)
        .unwrap_or_default();
    let artist_text = field(item, "artist")
        .map(scalar_text)
        .filter(|artist| !artist.is_empty())
        .unwrap_or_else(|| scalar_text(&item["aartist"]));
    let artists: Vec<String> = artist_text
        .split('&')
        .filter(|artist| !artist.trim().is_empty())
        .map(|artist| artist.trim().to_owned())
        .collect();
    let album_id = item
        .get("albumId")
        .or_else(|| field(item, "albumid"))
        .map(scalar_text)
        .filter(|album| !album.is_empty())
        .unwrap_or_else(|| album_id_fallback.to_owned());
    let duration = scalar_u64(&item["duration"]);
    let n_minfo = field(item, "N_MINFO")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let qualities = parse_n_minfo_qualities(n_minfo);
    let qualities = if qualities.is_empty() {
        parse_formats_qualities(
            field(item, "formats")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
    } else {
        qualities
    };
    Some(Track {
        id: TrackId::new(ID, song_id),
        title,
        artists,
        album: non_empty_string(&item["album"]),
        album_id: Some(album_id),
        duration: (duration > 0).then(|| Duration::from_secs(duration)),
        artwork_url: non_empty_string(&item["pic120"]),
        isrc: None,
        available_qualities: qualities,
        audio: None,
        extra: BTreeMap::new(),
    })
}

// ── Link parsing (share links; kw/parse.rs, audit 2026-11-18) ───────────

/// Link targets (kw/parse.rs:17-21): only two shapes exist — the song share
/// page and the playlist share page. No album or artist form is produced
/// upstream (the kw parse_link router has Song/Playlist branches only).
#[derive(Debug, Clone, PartialEq, Eq)]
enum KwLink {
    Song { rid: String },
    Playlist { id: String },
}

/// Link target extraction (kw/parse.rs:23-32): the link is trimmed, then an
/// unanchored `play_detail/(\d+)` capture wins first, followed by
/// `playlist_detail/(\d+)`. There is no scheme/host validation upstream, so
/// a `play_detail/` fragment anywhere in the string matches (documented
/// audit gap, inherited). Anything else is unrecognized.
fn kw_link_target(link: &str) -> Result<KwLink> {
    let trimmed = link.trim();
    if let Some(rid) = kw_capture(r"play_detail/(\d+)", trimmed) {
        return Ok(KwLink::Song { rid });
    }
    if let Some(id) = kw_capture(r"playlist_detail/(\d+)", trimmed) {
        return Ok(KwLink::Playlist { id });
    }
    Err(Error::InvalidInput(
        "unrecognized Kuwo song/playlist link".into(),
    ))
}

/// The per-call regex compile mirrors upstream `capture` (kw/parse.rs:34-40);
/// captures are unanchored (first match anywhere).
fn kw_capture(pattern: &'static str, value: &str) -> Option<String> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<&'static str, Regex>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    let regex = match guard.get(pattern) {
        Some(regex) => regex,
        None => {
            let compiled = Regex::new(pattern).ok()?;
            guard.insert(pattern, compiled);
            guard.get(pattern).expect("just inserted")
        }
    };
    regex
        .captures(value)
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str().to_owned())
}

/// The song-link route with the pinned query shape (`musicId` + `httpsStatus`,
/// kw/parse.rs:63-65).
fn song_info_url(rid: &str) -> String {
    format!("{SONG_INFO_URL}?musicId={rid}&httpsStatus=1")
}

/// songinfoandlrc body → metadata-only Track (kw/parse.rs:73-96). Only
/// `songName`/`artist`/`album`/`pic`/`duration` are read plus the
/// `extra["rid"]` slot: the live body also carries
/// pay/isdownload/coopFormats/formats/mp3Nsig, but upstream reads none of
/// them, so the paid/quality signal is lost here (inherited gap) and the
/// qualities list stays empty. An empty `songName` — including the live
/// `status:301 + data:null` failure envelope, which serde-parses fine — is
/// the upstream NotFound (parse.rs:79-81); the same response's `lrclist`
/// is ignored (lyrics have their own endpoint path, kw/lyric.rs).
///
/// Upstream keeps `artist` as one raw string; here it is split on `&` like
/// the module's other row shapes (parse_detail_song), and an empty `album`
/// maps to None via the module's `non_empty_string` convention.
fn parse_song_info(json: &Value, rid: &str) -> Option<Track> {
    let info = &json["data"]["songinfo"];
    let title = info["songName"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if title.is_empty() {
        return None;
    }
    let artists: Vec<String> = info["artist"]
        .as_str()
        .unwrap_or_default()
        .split('&')
        .filter(|artist| !artist.trim().is_empty())
        .map(|artist| artist.trim().to_owned())
        .collect();
    let duration = scalar_u64(&info["duration"]);
    let mut extra = BTreeMap::new();
    extra.insert("rid".to_owned(), rid.to_owned());
    Some(Track {
        id: TrackId::new(ID, rid),
        title,
        artists,
        album: non_empty_string(&info["album"]),
        album_id: None,
        duration: (duration > 0).then(|| Duration::from_secs(duration)),
        artwork_url: non_empty_string(&info["pic"]),
        isrc: None,
        available_qualities: vec![],
        audio: None,
        extra,
    })
}

/// Page-0 detail body → (metadata, parsed songs), the `get_detail_with_meta`
/// shape (kw/playlist.rs:209-231) shared by `fetch_playlist_meta` and the
/// link-resolver playlist branch: an empty/missing `musiclist` is NotFound
/// upstream, and `track_count` stays the full `total` even where a node caps
/// the rows at ~10 (upstream never paginates this path; inherited). The
/// upstream `link` backfill (`http://www.kuwo.cn/playlist_detail/{id}`) has
/// no slot in the musid-dli `Playlist` model and is dropped.
fn parse_playlist_with_songs(json: &Value, id: &str) -> Result<(Playlist, Vec<Track>)> {
    let songs: Vec<Track> = json["musiclist"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_detail_song)
        .collect();
    if songs.is_empty() {
        return Err(Error::NotFound);
    }
    Ok((parse_playlist_meta(json, id, songs.len()), songs))
}

// ── Playlist fetchers (wapi / pl.svc / r.s, anonymous) ──────────────────
impl KuwoProvider {
    /// Hot-recommendation ladder: Kuwo sometimes ignores `rn=36` and answers
    /// with only ten rows, so server pages are pulled up to
    /// MAX_PLAYLIST_REQUESTS times, deduped by id, until the logical page is
    /// filled or a page comes back empty/full. This call path uses
    /// appUid=76039576 without a Referer. (Upstream's category flow also has
    /// a second shape of the same endpoint for the empty-category fallback —
    /// appUid=38668888 with Referer, kw/playlist.rs:367-371 — that musid-dli
    /// does not wire; the trait's empty category routes here.)
    async fn fetch_hot_playlists(&self, page: u32, fill: usize) -> Result<Vec<Playlist>> {
        let mut playlists = Vec::with_capacity(PLAYLIST_PAGE_SIZE as usize);
        let mut seen = HashSet::new();
        for offset in 0..MAX_PLAYLIST_REQUESTS {
            let items = self.fetch_hot_page(page.saturating_add(offset)).await?;
            let item_count = items.len();
            for item in &items {
                if let Some(playlist) = parse_playlist_item(item)
                    && seen.insert(playlist.id.id.clone())
                {
                    playlists.push(playlist);
                    if playlists.len() >= fill {
                        return Ok(playlists);
                    }
                }
            }
            if item_count == 0 || item_count >= PLAYLIST_PAGE_SIZE as usize {
                break;
            }
        }
        Ok(playlists)
    }

    /// One hot-recommend server page: `code == 200` gated, entries in
    /// `data.data[]`.
    async fn fetch_hot_page(&self, page: u32) -> Result<Vec<Value>> {
        let url = format!(
            "{WAPI_PLAYLIST_URL}/getRcmPlayList?loginUid=0&loginSid=0&appUid=76039576&pn={page}&rn={PLAYLIST_PAGE_SIZE}&order=hot"
        );
        let json = self.get_json(&url, "hot playlists").await?;
        json["data"]["data"]
            .as_array()
            .cloned()
            .ok_or_else(|| perr("hot playlist list is missing"))
    }

    /// Category directory (getTagList): appUid=38668888 with the web Referer.
    async fn fetch_categories(&self) -> Result<Vec<PlaylistCategory>> {
        let url = format!(
            "{WAPI_PLAYLIST_URL}/getTagList?cmd=rcm_keyword_playlist&user=0&prod=kwplayer_pc_9.1.1.2&vipver=9.1.1.2&source=kwplayer_pc_9.1.1.2&loginUid=0&loginSid=0&appUid=38668888"
        );
        let json = self
            .get_json_with(&url, "playlist categories", Some(PLAYLIST_REFERER))
            .await?;
        Ok(parse_playlist_categories(&json))
    }

    /// Per-tag list (getTagPlayList): appUid=38668888 with the web Referer,
    /// category id urlencoded, `pn` starts at 1; `rn` stays pinned at 36
    /// (no upstream basis for other sizes).
    async fn fetch_tag_playlists(&self, category: &str, page: u32) -> Result<Vec<Playlist>> {
        let url = format!(
            "{WAPI_PLAYLIST_URL}/getTagPlayList?loginUid=0&loginSid=0&appUid=38668888&pn={}&rn={PLAYLIST_PAGE_SIZE}&id={}",
            page.max(1),
            urlencoding::encode(category)
        );
        let json = self
            .get_json_with(&url, "tag playlists", Some(PLAYLIST_REFERER))
            .await?;
        Ok(json["data"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_playlist_item)
            .collect())
    }

    /// One pl.svc detail page: `pn` starts at 0, `rn=1000` upstream, gated on
    /// `result=="ok"` (the body carries no `code` field) and requires the
    /// web Referer.
    async fn fetch_detail_page(&self, id: &str, page: u32, page_size: u32) -> Result<Value> {
        let url = format!(
            "{PLAYLIST_DETAIL_URL}?op=getlistinfo&pid={id}&pn={page}&rn={page_size}&encode=utf8&keyset=pl2012&identity=kuwo&pcmp4=1&vipver=MUSIC_9.0.5.0_W1&newver=1"
        );
        let json = self
            .get_json_with(&url, "playlist detail", Some(PLAYLIST_REFERER))
            .await?;
        ensure_detail_ok(&json)?;
        Ok(json)
    }

    /// Synthetic id resolution: `digest-5` marks a node id that only the
    /// qukudata endpoint can turn into a real playlist id; no fallback path
    /// exists upstream, an unresolvable node is NotFound.
    async fn resolve_playlist_id(&self, raw_id: &str) -> Result<String> {
        let (digest, id) = parse_id(raw_id);
        if digest != Some("5") {
            return Ok(id.to_owned());
        }
        let url = format!(
            "{PLAYLIST_NODE_URL}?op=query&cont=ninfo&node={id}&pn=0&rn=1&fmt=json&src=mbox&level=2"
        );
        let json = self.get_json(&url, "playlist node resolve").await?;
        parse_node_sourceid(&json).ok_or(Error::NotFound)
    }

    /// Full paginated track list: the first rn=1000 response may carry ~10
    /// rows on some nodes, so the ladder continues with the effective page
    /// size taken from the first response, deduped by song id, until `total`
    /// is reached or MAX_DETAIL_PAGES pages have been fetched.
    async fn fetch_playlist_tracks(&self, raw_id: &str) -> Result<Vec<Track>> {
        let id = self.resolve_playlist_id(raw_id).await?;
        let json = self.fetch_detail_page(&id, 0, DETAIL_REQUEST_SIZE).await?;
        let raw_items = json["musiclist"]
            .as_array()
            .ok_or_else(|| perr("playlist song list is missing"))?;
        let total = scalar_u64_opt(&json["total"]).unwrap_or(raw_items.len() as u64);
        let mut songs = Vec::with_capacity(total.min(DETAIL_REQUEST_SIZE as u64) as usize);
        let mut seen = HashSet::new();
        append_unique_songs(&mut songs, &mut seen, raw_items);
        let effective = raw_items.len() as u64;
        if effective == 0 || songs.len() as u64 >= total {
            return Ok(songs);
        }
        for offset in 1..detail_page_count(total, effective) as u32 {
            let json = self
                .fetch_detail_page(&id, offset, effective as u32)
                .await?;
            let Some(items) = json["musiclist"].as_array() else {
                break;
            };
            if items.is_empty() {
                break;
            }
            append_unique_songs(&mut songs, &mut seen, items);
            if songs.len() as u64 >= total {
                break;
            }
        }
        Ok(songs)
    }

    /// Page-0 detail (upstream get_detail_with_meta, which fetches only this
    /// one page for the gate, the total fallback and the songs): the shared
    /// path behind `fetch_playlist_meta` and the link-resolver playlist
    /// branch.
    async fn fetch_playlist_detail(&self, raw_id: &str) -> Result<(Playlist, Vec<Track>)> {
        let id = self.resolve_playlist_id(raw_id).await?;
        let json = self.fetch_detail_page(&id, 0, DETAIL_REQUEST_SIZE).await?;
        parse_playlist_with_songs(&json, &id)
    }

    /// Detail metadata from the first page: an empty `musiclist` is NotFound
    /// (upstream get_detail_with_meta). The paginated track list comes from
    /// `playlist_tracks`.
    async fn fetch_playlist_meta(&self, raw_id: &str) -> Result<Playlist> {
        let (meta, _) = self.fetch_playlist_detail(raw_id).await?;
        Ok(meta)
    }

    /// One song-link metadata request (kw/parse.rs fetch_song): anonymous,
    /// web Referer only. The envelope carries no `code` field, so the shared
    /// `get_json` gate passes it through and the empty-songName check does
    /// the NotFound work — upstream behaves the same (it never reads
    /// `status`). Metadata only: no stream URL is resolved here.
    async fn fetch_song_info(&self, rid: &str) -> Result<Track> {
        let json = self
            .get_json_with(
                &song_info_url(rid),
                "song link info",
                Some(PLAYLIST_REFERER),
            )
            .await?;
        parse_song_info(&json, rid).ok_or(Error::NotFound)
    }

    /// One anonymous albuminfo request on the legacy r.s route (upstream
    /// kw/playlist.rs get_album_songs): no signature, no token, no cookie.
    /// Upstream sends the web Referer (live-checked 2026-11-18 to be
    /// optional, kept faithful here); the reply is the JS object literal, so
    /// this path reads text and normalizes before serde — `get_json`'s
    /// `response.json()` would fail on the body.
    async fn fetch_album_info(&self, album_id: &str) -> Result<Value> {
        let url = album_info_url(album_id);
        let response = self
            .raw_get(&url, "album info", Some(PLAYLIST_REFERER))
            .await?;
        let json = parse_kuwo_json(&response.text())?;
        ensure_album_shape(&json)?;
        Ok(json)
    }
}
// ── Albums (r.s stype=albuminfo, anonymous) ─────────────────────────────
#[async_trait]
impl AlbumProvider for KuwoProvider {
    /// Album metadata from the albuminfo top level; the field mapping has no
    /// upstream counterpart (kw/playlist.rs reads only `musiclist`) — see
    /// `parse_album_meta` for what is consumed and dropped.
    async fn album(&self, id: &str) -> Result<Album> {
        let album_id = id.trim();
        if album_id.is_empty() {
            return Err(Error::InvalidInput("album id is empty".into()));
        }
        let json = self.fetch_album_info(album_id).await?;
        Ok(parse_album_meta(&json, album_id))
    }
    /// Full track list in one request (`rn=1000` pinned upstream; albums
    /// beyond 1000 songs truncate — no pagination reference exists). A
    /// well-formed body with an empty `musiclist` is an explicit empty
    /// result: upstream conflates it with a parse failure
    /// (酷我专辑曲目列表为空), and that semantics is deliberately not copied.
    async fn album_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let album_id = id.trim();
        if album_id.is_empty() {
            return Err(Error::InvalidInput("album id is empty".into()));
        }
        let json = self.fetch_album_info(album_id).await?;
        let items = json["musiclist"]
            .as_array()
            .ok_or_else(|| perr("album song list is missing"))?;
        Ok(items
            .iter()
            .filter_map(|item| parse_album_row(item, album_id))
            .collect())
    }
}
impl MusicProvider for KuwoProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        "酷我音乐"
    }
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            lyrics: true,
            playlists: true,
            albums: true,
            charts: true,
            // kw/mod.rs declares link_parse and wires parse_link upstream.
            link_resolve: true,
            ..Default::default()
        }
    }
    /// The single `authentication` bool stays false (an anonymous web token
    /// is not an account login; the registration test pins the bool), but
    /// the descriptor's auth facet names the mechanism that does exist:
    /// `AuthCapability::Anonymous` is the documented kuwo web-token case
    /// (AUTH_REGISTRY_DESIGN.md §5.4), more precise than the `None` default
    /// derivation would claim.
    fn auth_capability(&self) -> AuthCapability {
        AuthCapability::Anonymous
    }
}
#[async_trait]
impl TrackSearch for KuwoProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.keyword.trim().is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        let page = q.page.max(1);
        let limit = q.page_size.clamp(1, 100);
        let json = self
            .fetch_search_page(q.keyword.trim(), page - 1, limit)
            .await?;
        self.parse_search_page(&json, limit)
    }
}
#[async_trait]
impl StreamResolver for KuwoProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to Kuwo".into()));
        }
        // Voicefox parity (kw/url.rs:239-256, MODEL_ALIGNMENT_DESIGN.md §8
        // "kuwo candidate parity"): the mobile ladder and the web-signed play
        // URL run concurrently, and the loser URL rides as a structured
        // fallback candidate instead of being dropped. musid keeps its tier
        // preference rather than upstream's fixed mobile-first rule: the
        // anonymous mobile endpoint cannot serve lossless (placeholder
        // audio, `mobile_tiers` downgrades those requests), so Lossless/HiRes
        // requests resolve on the web endpoint and every other tier resolves
        // on the mobile ladder. The web URL also carries the resolve itself
        // when every mobile tier fails ("网页直链既做候选地址，也在移动端失败
        // 时兜底"); the web API declares no tier, so that fallback reports
        // High/Mp3 like upstream (kw/url.rs:244-247) instead of the
        // registered pre-M3 lossless report (§2.7).
        let (web, mobile) = futures::join!(
            self.fetch_web_url(&track.id.id),
            self.fetch_mobile_url_ladder(&track.id.id, options.quality),
        );
        let web_url = web.ok();
        let wants_lossless = matches!(
            options.quality,
            StreamQuality::Lossless | StreamQuality::HiRes
        );
        match mobile {
            Ok((url, tier)) => {
                if wants_lossless && let Some(web_url) = web_url {
                    // The web URL wins the lossless primary; the mobile tier
                    // URL becomes the structured candidate with the tier it
                    // actually serves and the okhttp UA its CDN requires.
                    return Ok(StreamInfo {
                        url: web_url,
                        quality: StreamQuality::Lossless,
                        format: AudioFormat::Flac,
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
                        fallback_endpoints: vec![FallbackEndpoint {
                            url,
                            headers: vec![("User-Agent".into(), UA_MEDIA.into())],
                            expires_at_unix: None,
                            content_length: None,
                            quality: Some(tier.quality),
                        }],
                    });
                }
                // The web signed URL is the structured candidate (its tier is
                // undeclared, so `quality: None` keeps the primary-tier
                // assumption the legacy slot already encoded), and it rides
                // the same okhttp UA upstream applies to every kuwo URL.
                let fallback_endpoints = kuwo_web_candidates(web_url.as_deref(), &url);
                Ok(StreamInfo {
                    url,
                    quality: tier.quality,
                    format: AudioFormat::Mp3,
                    duration: track.duration,
                    expires_at_unix: None,
                    content_length: None,
                    checksum: None,
                    // The media CDN requires the okhttp UA or it 403s.
                    headers: vec![("User-Agent".into(), UA_MEDIA.into())],
                    fallback_urls: vec![],
                    index_range: None,
                    init_range: None,
                    max_chunk_size: None,
                    size_is_advisory: false,
                    checksum_algorithm: None,
                    fallback_endpoints,
                })
            }
            Err(mobile_error) => {
                let Some(web_url) = web_url else {
                    return Err(mobile_error);
                };
                Ok(StreamInfo {
                    url: web_url,
                    quality: if wants_lossless {
                        StreamQuality::Lossless
                    } else {
                        StreamQuality::High
                    },
                    format: if wants_lossless {
                        AudioFormat::Flac
                    } else {
                        AudioFormat::Mp3
                    },
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
    }
}

#[async_trait]
impl LyricsProvider for KuwoProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to Kuwo".into()));
        }
        let song_id = &track.id.id;
        // Upstream order (kw/lyric.rs:96-101): the word lyrics are requested
        // first and their failure is swallowed — many songs have no word
        // version. The plain lyric failure must propagate: swallowing it
        // would turn a transient error into a false "no lyrics".
        let word = self.fetch_raw_lyric(song_id, true).await.ok();
        let plain = self.fetch_raw_lyric(song_id, false).await?;
        Ok(lyric_document(word, plain))
    }
}
/// Build the lyric document from the two fetched payloads. Upstream keeps
/// the word lyric beside the plain one (`LyricData.lxlyric` next to
/// `lyric`); musid-dli has a single document, so the word lyrics win when
/// they decode to at least one karaoke line, and an absent / failed /
/// unparseable lrcx degrades to the plain document (upstream's parser
/// likewise yields no karaoke lines there and only the plain lyric shows).
/// Kuwo lrcx decrypts to the lx-music unified karaoke format — the same
/// `[ms,dur]` + `<rel,dur[,0]>` relative-tag shape as decrypted KRC — so the
/// document is labelled `LyricFormat::Krc` (syntactic family; `LyricFormat`
/// has no lx/lrcx variant). `original` carries the plain LRC rendering like
/// the kugou KRC document; the word timings live on `LyricLine.words`.
/// Kuwo's lyric endpoints provide no translation/romanization (upstream
/// `tlyric`/`rlyric` are always None), so those stay unset.
fn lyric_document(word: Option<String>, plain: String) -> Option<LyricDocument> {
    if plain.trim().is_empty() {
        return None;
    }
    let lines = word.map(|text| parse_lrcx(&text)).unwrap_or_default();
    if !lines.is_empty() {
        let original = lines
            .iter()
            .map(|line| format!("[{}]{}", format_timestamp(line.start_ms), line.text))
            .collect::<Vec<_>>()
            .join("\n");
        return Some(LyricDocument {
            format: LyricFormat::Krc,
            lines,
            original,
            translation: None,
            romanization: None,
        });
    }
    Some(LyricDocument {
        format: LyricFormat::Lrc,
        lines: parse_lrc(&plain),
        original: plain,
        translation: None,
        romanization: None,
    })
}
/// Kuwo's web session signing (CSRF + secret header flow) is not implemented
/// as an account login; the library only serves the anonymous paths, and the
/// only session is the locally generated anonymous web token whose presence
/// the auth state reports (no account identity is ever claimed).
#[async_trait]
impl AuthenticationProvider for KuwoProvider {
    /// Migrated onto the unified [`AuthState`] machine (§8 slice 4 wiring):
    /// the state comes from `KuwoProvider::auth_state` and the legacy
    /// three-state shape is its documented fold
    /// ([`AuthState::to_login_status`]). The previous hardcoded `LoggedOut`
    /// contradicted the `web_token` the provider writes on every web-signed
    /// call; the anonymous token is `Authenticated` with identity `None`
    /// (anonymous web identity, folded to `LoggedIn { user_id: None, .. }`
    /// per AUTH_REGISTRY_DESIGN.md §2.1), not an account login.
    async fn login_status(&self) -> Result<LoginStatus> {
        Ok(self.auth_state().to_login_status(None))
    }
    async fn logout(&self) -> Result<()> {
        self.sessions.clear(ID);
        Ok(())
    }
}
// ── Charts (qukudata board tree + kbangserver board songs, anonymous) ───
#[async_trait]
impl ChartProvider for KuwoProvider {
    /// Board catalogue from the qukudata tree (node 2): no signature, no
    /// cookie, no Referer; with `fmt=json` the reply is real JSON, so the
    /// shared `get_json` channel applies (no JS-literal normalization).
    async fn charts(&self) -> Result<Vec<Chart>> {
        let json = self
            .get_json(&chart_catalogue_url(), "chart catalogue")
            .await?;
        parse_chart_catalogue(&json)
    }
    /// One pinned page of CHART_PAGE_SIZE rows (`pn=0`): the trait has no
    /// page parameters and upstream has no pull-the-whole-chart call, so a
    /// whole-board read stays out of scope. The board id passes through
    /// verbatim, like upstream get_list.
    async fn chart_tracks(&self, id: &str) -> Result<Vec<ChartEntry>> {
        let board_id = id.trim();
        if board_id.is_empty() {
            return Err(Error::InvalidInput("chart id is empty".into()));
        }
        let url = chart_songs_url(board_id, 0, CHART_PAGE_SIZE);
        let json = self.get_json(&url, "chart tracks").await?;
        parse_chart_entries(&json)
    }
}
// ── Playlists (wapi/pl.svc/r.s, anonymous; loginUid=0/loginSid=0) ───────
#[async_trait]
impl CollectionProvider for KuwoProvider {
    /// Playlist metadata (first detail page); the paginated track list comes
    /// from `playlist_tracks`.
    async fn playlist(&self, id: &str) -> Result<Playlist> {
        let id = id.trim();
        if id.is_empty() {
            return Err(Error::InvalidInput("playlist id is empty".into()));
        }
        self.fetch_playlist_meta(id).await
    }
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let id = id.trim();
        if id.is_empty() {
            return Err(Error::InvalidInput("playlist id is empty".into()));
        }
        self.fetch_playlist_tracks(id).await
    }
}
// ── Link resolution (share links; kw/parse.rs) ──────────────────────────
#[async_trait]
impl LinkResolver for KuwoProvider {
    /// The two share-link shapes (kw/parse.rs:42-55): a song link fetches
    /// metadata only — paid songs have no stream URL there but must still be
    /// listable — and a playlist link reuses the get_detail_with_meta path,
    /// carrying the page-0 songs on the playlist (upstream returns
    /// playlist+songs in one ParsedLink::Playlist). No Album/Artist branch
    /// exists upstream; unrecognized links are InvalidInput.
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink> {
        match kw_link_target(url)? {
            KwLink::Song { rid } => Ok(ResolvedLink::Track(self.fetch_song_info(&rid).await?)),
            KwLink::Playlist { id } => {
                let (mut playlist, songs) = self.fetch_playlist_detail(&id).await?;
                playlist.tracks = songs;
                Ok(ResolvedLink::Playlist(playlist))
            }
        }
    }
}
#[async_trait]
impl PlaylistProvider for KuwoProvider {
    async fn categories(&self) -> Result<Vec<PlaylistCategory>> {
        self.fetch_categories().await
    }
    /// Empty category = the hot-recommendation ladder (page_size caps the
    /// logical fill at the upstream 36 — larger pages have no upstream
    /// basis); anything else is the per-tag list with `pn` from 1.
    async fn playlists(&self, category: &str, page: u32, page_size: u32) -> Result<Vec<Playlist>> {
        let category = category.trim();
        if category.is_empty() {
            let fill = page_size.clamp(1, PLAYLIST_PAGE_SIZE) as usize;
            return self.fetch_hot_playlists(page.max(1), fill).await;
        }
        self.fetch_tag_playlists(category, page).await
    }
    /// Same legacy r.s route as the song search with `ft=playlist`; the body
    /// is a JS object literal, so this path reads text and normalizes it
    /// before JSON parsing (`get_json`'s `response.json()` would fail on it).
    async fn search_playlists(&self, keyword: &str, page: u32) -> Result<Vec<Playlist>> {
        if keyword.trim().is_empty() {
            return Err(Error::InvalidInput(
                "playlist search keyword is empty".into(),
            ));
        }
        let url = format!(
            "{SEARCH_URL}?all={}&ft=playlist&itemset=web_2013&client=kt&pcmp4=1&geo=c&vipver=1&pn={}&rn=30&rformat=json&encoding=utf8",
            urlencoding::encode(keyword.trim()),
            page.saturating_sub(1)
        );
        let response = self.raw_get(&url, "playlist search", None).await?;
        let json = parse_kuwo_json(&response.text())?;
        Ok(json["abslist"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_searched_playlist)
            .collect())
    }
    /// Every Kuwo playlist endpoint hardcodes `loginUid=0&loginSid=0` and no
    /// upstream user-playlist implementation exists to migrate: this stays
    /// Unsupported instead of inventing an endpoint.
    async fn user_playlists(&self, _page: u32, _page_size: u32) -> Result<Vec<Playlist>> {
        Err(Error::Unsupported(ID.into()))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ProviderSession;
    use crate::transport::{HttpMethod, MockTransport};
    use std::sync::Arc;
    /// Loads a reconstructed vector/response fixture from tests/fixtures/kuwo
    /// (see the README there for the derivation of these shapes and vectors).
    macro_rules! fixture {
        ($name:expr) => {
            serde_json::from_str::<Value>(include_str!(concat!("../tests/fixtures/kuwo/", $name)))
                .expect("kuwo fixture must be valid JSON")
        };
    }

    #[test]
    fn parses_n_minfo_quality_string() {
        let qualities = parse_n_minfo_qualities(
            "level:bcms,bitrate:22000,format:mgg,size:22.11Mb;level:zply,bitrate:20900,format:mflac,size:178.32Mb;level:high,bitrate:320,format:mp3,size:10.2M;level:standard,bitrate:128,format:mp3,size:4.1M;level:super,bitrate:2000,format:flac,size:30M",
        );
        // Only the standard bitrates map; DRM formats (22000/20900) do not.
        assert!(qualities.contains(&StreamQuality::High));
        assert!(qualities.contains(&StreamQuality::Low));
        assert!(qualities.contains(&StreamQuality::Lossless));
        assert!(!qualities.contains(&StreamQuality::HiRes));
        assert!(parse_n_minfo_qualities("").is_empty());
    }
    #[test]
    fn mobile_tiers_downgrade_lossless_requests() {
        assert_eq!(mobile_tiers(StreamQuality::Low).len(), 1);
        assert_eq!(mobile_tiers(StreamQuality::Low)[0].br, "128kmp3");
        let tiers = mobile_tiers(StreamQuality::Lossless);
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0].br, "320kmp3");
        assert_eq!(tiers[1].br, "128kmp3");
    }
    #[test]
    fn placeholder_tracks_are_detectable() {
        // Paid song live response: rid 260839262 with an 11s duration for a
        // request of rid 228908.
        let placeholder = json_placeholder(260839262u64, 11u64);
        assert_ne!(scalar_text(&placeholder["data"]["rid"]), "228908");
        let good = json_placeholder(228908u64, 269u64);
        assert_eq!(scalar_text(&good["data"]["rid"]), "228908");
        assert_eq!(scalar_u64(&good["data"]["duration"]), 269);
        // String-shaped rid responses are equally handled.
        assert_eq!(scalar_text(&Value::String("228908".into())), "228908");
    }
    fn json_placeholder(rid: u64, duration: u64) -> Value {
        serde_json::json!({
            "code": 200,
            "data": {"rid": rid, "duration": duration, "bitrate": 1, "url": "http://kw-er.kuwo.cn/x.mp3"}
        })
    }
    #[test]
    fn only_http_urls_pass_the_filter() {
        assert!(is_http_url("http://kw-er.kuwo.cn/a.mp3"));
        assert!(is_http_url("https://kw-er.kuwo.cn/a.mp3"));
        assert!(!is_http_url("该歌曲为付费内容"));
        assert!(!is_http_url(""));
    }
    #[test]
    fn lyric_params_are_xor_encrypted_with_yeelion() {
        // XOR is symmetric: encrypt then decrypt round-trips.
        let encrypted = encrypt_lyric_params("rid=MUSIC_228908");
        let decoded = STANDARD.decode(encrypted).unwrap();
        let recovered: Vec<u8> = decoded
            .iter()
            .enumerate()
            .map(|(i, byte)| byte ^ LYRIC_XOR_KEY[i % LYRIC_XOR_KEY.len()])
            .collect();
        assert_eq!(String::from_utf8(recovered).unwrap(), "rid=MUSIC_228908");
    }
    #[test]
    fn decodes_lyric_body_with_header_and_gzip_like_shapes() {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;
        use std::io::Write;
        let payload = "[00:01.00]hello\r\n[00:02.00]world";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(payload.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut framed = b"tp=content\r\n\r\n".to_vec();
        framed.extend_from_slice(&compressed);
        let decoded = decode_lyric_body(&framed).unwrap();
        assert_eq!(decoded, payload);
        // Without the header frame the whole body is treated as zlib data.
        let decoded = decode_lyric_body(&compressed).unwrap();
        assert_eq!(decoded, payload);
    }
    #[test]
    fn kuwo_web_secret_matches_reference_vectors() {
        assert_eq!(build_secret("abc123", 12345678), "4232e893231400bc614e");
        assert_eq!(build_secret("Hm_X", 1), "6b3dd4fa00000001");
        assert_ne!(
            build_secret("abc123", 12345679),
            build_secret("abc123", 12345678)
        );
        assert_eq!(build_secret("abc123", 12345678).len(), 6 * 2 + 8);
    }
    #[test]
    fn kuwo_request_id_looks_like_uuid_v4() {
        let id = request_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_ne!(id, request_id());
    }

    // --- fixture tests: reconstructed vectors and response shapes, see
    // tests/fixtures/kuwo/README.md for the derivation of each file.

    #[test]
    fn web_secret_fixture_matches_golden_vectors() {
        let fixture = fixture!("web_secret.json");
        for vector in fixture["vectors"].as_array().unwrap() {
            let cookie = vector["cookie"].as_str().unwrap();
            let nonce = vector["nonce"].as_u64().unwrap() as u32;
            let secret = build_secret(cookie, nonce);
            assert_eq!(
                secret,
                vector["secret"].as_str().unwrap(),
                "cookie {cookie}"
            );
            // Length invariant: 2 hex digits per cookie byte + 8 nonce digits.
            assert_eq!(secret.len(), cookie.len() * 2 + 8);
            assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }
    #[test]
    fn lyric_params_fixture_matches_golden_ciphertexts() {
        let fixture = fixture!("lyric_params.json");
        for case in ["plain", "lrcx"] {
            let vector = &fixture[case];
            let params = vector["params"].as_str().unwrap();
            let encrypted = encrypt_lyric_params(params);
            assert_eq!(encrypted, vector["base64"].as_str().unwrap(), "case {case}");
            // XOR is symmetric: decrypting the ciphertext recovers the params.
            let decoded = STANDARD.decode(&encrypted).unwrap();
            let recovered: Vec<u8> = decoded
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ LYRIC_XOR_KEY[i % LYRIC_XOR_KEY.len()])
                .collect();
            assert_eq!(String::from_utf8(recovered).unwrap(), params);
        }
    }
    #[test]
    fn lyric_body_fixture_decodes_gb18030_payload() {
        // The GB18030 bytes are not valid UTF-8, so this pins the GB18030
        // (not UTF-8) decode; see the README for how the .bin files were
        // produced.
        let expected = "[00:01.00]测试歌词\r\n[00:02.00]second line";
        let framed = include_bytes!("../tests/fixtures/kuwo/lyric_framed.bin");
        assert_eq!(decode_lyric_body(framed).unwrap(), expected);
        let unframed = include_bytes!("../tests/fixtures/kuwo/lyric_unframed.bin");
        assert_eq!(decode_lyric_body(unframed).unwrap(), expected);
    }
    #[test]
    fn lrcx_body_fixture_decodes_to_lx_karaoke_text() {
        // End-to-end word-lyric body vector (formula-derived, verified with a
        // Python reimplementation of the upstream chain before pinning; see
        // tests/fixtures/kuwo/README.md): frame skip → zlib → ASCII filter →
        // non-alphabet-ignoring base64 → yeelion XOR → GB18030. The payload
        // carries four trailing junk bytes (0x9f 0x8e \x00 \x7f).
        let expected = "[21456,2914]<0,146,0>牵<146,135,0>着<281,205,0>你\n[24370,2810]<0,300,0>走<300,260,0>过";
        let framed = include_bytes!("../tests/fixtures/kuwo/lyric_lrcx_body.bin");
        assert_eq!(decode_lrcx_body(framed).unwrap(), expected);
    }
    #[test]
    fn lrcx_fixture_text_parses_into_word_timed_lines() {
        let framed = include_bytes!("../tests/fixtures/kuwo/lyric_lrcx_body.bin");
        let text = decode_lrcx_body(framed).unwrap();
        let lines = parse_lrcx(&text);
        assert_eq!(lines.len(), 2);
        let first = &lines[0];
        assert_eq!(first.start_ms, 21_456);
        assert_eq!(first.text, "牵着你");
        assert_eq!(first.words.len(), 3);
        // Word starts are offsets relative to the line head.
        assert_eq!(first.words[0].text, "牵");
        assert_eq!(first.words[0].start_ms, 21_456);
        assert_eq!(first.words[0].duration_ms, 146);
        assert_eq!(first.words[1].text, "着");
        assert_eq!(first.words[1].start_ms, 21_602);
        assert_eq!(first.words[1].duration_ms, 135);
        assert_eq!(first.words[2].text, "你");
        assert_eq!(first.words[2].start_ms, 21_737);
        assert_eq!(first.words[2].duration_ms, 205);
        let second = &lines[1];
        assert_eq!(second.start_ms, 24_370);
        assert_eq!(second.text, "走过");
        assert_eq!(second.words[0].text, "走");
        assert_eq!(second.words[0].start_ms, 24_370);
        assert_eq!(second.words[0].duration_ms, 300);
        assert_eq!(second.words[1].text, "过");
        assert_eq!(second.words[1].start_ms, 24_670);
        assert_eq!(second.words[1].duration_ms, 260);
    }
    #[test]
    fn parses_lx_karaoke_relative_word_timestamps() {
        // Upstream lx-lyric parser vector (voicefox yrc.rs:146-152): the
        // `[mm:ss.xxx]` line-head form with word starts relative to it.
        let lines = parse_lrcx("[00:12.340]<0,200>你<200,300>好");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].start_ms, 12_340);
        assert_eq!(lines[0].words[0].start_ms, 12_340);
        assert_eq!(lines[0].words[1].start_ms, 12_540);
        // 3-element tags over the numeric line-head form (the lrcx shape).
        let lines = parse_lrcx("[21456,2914]<0,146,0>牵<146,135,0>着");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].start_ms, 21_456);
        assert_eq!(lines[0].words[0].start_ms, 21_456);
        assert_eq!(lines[0].words[1].start_ms, 21_602);
        assert_eq!(lines[0].words[1].duration_ms, 135);
    }
    #[test]
    fn unparseable_lrcx_lines_are_dropped() {
        // Metadata-style tags and tag-less text produce no lines: the
        // document builder then falls back to the plain lyric document.
        assert!(parse_lrcx("[ti:demo]\ngarbage without tags").is_empty());
    }
    #[test]
    fn lyric_document_word_branch_wins_and_unusable_lrcx_falls_back() {
        // Word lyrics that decode to at least one karaoke line win.
        let doc = lyric_document(
            Some("[21456,2914]<0,146,0>牵<146,135,0>着<281,205,0>你".into()),
            "[00:21.45]牵着你".into(),
        )
        .expect("plain payload present");
        assert_eq!(doc.format, LyricFormat::Krc);
        assert_eq!(doc.lines.len(), 1);
        assert_eq!(doc.lines[0].words.len(), 3);
        // `original` carries the plain LRC rendering, like the kugou KRC
        // document; the word timings live on LyricLine.words.
        assert_eq!(doc.original, "[00:21.456]牵着你");
        // No lrcx payload (songs without a word version): plain document.
        let doc = lyric_document(None, "[00:01.00]hello".into()).expect("plain payload present");
        assert_eq!(doc.format, LyricFormat::Lrc);
        assert!(doc.lines[0].words.is_empty());
        // An lrcx payload that decodes to no lines degrades to plain too.
        let doc = lyric_document(Some("[ti:demo]".into()), "[00:01.00]hello".into())
            .expect("plain payload present");
        assert_eq!(doc.format, LyricFormat::Lrc);
        // An empty plain payload is None regardless of the word lyrics.
        assert!(lyric_document(Some("[21456,2914]<0,146,0>牵".into()), String::new()).is_none());
    }
    #[test]
    fn search_fixture_parses_tracks_counters_and_skips_bad_entries() {
        let fixture = fixture!("search_page.json");
        let page = KuwoProvider::new()
            .parse_search_page(&fixture, 2)
            .expect("fixture page parses");
        // The two malformed entries (blank rid, blank title) are skipped.
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total, Some(12));
        // has_more: 2 items >= limit 2 and SHOW 10 >= limit 2.
        assert!(page.has_more);
        let first = &page.items[0];
        assert_eq!(first.id.id, "228908");
        assert_eq!(first.title, "晴天");
        assert_eq!(
            first.artists,
            vec!["周杰伦".to_owned(), "费玉清".to_owned()]
        );
        assert_eq!(first.album.as_deref(), Some("叶惠美"));
        assert_eq!(first.album_id.as_deref(), Some("104004"));
        assert_eq!(first.duration, Some(Duration::from_secs(270)));
        // N_MINFO maps only the standard bitrates; DRM 22000/20900 stay unmapped.
        assert!(first.available_qualities.contains(&StreamQuality::High));
        assert!(first.available_qualities.contains(&StreamQuality::Low));
        assert!(first.available_qualities.contains(&StreamQuality::Lossless));
        assert!(!first.available_qualities.contains(&StreamQuality::HiRes));
        let second = &page.items[1];
        assert_eq!(second.id.id, "6550104");
        assert_eq!(second.title, "周边曲目");
        assert_eq!(second.artists, vec!["Alice".to_owned(), "Bob".to_owned()]);
        assert_eq!(second.album, None);
        assert_eq!(second.album_id, None);
        assert_eq!(second.duration, Some(Duration::from_secs(232)));
        assert!(second.available_qualities.is_empty());
        // A limit above both the page size and SHOW turns has_more off.
        let page = KuwoProvider::new()
            .parse_search_page(&fixture, 100)
            .unwrap();
        assert!(!page.has_more);
    }
    #[test]
    fn mobile_play_fixture_rejects_placeholder_and_serves_good_tracks() {
        let fixture = fixture!("mobile_play.json");
        let placeholder = &fixture["placeholder"];
        // The live-observed paid placeholder: rid 260839262 / 11 s. For a
        // request of rid 228908 the rid mismatch rejects the body.
        assert_eq!(scalar_text(&placeholder["data"]["rid"]), "260839262");
        assert_eq!(scalar_u64(&placeholder["data"]["duration"]), 11);
        assert!(matches!(
            validate_mobile_play(&placeholder["data"], "228908"),
            Err(Error::NotFound)
        ));
        let good = &fixture["good"];
        assert_eq!(scalar_text(&good["data"]["rid"]), "6550104");
        assert_eq!(scalar_u64(&good["data"]["duration"]), 232);
        assert_eq!(
            validate_mobile_play(&good["data"], "6550104").unwrap(),
            "http://kw-bj.kuwo.cn/x.mp3"
        );
        // A rid mismatch on an otherwise good body is equally rejected.
        assert!(matches!(
            validate_mobile_play(&good["data"], "228908"),
            Err(Error::NotFound)
        ));
        // String-shaped rid normalizes to the same acceptance.
        let string_rid = &fixture["good_string_rid"];
        assert_eq!(
            validate_mobile_play(&string_rid["data"], "6550104").unwrap(),
            "http://kw-bj.kuwo.cn/x.mp3"
        );
    }
    #[test]
    fn web_play_fixture_gates_code_and_url() {
        let fixture = fixture!("web_play.json");
        assert_eq!(
            parse_web_play_url(&fixture["success"].to_string()).unwrap(),
            "http://gec-kw-doc.kuwo.cn/example/example.flac"
        );
        // Paid content (code -1) and rights/region blocks are NotFound.
        assert!(matches!(
            parse_web_play_url(&fixture["paid"].to_string()),
            Err(Error::NotFound)
        ));
        assert!(matches!(
            parse_web_play_url(&fixture["region_block"].to_string()),
            Err(Error::NotFound)
        ));
        // A code-200 body whose data.url is the paid-content text is also
        // rejected by the URL filter.
        assert!(matches!(
            parse_web_play_url(&fixture["paid_text_url"].to_string()),
            Err(Error::NotFound)
        ));
        // The illegal-session marker is caught before the JSON gate is
        // reached (web_get inspects the raw text).
        let illegal = fixture["illegal_session"].as_str().unwrap();
        assert!(is_illegal_session(illegal));
    }
    #[test]
    fn url_filter_fixture_accepts_only_http_urls() {
        let fixture = fixture!("url_filter.json");
        for url in fixture["accepted"].as_array().unwrap() {
            let url = url.as_str().unwrap();
            assert!(is_http_url(url), "{url} should be accepted");
        }
        for url in fixture["rejected"].as_array().unwrap() {
            let url = url.as_str().unwrap();
            assert!(!is_http_url(url), "{url:?} should be rejected");
        }
    }

    // --- cover fixtures: shape vectors from the 2026-11-17 audit live
    // smoke (kw/url.rs:84-137), see tests/fixtures/kuwo/README.md. CDN
    // hosts/paths rotate, so the tests lock the response shape, not the
    // literal URL.

    #[test]
    fn cover_urls_match_the_upstream_routes() {
        // Tier one: web-signed musicInfo with httpsStatus=1 (kw/url.rs:25,
        // :94-97); the reqId query is appended later by web_get.
        assert_eq!(
            music_info_url("6550104"),
            "https://www.kuwo.cn/api/www/music/musicInfo?mid=6550104&httpsStatus=1"
        );
        // Tier two: plain-http artistpicserver with the hardcoded 500px
        // params (kw/url.rs:120-124) — no size negotiation anywhere.
        assert_eq!(
            artist_pic_url("6550104"),
            "http://artistpicserver.kuwo.cn/pic.web?corp=kuwo&type=rid_pic&pictype=500&size=500&rid=6550104"
        );
    }

    #[test]
    fn music_info_fixture_gates_code_and_pic_url() {
        let fixture = fixture!("cover_music_info.json");
        // Tier one success shape: code 200 with an absolute https pic.
        let success = parse_music_info_pic(&fixture["success"].to_string()).unwrap();
        assert!(success.starts_with("https://"), "{success} should be https");
        assert!(success.contains("albumcover"));
        // Invalid rid: code -1 (歌曲不存在) returns None so the two-level
        // fallback falls through to artistpicserver.
        assert!(parse_music_info_pic(&fixture["not_found"].to_string()).is_none());
        // A code-200 body whose pic is unusable is equally None: the empty
        // string and the missing field take the same is_http_url filter.
        assert!(parse_music_info_pic(&fixture["empty_pic"].to_string()).is_none());
        assert!(parse_music_info_pic(&fixture["missing_pic"].to_string()).is_none());
        // The paid-content text in place of a URL fails the filter.
        assert!(parse_music_info_pic(&fixture["paid_text_pic"].to_string()).is_none());
        // Malformed JSON is swallowed like upstream's from_str(...).ok()?.
        assert!(parse_music_info_pic("not json").is_none());
    }

    #[test]
    fn artist_pic_fixture_filters_no_pic_and_trims_the_url_line() {
        let fixture = fixture!("cover_artist_pic.json");
        let success = fixture["success"].as_str().unwrap();
        assert_eq!(parse_artist_pic_body(success).as_deref(), Some(success));
        // NO_PIC: HTTP 200 with a non-URL body must stay None — the filter
        // is the only defense against handing "NO_PIC" out as a URL.
        assert_eq!(
            parse_artist_pic_body(fixture["no_pic"].as_str().unwrap()),
            None
        );
        // The upstream .trim() covers whitespace-padded single-line bodies.
        assert_eq!(
            parse_artist_pic_body(fixture["padded"].as_str().unwrap()).as_deref(),
            Some(success)
        );
    }

    // --- playlist fixtures: shapes reconstructed from kw/playlist.rs and
    // kw/leaderboard.rs, see tests/fixtures/kuwo/README.md.

    #[test]
    fn playlist_search_fixture_normalizes_the_js_object_literal() {
        let fixture = fixture!("playlist_search.json");
        // Upstream inline vector: bare keys, single-quoted strings, trailing
        // semicolon; `total` parses back as the string "10".
        let json = parse_kuwo_json(fixture["literal"].as_str().unwrap()).expect("literal parses");
        assert_eq!(json["total"].as_str(), Some("10"));
        assert_eq!(json["abslist"][0]["playlistid"].as_str(), Some("123"));
        assert_eq!(json["abslist"][0]["name"].as_str(), Some("华语"));
        let playlist = parse_searched_playlist(&json["abslist"][0]).unwrap();
        assert_eq!(playlist.id.id, "123");
        assert_eq!(playlist.title, "华语");
        assert_eq!(playlist.description.as_deref(), Some("介绍"));
        assert_eq!(playlist.track_count, Some(5));
        assert_eq!(playlist.artwork_url, None);
        // Valid JSON passes through untouched.
        assert!(parse_kuwo_json(r#"{"a":1}"#).is_ok());
    }
    #[test]
    fn playlist_search_fixture_skips_bad_entries_and_falls_back_on_field_case() {
        let fixture = fixture!("playlist_search.json");
        // The empty playlistid and blank-name entries are dropped.
        let items: Vec<Playlist> = fixture["valid_json"]["abslist"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_searched_playlist)
            .collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.id, "1082685103");
        assert_eq!(items[0].title, "热门歌单");
        assert_eq!(items[0].track_count, Some(12));
        assert_eq!(items[0].description.as_deref(), Some("简介"));
        // `_150.` covers are rewritten to `_700.`.
        assert_eq!(
            items[0].artwork_url.as_deref(),
            Some("http://img4.kuwo.cn/star/playlist/p_700.jpg")
        );
        // All-upper-case field names fall back through `field`.
        let items: Vec<Playlist> = fixture["uppercase_fields"]["abslist"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_searched_playlist)
            .collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.id, "42");
        assert_eq!(items[0].title, "全大写字段");
        assert_eq!(items[0].track_count, Some(3));
        assert_eq!(
            items[0].artwork_url.as_deref(),
            Some("http://img4.kuwo.cn/star/playlist/q_700.jpg")
        );
    }
    #[test]
    fn playlist_list_fixture_parses_items_and_composes_digest_ids() {
        let fixture = fixture!("playlist_list.json");
        let playlists: Vec<Playlist> = fixture["data"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_playlist_item)
            .collect();
        // The empty-id and empty-name entries are dropped.
        assert_eq!(playlists.len(), 3);
        assert_eq!(playlists[0].id.id, "1082685103");
        assert_eq!(playlists[0].title, "热门华语精选");
        assert_eq!(playlists[0].track_count, Some(12));
        assert_eq!(
            playlists[0].description.as_deref(),
            Some("每日更新的华语热门歌单")
        );
        assert_eq!(
            playlists[0].artwork_url.as_deref(),
            Some("http://img4.kuwo.cn/star/playlist/a.jpg")
        );
        // A non-empty digest composes digest-{digest}__{id}; numeric
        // id/total values normalize to the same shapes.
        assert_eq!(playlists[1].id.id, "digest-3__1082685104");
        assert_eq!(playlists[1].track_count, Some(45));
        assert_eq!(playlists[1].description, None);
        assert_eq!(playlists[1].artwork_url, None);
        // digest-5 ids are node ids: they must be resolved before use.
        assert_eq!(playlists[2].id.id, "digest-5__1082685105");
    }
    #[test]
    fn synthetic_ids_split_through_parse_id() {
        assert_eq!(parse_id("1082685103"), (None, "1082685103"));
        assert_eq!(parse_id("digest-5__123"), (Some("5"), "123"));
        assert_eq!(parse_id("digest-3__1082685104"), (Some("3"), "1082685104"));
        assert_eq!(parse_id(""), (None, ""));
    }
    #[test]
    fn playlist_categories_fixture_filters_legacy_and_flags_hot() {
        let fixture = fixture!("playlist_categories.json");
        let categories = parse_playlist_categories(&fixture);
        // The synthetic 全部 category fronts the list.
        assert_eq!(categories[0].id, "");
        assert_eq!(categories[0].name, "全部");
        assert_eq!(categories[0].group.as_deref(), Some("全部"));
        assert!(categories[0].hot);
        let names: Vec<&str> = categories[1..].iter().map(|c| c.name.as_str()).collect();
        // 旧分类 (digest "9"), the empty-id and the empty-name rows dropped.
        assert_eq!(names, vec!["流行", "摇滚", "民谣", "无extend", "无组名"]);
        let by_name = |name: &str| {
            categories
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("missing category {name}"))
        };
        // extend "hot" (lowercase) still flags hot; digest "10000" is kept.
        assert!(by_name("流行").hot);
        assert!(by_name("摇滚").hot);
        assert!(!by_name("民谣").hot);
        assert!(!by_name("无extend").hot);
        assert!(by_name("无组名").hot);
        assert_eq!(by_name("流行").group.as_deref(), Some("风格"));
        // An empty group name produces no group.
        assert_eq!(by_name("无组名").group, None);
    }
    #[test]
    fn playlist_detail_fixture_parses_songs_formats_and_meta() {
        let fixture = fixture!("playlist_detail.json");
        let success = &fixture["success"];
        ensure_detail_ok(success).expect("ok result passes the gate");
        assert!(ensure_detail_ok(&fixture["failed"]).is_err());
        let songs: Vec<Track> = success["musiclist"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_detail_song)
            .collect();
        // The empty-id row is dropped.
        assert_eq!(songs.len(), 4);
        let first = &songs[0];
        assert_eq!(first.id.id, "228908");
        assert_eq!(first.title, "晴天");
        assert_eq!(
            first.artists,
            vec!["周杰伦".to_owned(), "费玉清".to_owned()]
        );
        assert_eq!(first.album.as_deref(), Some("叶惠美"));
        assert_eq!(first.album_id.as_deref(), Some("104004"));
        assert_eq!(first.duration, Some(Duration::from_secs(270)));
        assert_eq!(
            first.artwork_url.as_deref(),
            Some("http://img4.kuwo.cn/star/albumcover/s1.jpg")
        );
        assert_eq!(
            first.available_qualities,
            vec![
                StreamQuality::Low,
                StreamQuality::High,
                StreamQuality::Lossless,
                StreamQuality::HiRes
            ]
        );
        // song_duration falls back to duration; WMA128 maps to Low.
        let second = &songs[1];
        assert_eq!(second.id.id, "6550104");
        assert_eq!(second.duration, Some(Duration::from_secs(232)));
        assert_eq!(second.available_qualities, vec![StreamQuality::Low]);
        assert_eq!(second.album, None);
        assert_eq!(second.album_id, None);
        // An empty formats set floors at Low128.
        assert_eq!(songs[2].available_qualities, vec![StreamQuality::Low]);
        assert!(songs[2].artwork_url.is_none());
        // HIRES maps to HiRes alongside MP3128.
        assert_eq!(
            songs[3].available_qualities,
            vec![StreamQuality::Low, StreamQuality::HiRes]
        );
        // Metadata: total/title/info/pic (playnum/uname have no model slot).
        let meta = parse_playlist_meta(success, "1082685103", 4);
        assert_eq!(meta.id.id, "1082685103");
        assert_eq!(meta.title, "经典华语合集");
        assert_eq!(meta.track_count, Some(6));
        assert_eq!(meta.description.as_deref(), Some("歌单介绍"));
        assert_eq!(
            meta.artwork_url.as_deref(),
            Some("http://img4.kuwo.cn/star/playlist/d.jpg")
        );
        assert!(meta.tracks.is_empty());
        // A body without a title falls back to 酷我歌单 {id}.
        let meta = parse_playlist_meta(&fixture["no_title"], "77", 1);
        assert_eq!(meta.title, "酷我歌单 77");
        assert_eq!(meta.track_count, Some(1));
        // A body without musiclist parses no songs: the meta path is
        // NotFound (fetch_playlist_meta).
        let none: Vec<Track> = fixture["no_musiclist"]["musiclist"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_detail_song)
            .collect();
        assert!(none.is_empty());
    }
    #[test]
    fn playlist_digest_node_fixture_resolves_child_sourceid() {
        let fixture = fixture!("playlist_digest_node.json");
        assert_eq!(
            parse_node_sourceid(&fixture["resolved"]).as_deref(),
            Some("1082685103")
        );
        // Empty child, blank sourceid and missing child are unresolvable:
        // no fallback path exists upstream.
        assert!(parse_node_sourceid(&fixture["empty_child"]).is_none());
        assert!(parse_node_sourceid(&fixture["blank_sourceid"]).is_none());
        assert!(parse_node_sourceid(&fixture["no_child"]).is_none());
    }
    // --- link-parsing fixtures: the two share-link shapes and the
    // songinfoandlrc metadata body (kw/parse.rs, audit 2026-11-18), see
    // tests/fixtures/kuwo/README.md.

    #[test]
    fn link_target_fixtures_follow_upstream_parse() {
        let fixture = fixture!("link_targets.json");
        for case in fixture["positive"].as_array().unwrap() {
            let link = case["link"].as_str().unwrap();
            let target = kw_link_target(link).unwrap_or_else(|e| {
                panic!("link {link} should be recognized: {e}");
            });
            let expected_kind = case["kind"].as_str().unwrap();
            let expected_id = case["id"].as_str().unwrap();
            match (&target, expected_kind) {
                (KwLink::Song { rid }, "song") => assert_eq!(
                    rid, expected_id,
                    "link {link} should target song {expected_id}"
                ),
                (KwLink::Playlist { id }, "playlist") => assert_eq!(
                    id, expected_id,
                    "link {link} should target playlist {expected_id}"
                ),
                _ => panic!("link {link} should target {expected_kind}"),
            }
        }
        for link in fixture["rejected"].as_array().unwrap() {
            let link = link.as_str().unwrap();
            assert!(
                matches!(kw_link_target(link), Err(Error::InvalidInput(_))),
                "link {link} should be rejected"
            );
        }
    }

    #[test]
    fn songinfoandlrc_url_matches_the_upstream_route() {
        let fixture = fixture!("songinfoandlrc.json");
        assert_eq!(
            song_info_url("82068"),
            fixture["url_golden"].as_str().unwrap()
        );
    }

    #[test]
    fn songinfoandlrc_fixture_maps_song_metadata_only() {
        let fixture = fixture!("songinfoandlrc.json");
        let track = parse_song_info(&fixture["success"], "82068").expect("songName present");
        assert_eq!(track.id.provider, ID);
        assert_eq!(track.id.id, "82068");
        assert_eq!(track.title, "冰山上的雪莲");
        // The live body's artist is an empty string: the artist list stays
        // empty (metadata-only endpoint, paid songs included).
        assert!(track.artists.is_empty());
        assert_eq!(track.album.as_deref(), Some("我们的生活 充满阳光"));
        assert_eq!(
            track.artwork_url.as_deref(),
            Some("http://img1.kwcdn.kuwo.cn/star/albumcover/240/s4s26/64/940273715.jpg")
        );
        // Live `duration` is the string "0": no duration slot survives the
        // (duration > 0) convention.
        assert_eq!(track.duration, None);
        assert_eq!(track.extra.get("rid").map(String::as_str), Some("82068"));
        // pay/isdownload/coopFormats/formats/mp3Nsig exist in the body but
        // are read by nobody upstream: the availability signal is lost and
        // the qualities list stays empty (inherited gap).
        assert!(track.available_qualities.is_empty());
        assert!(track.album_id.is_none());
        // lrclist is carried in the same response but ignored (lyrics have
        // their own endpoint path, kw/lyric.rs).
        assert!(fixture["success"]["data"]["lrclist"].as_array().is_some());
        // The live failure envelope (status 301, data null) serde-parses
        // fine and hits the empty-songName gate: NotFound.
        assert!(parse_song_info(&fixture["not_found"], "228908").is_none());
        // The number-shaped variant pins the numeric fallback branch of the
        // duration read and the '&'-split of the artist.
        let track = parse_song_info(&fixture["numeric_duration"], "82068")
            .expect("variant songName present");
        assert_eq!(track.duration, Some(Duration::from_secs(234)));
        assert_eq!(track.artists, vec!["雷振邦".to_owned(), "雷佳".to_owned()]);
    }

    #[test]
    fn playlist_link_detail_pairs_meta_with_page0_songs() {
        let fixture = fixture!("playlist_detail.json");
        let (playlist, songs) =
            parse_playlist_with_songs(&fixture["success"], "1082685103").expect("musiclist there");
        // Link resolution carries the page-0 songs alongside the metadata
        // (upstream get_detail_with_meta returns both in one call).
        assert_eq!(songs.len(), 4);
        assert_eq!(songs[0].id.id, "228908");
        assert_eq!(playlist.id.id, "1082685103");
        assert_eq!(playlist.title, "经典华语合集");
        // track_count stays the full total even where a node caps the rows
        // (total=6 vs the 4 parseable page-0 rows here): the inherited
        // upstream single-page behavior.
        assert_eq!(playlist.track_count, Some(6));
        assert!(playlist.tracks.is_empty());
        // A body without musiclist is NotFound for the whole link
        // (upstream get_detail_with_meta gate).
        assert!(parse_playlist_with_songs(&fixture["no_musiclist"], "77").is_err());
    }

    // --- chart fixtures: the qukudata board tree and the kbangserver board
    // songs shape (kw/leaderboard.rs get_boards/get_list plus the
    // 2026-11-18 audit vectors), see tests/fixtures/kuwo/README.md.

    #[test]
    fn chart_urls_match_the_golden_vectors() {
        let fixture = fixture!("chart_catalogue.json");
        assert_eq!(
            chart_catalogue_url(),
            fixture["url_golden"].as_str().unwrap()
        );
        // The audit's golden board URL probes the format string with
        // pn=0/rn=3: `pn` is a 0-based server offset, so page 1 asks pn=0.
        let fixture = fixture!("chart_songs.json");
        assert_eq!(
            chart_songs_url("93", 0, 3),
            fixture["url_golden_board93_rn3"].as_str().unwrap()
        );
    }
    #[test]
    fn chart_catalogue_fixture_keeps_source_1_boards_on_sourceid() {
        let fixture = fixture!("chart_catalogue.json");
        let boards = parse_chart_catalogue(&fixture["catalogue"]).expect("catalogue parses");
        // The source=="27" external-link board (its sourceid is a
        // tencentmusic.com URL), the blank-name row and the row without
        // sourceid are dropped; 2 of 5 survive.
        assert_eq!(boards.len(), 2);
        // The board request id is `sourceid`, not the tree `id` (the tree
        // row id 534804 is a node number, the board id is 340).
        assert_eq!(boards[0].id.id, "340");
        assert_eq!(boards[0].title, "青少年专属飙升榜");
        assert_eq!(boards[0].description.as_deref(), Some("更新于2024-08-15"));
        // A numeric sourceid normalizes to the same string shape.
        assert_eq!(boards[1].id.id, "93");
        assert_eq!(boards[1].title, "酷我热榜");
        // An empty info maps to no description.
        assert_eq!(boards[1].description, None);
        // A body without `child` and a catalogue with no Kuwo board left
        // are the upstream get_boards failure branches.
        assert!(parse_chart_catalogue(&fixture["no_child"]).is_err());
        assert!(parse_chart_catalogue(&fixture["all_external"]).is_err());
    }
    #[test]
    fn chart_songs_fixture_ranks_parsed_rows_and_drops_bad_ones() {
        let fixture = fixture!("chart_songs.json");
        let entries = parse_chart_entries(&fixture["board"]).expect("board parses");
        // The empty-id row is dropped before ranking, so ranks stay
        // contiguous 1..N in list order.
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries.iter().map(|entry| entry.rank).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        // Audit row-0 vector: `song_duration` "189" wins over the bogus
        // `duration` "3" (live rows carry a wrong `duration`), and the
        // unmapped format tokens are ignored.
        let first = &entries[0].track;
        assert_eq!(first.id.id, "668801544");
        assert_eq!(first.title, "慌啥子嘛 (赵本山版)");
        assert_eq!(first.artists, vec!["赵本山".to_owned()]);
        assert_eq!(first.album.as_deref(), Some("慌啥子嘛（赵本山版）"));
        assert_eq!(first.album_id.as_deref(), Some("107468365"));
        assert_eq!(first.duration, Some(Duration::from_secs(189)));
        assert_eq!(
            first.available_qualities,
            vec![
                StreamQuality::High,
                StreamQuality::Low,
                StreamQuality::Lossless
            ]
        );
        assert_eq!(
            first.artwork_url.as_deref(),
            Some("http://img4.kuwo.cn/star/albumcover/row0.jpg")
        );
        // Numeric id, `&` artist split, the duration fallback and the
        // empty-formats Low128 floor.
        let second = &entries[1].track;
        assert_eq!(second.id.id, "669076081");
        assert_eq!(second.artists, vec!["张三".to_owned(), "李四".to_owned()]);
        assert_eq!(second.duration, Some(Duration::from_secs(200)));
        assert_eq!(second.available_qualities, vec![StreamQuality::Low]);
        // A present-but-empty `musiclist` is an empty page (upstream keeps
        // that a real board with zero rows); a missing `musiclist` — the
        // live bad-board-id body `{}` — is NotFound, a deliberate
        // reclassification of the upstream parse error.
        assert!(
            parse_chart_entries(&fixture["empty_musiclist"])
                .expect("empty board is a valid page")
                .is_empty()
        );
        assert!(parse_chart_entries(&fixture["no_musiclist"]).is_err());
    }
    #[test]
    fn detail_page_counts_fall_out_of_the_first_response_size() {
        // A node capping rn=1000 at ~10 rows produces a 20-page ladder.
        assert_eq!(detail_page_count(194, 10), 20);
        assert_eq!(detail_page_count(201, 10), 21);
        // rn=1000 honored: one page.
        assert_eq!(detail_page_count(194, 1000), 1);
        // The 100-page cap truncates ~100k-song playlists.
        assert_eq!(detail_page_count(100_000, 10), 100);
        assert_eq!(detail_page_count(0, 10), 0);
        // Guard: no continuation pages when the first page is empty.
        assert_eq!(detail_page_count(194, 0), 0);
    }
    #[test]
    fn formats_tokens_map_to_quality_tiers() {
        assert_eq!(
            parse_formats_qualities("MP3128|WMA128"),
            vec![StreamQuality::Low]
        );
        assert_eq!(parse_formats_qualities("MP3H"), vec![StreamQuality::High]);
        assert_eq!(
            parse_formats_qualities("ALFLAC"),
            vec![StreamQuality::Lossless]
        );
        assert_eq!(
            parse_formats_qualities("HIRFLAC|HIRES"),
            vec![StreamQuality::HiRes]
        );
        assert_eq!(
            parse_formats_qualities("MP3128|MP3H|ALFLAC|HIRFLAC"),
            vec![
                StreamQuality::Low,
                StreamQuality::High,
                StreamQuality::Lossless,
                StreamQuality::HiRes
            ]
        );
        // Unknown tokens are ignored; an all-empty set floors at Low128.
        assert_eq!(
            parse_formats_qualities("MysteryFormat"),
            vec![StreamQuality::Low]
        );
        assert_eq!(parse_formats_qualities(""), vec![StreamQuality::Low]);
    }
    // --- album fixtures: the search.kuwo.cn/r.s stype=albuminfo shape,
    // reconstructed from the 2026-11-18 live audit (albumid=1293 capture,
    // see tests/fixtures/kuwo/README.md). The track rows are the recorded
    // row-0 vector plus reconstructed placeholder rows pinning the variants.

    #[test]
    fn album_info_url_matches_the_upstream_route() {
        // Endpoint golden (audit vector 1): one anonymous GET, pn/rn pinned
        // at 0/1000, no signature/token; the Referer upstream sends is
        // live-checked optional and not part of the URL.
        assert_eq!(
            album_info_url("104004"),
            fixture!("album_info.json")["url_golden"].as_str().unwrap()
        );
    }
    #[test]
    fn album_info_fixture_literal_parses_with_intact_n_minfo_values() {
        let fixture = fixture!("album_info.json");
        // Defect 1 pin: the N_MINFO value carries bare `foo:bar` pairs; the
        // normalizer must not quote keys inside the single-quoted string (a
        // regex pass over `{,` boundaries corrupts it into invalid JSON).
        let json = parse_kuwo_json(fixture["literal"].as_str().unwrap()).expect("literal parses");
        let n_minfo = json["musiclist"][0]["N_MINFO"].as_str().unwrap();
        assert!(n_minfo.starts_with("level:dtsx,bitrate:25000,format:mmp4"));
        assert!(n_minfo.contains("level:ff,bitrate:2000,format:flac,size:32.38Mb"));
        assert!(n_minfo.ends_with("level:zp,bitrate:20000,format:zp,size:zpMb"));
        assert_eq!(json["songnum"].as_str(), Some("11"));
        // The row's album id is the camelCase `albumId` and arrives as a
        // JSON number (defect 3: no lower-case `albumid` exists on the row).
        assert_eq!(json["musiclist"][0]["albumId"].as_u64(), Some(1293));
        assert!(json["musiclist"][0].get("albumid").is_none());
        assert_eq!(
            json["hts_img"].as_str(),
            Some("https://img2.kuwo.cn/star/albumcover/240/s3s94/93/211513640.jpg")
        );
        // Live albuminfo rows carry a bare musicrid (defect 2: no MUSIC_).
        assert_eq!(json["musiclist"][0]["musicrid"].as_str(), Some("238210"));
        // Valid JSON passes through untouched.
        assert!(parse_kuwo_json(r#"{"a":1}"#).is_ok());
    }
    #[test]
    fn album_fixture_rows_parse_bare_musicrid_album_id_and_qualities() {
        let fixture = fixture!("album_info.json");
        let json = parse_kuwo_json(fixture["literal"].as_str().unwrap()).unwrap();
        let items = json["musiclist"].as_array().unwrap();
        let tracks: Vec<Track> = items
            .iter()
            .filter_map(|item| parse_album_row(item, "1293"))
            .collect();
        // The empty-rid row is dropped: 4 rows in, 3 tracks out.
        assert_eq!(tracks.len(), 3);
        let first = &tracks[0];
        assert_eq!(first.id.id, "238210");
        assert_eq!(first.title, "以父之名");
        // `artist` wins over `aartist`; the `Jay&nbsp;Chou` entity is not decoded.
        assert_eq!(first.artists, vec!["周杰伦".to_owned()]);
        assert_eq!(first.album.as_deref(), Some("叶惠美"));
        assert_eq!(first.album_id.as_deref(), Some("1293"));
        assert_eq!(first.duration, Some(Duration::from_secs(342)));
        assert_eq!(
            first.artwork_url.as_deref(),
            Some("http://img4.kuwo.cn/star/albumcover/120/s3s94/93/211513640.jpg")
        );
        // N_MINFO maps only 2000/320/128 in encounter order; the DRM
        // (20000/20201/...) and unmatched tiers stay unmapped, no HiRes.
        assert_eq!(
            first.available_qualities,
            vec![
                StreamQuality::Lossless,
                StreamQuality::High,
                StreamQuality::Low
            ]
        );
        // No N_MINFO: the pipe-separated `formats` tokens take over.
        let second = &tracks[1];
        assert_eq!(second.id.id, "238217");
        assert_eq!(second.title, "晴天");
        assert_eq!(second.album_id.as_deref(), Some("1293"));
        assert_eq!(second.duration, Some(Duration::from_secs(269)));
        assert_eq!(
            second.available_qualities,
            vec![
                StreamQuality::Low,
                StreamQuality::High,
                StreamQuality::Lossless
            ]
        );
        // MUSIC_-prefixed musicrid strips to the id; a missing albumId falls
        // back to the requested album id; a numeric duration normalizes.
        let third = &tracks[2];
        assert_eq!(third.id.id, "6550104");
        assert_eq!(third.album_id.as_deref(), Some("1293"));
        assert_eq!(third.duration, Some(Duration::from_secs(232)));
        assert_eq!(third.available_qualities, vec![StreamQuality::Low]);
    }
    #[test]
    fn album_fixture_meta_maps_the_top_level_fields() {
        let fixture = fixture!("album_info.json");
        let json = parse_kuwo_json(fixture["literal"].as_str().unwrap()).unwrap();
        let album = parse_album_meta(&json, "1293");
        assert_eq!(album.id.id, "1293");
        assert_eq!(album.title, "叶惠美");
        assert_eq!(album.artists, vec!["周杰伦".to_owned()]);
        // `hts_img` is the only observed full-URL cover; `pic` is relative.
        assert_eq!(
            album.artwork_url.as_deref(),
            Some("https://img2.kuwo.cn/star/albumcover/240/s3s94/93/211513640.jpg")
        );
        assert_eq!(album.track_count, Some(11));
    }
    #[test]
    fn empty_album_fixture_is_an_explicit_empty_result() {
        // Live albumid=104004 (audit vector 4): HTTP 200, songnum '0',
        // musiclist [] with normal top-level meta. Upstream conflates this
        // with a parse failure; the port must not copy that semantics.
        let fixture = fixture!("album_info.json");
        let json = parse_kuwo_json(fixture["empty"].as_str().unwrap()).unwrap();
        assert!(ensure_album_shape(&json).is_ok());
        let items = json["musiclist"].as_array().unwrap();
        let tracks: Vec<Track> = items
            .iter()
            .filter_map(|item| parse_album_row(item, "104004"))
            .collect();
        assert!(tracks.is_empty());
        let album = parse_album_meta(&json, "104004");
        assert_eq!(album.title, "军旅歌曲");
        assert_eq!(album.track_count, Some(0));
    }
    #[test]
    fn album_body_without_album_fields_fails_the_shape_gate() {
        assert!(parse_kuwo_json("{foo:'1'}").is_ok_and(|json| ensure_album_shape(&json).is_err()));
    }
    #[tokio::test]
    async fn user_playlists_stay_unsupported_and_inputs_are_validated() {
        let provider = KuwoProvider::new();
        // No upstream user-playlist implementation exists to migrate.
        assert!(matches!(
            provider.user_playlists(1, 10).await,
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            provider.search_playlists("", 1).await,
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            provider.playlist("").await,
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            provider.playlist_tracks("").await,
            Err(Error::InvalidInput(_))
        ));
    }
    /// The §8 slice 4 wiring: `login_status` derives from
    /// `KuwoProvider::auth_state` — the anonymous `web_token` is the whole
    /// session shape — and the legacy three-state shape is its documented
    /// fold. The anonymous web identity is `LoggedIn { user_id: None, .. }`,
    /// not an account claim; [`Self::mark_session_expired`] discards the
    /// token (state returns to `LoggedOut` until the next web call mints a
    /// fresh one) and returns the classified [`Error::AuthenticationRequired`]
    /// (Display body "login required", the pre-migration string; not
    /// retryable — recovery is a fresh token, not a resend).
    #[tokio::test]
    async fn auth_state_derivation_drives_login_status() {
        let provider = KuwoProvider::new();
        // No session at all: LoggedOut, in both views.
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        assert_eq!(
            provider.validate_session().await.unwrap(),
            AuthState::LoggedOut
        );

        // A session without the token (cookies only) is still LoggedOut:
        // token presence is the predicate, not session presence.
        let store = provider.session_store();
        let mut session = ProviderSession::default();
        session.cookies.insert("Hm_Iuvt_other".into(), "v".into());
        store.set(ID, session).unwrap();
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );

        // Token minted (what session_token does before every web-signed
        // call): Authenticated, folded to the anonymous LoggedIn shape —
        // the anonymous web identity, never an account claim.
        provider.session_token();
        assert_eq!(provider.auth_state(), AuthState::Authenticated);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedIn {
                user_id: None,
                display_name: None,
            }
        );
        assert_eq!(
            provider.validate_session().await.unwrap(),
            AuthState::Authenticated
        );

        // The classified rejection: AuthenticationRequired carrying the
        // provider id and the label detail; the token is discarded, so the
        // state returns to LoggedOut until the next call mints a fresh one.
        let error = provider.mark_session_expired(Some(ErrorDetail::new("web play url")));
        assert_eq!(
            error.category(),
            crate::ErrorCategory::AuthenticationRequired
        );
        assert_eq!(error.provider_id(), Some("kuwo"));
        assert_eq!(error.detail(), Some("web play url"));
        assert!(
            !error.is_retryable(),
            "recovery is a fresh token on the next call, not a resend"
        );
        assert_eq!(error.to_string(), "login required: web play url");
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        // Without a detail the Display is exactly the pre-migration body.
        let bare = provider.mark_session_expired(None);
        assert_eq!(bare.to_string(), "login required");

        // logout clears the whole session; the state machine's
        // Authenticated -> LoggedOut edge stays reachable.
        provider.session_token();
        assert_eq!(provider.auth_state(), AuthState::Authenticated);
        provider.logout().await.unwrap();
        assert_eq!(provider.auth_state(), AuthState::LoggedOut);
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
    }
    /// The descriptor facet (AUTH_REGISTRY_DESIGN.md §5.4): the single
    /// `authentication` bool stays false — an anonymous web token is not an
    /// account login (the registration test pins the bool) — while the auth
    /// facet names the mechanism that does exist
    /// (`AuthCapability::Anonymous`), in both the provider view and the
    /// registry descriptor.
    #[test]
    fn auth_capability_declares_the_anonymous_web_token() {
        let provider = KuwoProvider::new();
        assert!(!provider.capabilities().authentication);
        assert_eq!(provider.auth_capability(), AuthCapability::Anonymous);
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert_eq!(descriptor.auth, AuthCapability::Anonymous);
        assert!(!descriptor.capabilities.authentication);
    }
    /// 兼容策略 verification (AUTH_REGISTRY_DESIGN.md §9.3): every
    /// `Error::InvalidInput` construction site keeps its category and its
    /// exact message. The legacy variant is the caller-error carrier (the
    /// M2 taxonomy has no caller-input variant, §1.1 keeps it unchanged), so
    /// the per-site mapping is the category check — all ten sites are
    /// genuinely caller-input errors — and the messages must not drift.
    #[tokio::test]
    async fn invalid_input_sites_keep_their_category_and_message() {
        let provider = KuwoProvider::new();
        let mut foreign = sample_track();
        foreign.id = TrackId::new("netease", "123");
        let checks: Vec<(&str, Error)> = vec![
            (
                "kw_link_target",
                kw_link_target("https://example.com/nope").unwrap_err(),
            ),
            (
                "search",
                provider.search(&SearchQuery::new("")).await.unwrap_err(),
            ),
            (
                "search_playlists",
                provider.search_playlists("", 1).await.unwrap_err(),
            ),
            ("playlist", provider.playlist("").await.unwrap_err()),
            (
                "playlist_tracks",
                provider.playlist_tracks("").await.unwrap_err(),
            ),
            ("album", provider.album("").await.unwrap_err()),
            ("album_tracks", provider.album_tracks("").await.unwrap_err()),
            ("chart_tracks", provider.chart_tracks("").await.unwrap_err()),
            (
                "resolve",
                provider
                    .resolve(&foreign, &ResolveOptions::default())
                    .await
                    .unwrap_err(),
            ),
            ("lyrics", provider.lyrics(&foreign).await.unwrap_err()),
        ];
        let expected = [
            "unrecognized Kuwo song/playlist link",
            "search keyword is empty",
            "playlist search keyword is empty",
            "playlist id is empty",
            "playlist id is empty",
            "album id is empty",
            "album id is empty",
            "chart id is empty",
            "track does not belong to Kuwo",
            "track does not belong to Kuwo",
        ];
        for ((label, error), message) in checks.into_iter().zip(expected) {
            assert_eq!(
                error.category(),
                crate::ErrorCategory::InvalidInput,
                "{label}"
            );
            assert_eq!(
                error.to_string(),
                format!("invalid input: {message}"),
                "{label}"
            );
        }
    }
    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = KuwoProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.lyrics);
        // The public playlist module is implemented (wapi/pl.svc/r.s).
        assert!(descriptor.capabilities.playlists);
        // The album module is implemented (r.s stype=albuminfo).
        assert!(descriptor.capabilities.albums);
        // The chart module is implemented (qukudata board tree +
        // kbangserver board songs) and registered.
        assert!(descriptor.capabilities.charts);
        // Link parsing is implemented (kw/parse.rs: the two share-link
        // shapes, song metadata + playlist detail) and registered.
        assert!(descriptor.capabilities.link_resolve);
        assert!(registry.collection_provider(ID).is_some());
        assert!(registry.playlist_provider(ID).is_some());
        assert!(registry.album_provider(ID).is_some());
        assert!(registry.chart_provider(ID).is_some());
        assert!(registry.link_provider(ID).is_some());
        // Web session signing is not implemented: no login capability.
        assert!(!descriptor.capabilities.authentication);
        assert!(registry.lyrics_provider(ID).is_some());
    }

    // --- unified transport migration tests (mock transport, no network):
    // the provider runs against Http::from_shared_mock and the recorded
    // request log pins the migrated request faces (TRANSPORT_DESIGN.md §7.4).

    fn mock_json(status: u16, body: &str) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: body.as_bytes().to_vec(),
        }
    }

    fn mock_bytes(status: u16, body: &[u8]) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: body.to_vec(),
        }
    }

    fn sample_track() -> Track {
        Track {
            id: TrackId::new(ID, "228908"),
            title: "晴天".into(),
            artists: vec!["周杰伦".into()],
            album: None,
            album_id: None,
            duration: Some(Duration::from_secs(269)),
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: BTreeMap::new(),
        }
    }

    fn mock_provider(mock: &Arc<MockTransport>) -> KuwoProvider {
        KuwoProvider::with_transport(Http::from_shared_mock(mock.clone()))
    }

    // --- resolve race tests (voicefox kw/url.rs:239-256 parity, §8 kuwo
    // candidate parity): the web-signed play URL and the mobile tier ladder
    // run concurrently; the loser rides as a structured fallback endpoint
    // and the web URL carries the resolve when the whole ladder exhausts.
    // futures::join! polls the web future first, so the scripted response
    // order is web, then the mobile tiers.

    #[tokio::test]
    async fn resolve_lossless_web_primary_carries_the_mobile_candidate() {
        let mock = Arc::new(MockTransport::script([
            Ok(mock_json(
                200,
                r#"{"code":200,"data":{"url":"http://kw-web.kuwo.cn/y.flac"}}"#,
            )),
            Ok(mock_json(
                200,
                r#"{"code":200,"data":{"rid":"228908","duration":232,"bitrate":320,"url":"http://kw-bj.kuwo.cn/x.mp3"}}"#,
            )),
        ]));
        let provider = mock_provider(&mock);
        let options = ResolveOptions {
            quality: StreamQuality::Lossless,
        };
        let info = provider
            .resolve(&sample_track(), &options)
            .await
            .expect("web + mobile both resolve");
        // The web URL wins the lossless primary; the mobile tier URL rides as
        // the structured candidate with the tier it serves and the okhttp UA
        // its CDN requires. No legacy strings (one slot per producer, §2.4).
        assert_eq!(info.url, "http://kw-web.kuwo.cn/y.flac");
        assert_eq!(info.quality, StreamQuality::Lossless);
        assert_eq!(info.format, AudioFormat::Flac);
        assert!(info.fallback_urls.is_empty());
        assert_eq!(info.fallback_endpoints.len(), 1);
        assert_eq!(info.fallback_endpoints[0].url, "http://kw-bj.kuwo.cn/x.mp3");
        assert_eq!(
            info.fallback_endpoints[0].quality,
            Some(StreamQuality::High)
        );
        assert_eq!(
            info.fallback_endpoints[0].headers,
            vec![("User-Agent".to_owned(), UA_MEDIA.to_owned())]
        );
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2, "one web + one mobile send");
        assert!(
            recorded[0].url.starts_with(WEB_PLAY_URL) && recorded[0].url.contains("reqId="),
            "the web face rides the signed web endpoint: {}",
            recorded[0].url
        );
        assert!(
            recorded[1].url.contains("br=320kmp3"),
            "the mobile face requests the 320k tier: {}",
            recorded[1].url
        );
    }

    #[tokio::test]
    async fn resolve_mobile_primary_carries_the_web_candidate() {
        let mock = Arc::new(MockTransport::script([
            Ok(mock_json(
                200,
                r#"{"code":200,"data":{"url":"http://kw-web.kuwo.cn/y.flac"}}"#,
            )),
            Ok(mock_json(
                200,
                r#"{"code":200,"data":{"rid":"228908","duration":232,"bitrate":320,"url":"http://kw-bj.kuwo.cn/x.mp3"}}"#,
            )),
        ]));
        let provider = mock_provider(&mock);
        let info = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .expect("web + mobile both resolve");
        // The mobile ladder wins the non-lossless primary; the web signed URL
        // is the structured candidate with the undeclared-tier `None` and the
        // okhttp UA upstream applies to every kuwo URL.
        assert_eq!(info.url, "http://kw-bj.kuwo.cn/x.mp3");
        assert_eq!(info.quality, StreamQuality::High);
        assert_eq!(info.format, AudioFormat::Mp3);
        assert!(
            info.fallback_urls.is_empty(),
            "the web candidate rides the structured slot only"
        );
        assert_eq!(info.fallback_endpoints.len(), 1);
        assert_eq!(
            info.fallback_endpoints[0].url,
            "http://kw-web.kuwo.cn/y.flac"
        );
        assert_eq!(info.fallback_endpoints[0].quality, None);
        assert_eq!(
            info.fallback_endpoints[0].headers,
            vec![("User-Agent".to_owned(), UA_MEDIA.to_owned())]
        );
    }

    #[tokio::test]
    async fn resolve_web_fallback_serves_when_the_mobile_ladder_exhausts() {
        let placeholder = r#"{"code":200,"data":{"rid":260839262,"duration":11,"bitrate":1,"url":"http://kw-er.kuwo.cn/x.mp3"}}"#;
        let mock = Arc::new(MockTransport::script([
            Ok(mock_json(
                200,
                r#"{"code":200,"data":{"url":"http://kw-web.kuwo.cn/y.mp3"}}"#,
            )),
            Ok(mock_json(200, placeholder)),
            Ok(mock_json(200, placeholder)),
        ]));
        let provider = mock_provider(&mock);
        let info = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .expect("the web URL carries the resolve when every tier placeholder-fails");
        // New reachable path (voicefox parity): web primary, reported High/Mp3
        // because the web API declares no tier (kw/url.rs:244-247).
        assert_eq!(info.url, "http://kw-web.kuwo.cn/y.mp3");
        assert_eq!(info.quality, StreamQuality::High);
        assert_eq!(info.format, AudioFormat::Mp3);
        assert!(info.fallback_endpoints.is_empty());
        assert_eq!(mock.recorded().len(), 3, "web + both mobile tiers");
    }

    #[tokio::test]
    async fn resolve_reports_not_found_when_both_faces_fail() {
        let placeholder = r#"{"code":200,"data":{"rid":260839262,"duration":11,"bitrate":1,"url":"http://kw-er.kuwo.cn/x.mp3"}}"#;
        let mock = Arc::new(MockTransport::script([
            Ok(mock_json(200, r#"{"code":-1,"msg":"vip only"}"#)),
            Ok(mock_json(200, placeholder)),
            Ok(mock_json(200, placeholder)),
        ]));
        let provider = mock_provider(&mock);
        let error = provider
            .resolve(&sample_track(), &ResolveOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound), "got: {error}");
        assert_eq!(mock.recorded().len(), 3);
    }

    #[tokio::test]
    async fn resolve_lossless_mobile_only_keeps_the_primary_tier_report() {
        let mock = Arc::new(MockTransport::script([
            Ok(mock_json(200, r#"{"code":-1,"msg":"vip only"}"#)),
            Ok(mock_json(
                200,
                r#"{"code":200,"data":{"rid":"228908","duration":232,"bitrate":320,"url":"http://kw-bj.kuwo.cn/x.mp3"}}"#,
            )),
        ]));
        let provider = mock_provider(&mock);
        let options = ResolveOptions {
            quality: StreamQuality::Lossless,
        };
        let info = provider
            .resolve(&sample_track(), &options)
            .await
            .expect("mobile 320k serves when the web face refuses");
        // Pre-parity behavior preserved: without a web URL the mobile tier
        // wins and its own tier is reported; no candidate is fabricated.
        assert_eq!(info.url, "http://kw-bj.kuwo.cn/x.mp3");
        assert_eq!(info.quality, StreamQuality::High);
        assert_eq!(info.fallback_endpoints.len(), 0);
    }

    #[tokio::test]
    async fn get_json_runs_through_send_api_json_with_mobile_ua_and_cookies() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert_eq!(request.method, HttpMethod::Get);
            assert_eq!(request.url, "https://mobi.kuwo.cn/mobi.s?rid=6550104");
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "user-agent" && v == UA_MOBILE)
            );
            let cookie = request
                .headers
                .iter()
                .find(|(k, _)| k == "cookie")
                .map(|(_, v)| v.as_str())
                .expect("session cookies must ride the request");
            assert!(cookie.contains("uid=123"), "{cookie}");
            assert!(cookie.contains("tracker=x"), "{cookie}");
            Ok(mock_json(
                200,
                r#"{"code":200,"data":{"url":"http://kw.example/x.mp3"}}"#,
            ))
        }));
        let mut session = ProviderSession::default();
        session.cookies.insert("uid".into(), "123".into());
        session.cookies.insert("tracker".into(), "x".into());
        let store = SessionStore::default();
        store.set(ID, session).unwrap();
        let provider = mock_provider(&mock).with_session_store(store);
        let value = provider
            .get_json("https://mobi.kuwo.cn/mobi.s?rid=6550104", "stream resolve")
            .await
            .unwrap();
        assert_eq!(value["data"]["url"], "http://kw.example/x.mp3");
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn get_json_missing_code_body_passes_the_gate() {
        // Search / playlist bodies carry no `code` field: `kuwo_code_of`
        // falls back to `KUWO_CODES.ok` (review P-7), exactly like the
        // previous `unwrap_or(KUWO_CODES.ok)`; a `0` fallback would have
        // turned this body into `Fatal("API code 0")`.
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"TOTAL":12,"SHOW":10,"abslist":[]}"#,
        ))]));
        let provider = mock_provider(&mock);
        let value = provider
            .get_json("https://example.test/r.s", "search")
            .await
            .unwrap();
        assert_eq!(value["TOTAL"], 12);
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn get_json_fatal_api_code_keeps_label_and_api_text_without_retry() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":-1,"msg":"vip only"}"#,
        ))]));
        let provider = mock_provider(&mock);
        let error = provider
            .get_json("https://mobi.kuwo.cn/mobi.s", "stream resolve")
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider kuwo failed: stream resolve: API code -1: vip only"
        );
        assert_eq!(
            mock.recorded().len(),
            1,
            "api_retries: 0 keeps body codes terminal (§5.3)"
        );
    }

    #[tokio::test]
    async fn get_json_retries_transport_errors_and_503_under_the_unified_policy() {
        // One transport failure, then one 503, then success: the engine's
        // ladder (`transport_retries: 4`) replaces the fixed
        // `[600,1200,2400,4000]` table with the same 5-send budget
        // (TRANSPORT_DESIGN.md §5.5 row 1).
        let mock = Arc::new(MockTransport::script([
            Err(TransportError::Transport("conn reset".into())),
            Ok(mock_json(503, "")),
            Ok(mock_json(200, r#"{"code":200,"data":{}}"#)),
        ]));
        let provider = mock_provider(&mock);
        let value = provider
            .get_json("https://mobi.kuwo.cn/mobi.s", "stream resolve")
            .await
            .unwrap();
        assert_eq!(value["code"], 200);
        assert_eq!(mock.recorded().len(), 3);
    }

    #[tokio::test]
    async fn get_json_with_carries_the_playlist_referer_per_request() {
        let mock = Arc::new(MockTransport::new(|_request| {
            Ok(mock_json(200, r#"{"code":200,"data":{}}"#))
        }));
        let provider = mock_provider(&mock);
        let tagged = provider
            .get_json_with(
                "http://wapi.kuwo.cn/api/pc/classify/playlist/getTagList",
                "playlist categories",
                Some(PLAYLIST_REFERER),
            )
            .await
            .unwrap();
        assert_eq!(tagged["code"], 200);
        let hot = provider
            .get_json_with(
                "http://wapi.kuwo.cn/api/pc/classify/playlist/getRcmPlayList",
                "hot playlists",
                None,
            )
            .await
            .unwrap();
        assert_eq!(hot["code"], 200);
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2);
        assert!(
            recorded[0]
                .headers
                .iter()
                .any(|(k, v)| k == "referer" && v == PLAYLIST_REFERER),
            "the Referer rides as a per-request header"
        );
        assert!(
            !recorded[1].headers.iter().any(|(k, _)| k == "referer"),
            "the hot page sends no Referer"
        );
    }

    #[tokio::test]
    async fn fetch_raw_lyric_reads_the_buffered_body_through_raw_get() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert!(request.url.starts_with(LYRIC_URL), "{}", request.url);
            // The query is the yeelion-encrypted params; decrypt and check.
            let query = request.url.split('?').nth(1).unwrap_or_default();
            let decoded = STANDARD.decode(query).expect("query is base64");
            let recovered: Vec<u8> = decoded
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ LYRIC_XOR_KEY[i % LYRIC_XOR_KEY.len()])
                .collect();
            let params = String::from_utf8(recovered).unwrap();
            assert!(params.contains("rid=MUSIC_228908"), "{params}");
            Ok(mock_bytes(
                200,
                include_bytes!("../tests/fixtures/kuwo/lyric_framed.bin"),
            ))
        }));
        let provider = mock_provider(&mock);
        let text = provider.fetch_raw_lyric("228908", false).await.unwrap();
        assert_eq!(text, "[00:01.00]测试歌词\r\n[00:02.00]second line");
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn fetch_artist_pic_url_parses_the_plain_text_line() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert_eq!(request.url, artist_pic_url("6550104"));
            assert!(
                !request.headers.iter().any(|(k, _)| k == "referer"),
                "tier two sends no Referer"
            );
            Ok(mock_bytes(
                200,
                b"http://img4.kuwo.cn/star/albumcover/x.jpg\n",
            ))
        }));
        let provider = mock_provider(&mock);
        assert_eq!(
            provider.fetch_artist_pic_url("6550104").await.as_deref(),
            Some("http://img4.kuwo.cn/star/albumcover/x.jpg")
        );
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn search_playlists_parses_the_js_literal_through_raw_get() {
        let mock = Arc::new(MockTransport::script([Ok(mock_bytes(
            200,
            r#"{'total':'10',abslist:[{'playlistid':'123','name':'华语','songnum':'5'}]};"#
                .as_bytes(),
        ))]));
        let provider = mock_provider(&mock);
        let playlists = provider.search_playlists("华语", 1).await.unwrap();
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].id.id, "123");
        assert_eq!(playlists[0].title, "华语");
        assert_eq!(playlists[0].track_count, Some(5));
    }

    #[tokio::test]
    async fn fetch_web_url_signs_secret_and_reqid_and_stays_single_shot() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert_eq!(request.method, HttpMethod::Get);
            assert!(
                request.url.starts_with(WEB_PLAY_URL) && request.url.contains("reqId="),
                "{}",
                request.url
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "user-agent" && v == WEB_UA)
            );
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "referer" && v == WEB_REFERER)
            );
            let cookie = request
                .headers
                .iter()
                .find(|(k, _)| k == "cookie")
                .map(|(_, v)| v.as_str())
                .expect("session cookie must ride the request");
            let token = cookie
                .strip_prefix(&format!("{SESSION_COOKIE}="))
                .expect("Hm_Iuvt_ cookie shape");
            let secret = request
                .headers
                .iter()
                .find(|(k, _)| k == "Secret")
                .map(|(_, v)| v.as_str())
                .expect("Secret header");
            // Secret = hex(cookie xor LCG mask) + 8 nonce digits
            // (build_secret); the trailing nonce must re-derive it.
            assert_eq!(secret.len(), token.len() * 2 + 8);
            assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
            let nonce = u32::from_str_radix(&secret[secret.len() - 8..], 16).unwrap();
            assert_eq!(build_secret(token, nonce), secret);
            Ok(mock_json(
                200,
                r#"{"code":200,"data":{"url":"http://gec-kw-doc.kuwo.cn/example/example.flac"}}"#,
            ))
        }));
        let provider = mock_provider(&mock);
        let url = provider.fetch_web_url("6550104").await.unwrap();
        assert_eq!(url, "http://gec-kw-doc.kuwo.cn/example/example.flac");
        assert_eq!(
            mock.recorded().len(),
            1,
            "web_get stays single-shot (send_once, no ladder)"
        );
    }

    #[tokio::test]
    async fn web_get_invalidates_the_web_token_on_illegal_session_without_retry() {
        let mut session = ProviderSession::default();
        session
            .tokens
            .insert("web_token".into(), "storedtoken".into());
        let store = SessionStore::default();
        store.set(ID, session).unwrap();
        let mock = Arc::new(MockTransport::new(|request| {
            // The pre-seeded token rides the signed request.
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "cookie" && v == &format!("{SESSION_COOKIE}=storedtoken"))
            );
            // HTTP 200 with the illegal-session text marker: only the body
            // shape reveals the rejection.
            Ok(TransportResponse {
                status: 200,
                headers: Vec::new(),
                url: None,
                body: br#"{"success":false,"message":"request is illegal"}"#.to_vec(),
            })
        }));
        let provider = mock_provider(&mock).with_session_store(store.clone());
        let error = provider.fetch_web_url("6550104").await.unwrap_err();
        assert!(
            error.to_string().contains("session rejected"),
            "got: {error}"
        );
        assert_eq!(
            mock.recorded().len(),
            1,
            "no retry: the caller retries with a fresh token"
        );
        // The stored token was invalidated; the next call generates a fresh
        // one (session_token).
        let session = store.get(ID).unwrap();
        assert!(!session.tokens.contains_key("web_token"));
    }

    #[tokio::test]
    async fn resolve_serves_the_mobile_tier_through_the_unified_layer() {
        // Race shape (voicefox kw/url.rs:239-256 parity): the web face runs
        // alongside the mobile ladder even for Low requests and rides as the
        // structured candidate; the mobile tier pins are kept verbatim.
        let mock = Arc::new(MockTransport::new(|request| {
            if request.url.starts_with(WEB_PLAY_URL) {
                assert!(request.url.contains("reqId="), "{}", request.url);
                return Ok(mock_json(
                    200,
                    r#"{"code":200,"data":{"url":"http://kw-web.kuwo.cn/y.mp3"}}"#,
                ));
            }
            assert!(
                request.url.contains("mobi.kuwo.cn/mobi.s") && request.url.contains("br=128kmp3"),
                "{}",
                request.url
            );
            Ok(mock_json(
                200,
                r#"{"code":200,"data":{"rid":228908,"duration":269,"url":"http://kw-er.kuwo.cn/x.mp3"}}"#,
            ))
        }));
        let provider = mock_provider(&mock);
        let track = sample_track();
        let info = provider
            .resolve(
                &track,
                &ResolveOptions {
                    quality: StreamQuality::Low,
                },
            )
            .await
            .unwrap();
        assert_eq!(info.url, "http://kw-er.kuwo.cn/x.mp3");
        assert_eq!(info.quality, StreamQuality::Low);
        assert_eq!(info.format, AudioFormat::Mp3);
        // The media CDN requires the okhttp UA or it 403s.
        assert_eq!(
            info.headers,
            vec![("User-Agent".to_owned(), UA_MEDIA.to_owned())]
        );
        // The web signed URL rides as the structured candidate (undeclared
        // tier, okhttp UA); no legacy strings are written.
        assert!(info.fallback_urls.is_empty());
        assert_eq!(info.fallback_endpoints.len(), 1);
        assert_eq!(
            info.fallback_endpoints[0].url,
            "http://kw-web.kuwo.cn/y.mp3"
        );
        assert_eq!(info.fallback_endpoints[0].quality, None);
        assert_eq!(
            info.fallback_endpoints[0].headers,
            vec![("User-Agent".to_owned(), UA_MEDIA.to_owned())]
        );
        assert_eq!(mock.recorded().len(), 2, "web + mobile-128 send");
    }
}
