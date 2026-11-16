//! Kuwo Music provider migrated from VoiceFox's `lx-source::kw`.
//! Protocol origin: lx-music `musicSdk/kw/`. Anonymous search, mobile stream
//! URLs and XOR/zlib lyric decryption are implemented. The web play URL and
//! cover endpoints need the web session signing flow, which is not implemented
//! yet; lossless tiers are unavailable on the anonymous mobile endpoint and
//! downgrade to 320k/128k. Paid songs return an 11-second placeholder audio
//! with a mismatched rid: that response is rejected, never handed out.
use crate::{
    Error, Result,
    lrc::parse_lrc,
    models::{AudioFormat, LyricDocument, LyricFormat, StreamInfo, StreamQuality, Track, TrackId},
    provider::{
        AuthenticationProvider, LoginStatus, LyricsProvider, MusicProvider, ResolveOptions,
        SearchPage, SearchQuery, StreamResolver, TrackSearch,
    },
    retry::{KUWO_CODES, StepOutcome, classify_api, classify_http},
    session::SessionStore,
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::read::ZlibDecoder;
use md5::Digest;
use reqwest::{Client, RequestBuilder, header};
use serde_json::Value;
use std::{collections::BTreeMap, io::Read, time::Duration};

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

#[derive(Clone)]
pub struct KuwoProvider {
    client: Client,
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
    /// Register all currently implemented Kuwo capabilities in one step.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_lyrics(provider);
    }
    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        let mut request = request.header(header::USER_AGENT, UA_MOBILE);
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
                    tracing::debug!("Kuwo {label} transport retry {}: {error}", attempt + 1);
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
        // Kuwo responses carry `code` on the play endpoints but not on search.
        let budget = 0;
        match classify_api(
            value["code"].as_i64().unwrap_or(KUWO_CODES.ok),
            value["msg"].as_str().unwrap_or("unknown error"),
            budget,
            budget,
            &KUWO_CODES,
        ) {
            StepOutcome::Ok => Ok(value),
            StepOutcome::Retry => Err(perr(format!("{label}: throttled"))),
            StepOutcome::SessionExpired => Err(perr(format!("{label}: login required"))),
            StepOutcome::Fatal(m) => Err(perr(format!("{label}: {m}"))),
        }
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
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
/// Decode the lyric body: skip the `tp=content` header, zlib-inflate, then
/// GB18030-decode (the lyric payload is not UTF-8).
fn decode_lyric_body(raw: &[u8]) -> Result<String> {
    let compressed = match raw.windows(4).position(|window| window == b"\r\n\r\n") {
        Some(position) => &raw[position + 4..],
        None => raw,
    };
    let mut decoder = ZlibDecoder::new(compressed);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| perr(format!("lyric zlib decompress failed: {e}")))?;
    let (decoded, _, _) = encoding_rs::GB18030.decode(&decompressed);
    Ok(decoded.into_owned())
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
            extra: BTreeMap::new(),
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
        let data = &value["data"];
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
    async fn fetch_raw_lyric(&self, song_id: &str) -> Result<String> {
        let params =
            format!("user=12345,web,web,web&requester=localhost&req=1&rid=MUSIC_{song_id}");
        let url = format!("{LYRIC_URL}?{}", encrypt_lyric_params(&params));
        let response = self.raw_get(&url, "lyric").await?;
        let raw = response
            .bytes()
            .await
            .map_err(|e| perr(format!("lyric body read failed: {e}")))?;
        if raw.is_empty() {
            return Err(Error::NotFound);
        }
        decode_lyric_body(&raw)
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
    fn sign_web(&self, request: RequestBuilder, cookie: &str, nonce: u32) -> RequestBuilder {
        request
            .header(header::USER_AGENT, WEB_UA)
            .header(header::REFERER, WEB_REFERER)
            .header("Secret", build_secret(cookie, nonce))
            .header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"))
    }
    /// Signed GET returning the raw body text (illegal-session detection
    /// needs the text shape).
    async fn web_get(&self, url: &str, label: &str) -> Result<String> {
        let cookie = self.session_token();
        let nonce = (random_byte(0) as u32) * 1_000_000 + (random_byte(1) as u32) * 10_000;
        let url = with_request_id(url, &request_id());
        let response = self
            .sign_web(self.client.get(&url), &cookie, nonce)
            .send()
            .await
            .map_err(|e| perr(format!("{label}: {e}")))?;
        let body = response
            .text()
            .await
            .map_err(|e| perr(format!("{label}: read failed: {e}")))?;
        if is_illegal_session(&body) {
            self.invalidate_web_token();
            return Err(perr(format!(
                "{label}: session rejected, retry with a fresh token"
            )));
        }
        Ok(body)
    }
    /// Web play URL (web-signed); lossless tiers work here where the mobile
    /// endpoint serves placeholders. Returns the URL only.
    async fn fetch_web_url(&self, song_id: &str) -> Result<String> {
        let url = format!("{WEB_PLAY_URL}?mid={song_id}&type=music&httpsStatus=1");
        let body = self.web_get(&url, "web play url").await?;
        let json: Value = serde_json::from_str(&body)
            .map_err(|e| perr(format!("web play url parse failed: {e}")))?;
        if json["code"].as_i64() != Some(200) {
            // Common: code=-1 paid content; other codes are rights/region blocks.
            return Err(Error::NotFound);
        }
        json["data"]["url"]
            .as_str()
            .map(str::trim)
            .filter(|url| is_http_url(url))
            .map(str::to_owned)
            .ok_or(Error::NotFound)
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
            ..Default::default()
        }
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
}
#[async_trait]
impl StreamResolver for KuwoProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to Kuwo".into()));
        }
        // Lossless tiers: the web-signed playUrl endpoint serves them where
        // the anonymous mobile endpoint only returns placeholders. Falls back
        // to mobile 320k/128k on any failure.
        if matches!(
            options.quality,
            StreamQuality::Lossless | StreamQuality::HiRes
        ) && let Ok(url) = self.fetch_web_url(&track.id.id).await
        {
            return Ok(StreamInfo {
                url,
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
            });
        }
        let mut last_error = Error::NotFound;
        for tier in mobile_tiers(options.quality) {
            match self.fetch_mobile_url(&track.id.id, tier).await {
                Ok(url) => {
                    return Ok(StreamInfo {
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
                    });
                }
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }
}

#[async_trait]
impl LyricsProvider for KuwoProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to Kuwo".into()));
        }
        let original = self.fetch_raw_lyric(&track.id.id).await?;
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
/// Kuwo's web session signing (CSRF + secret header flow) is not implemented;
/// the library only serves the anonymous paths, so there is no login state.
#[async_trait]
impl AuthenticationProvider for KuwoProvider {
    async fn login_status(&self) -> Result<LoginStatus> {
        Ok(LoginStatus::LoggedOut)
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
    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = KuwoProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.lyrics);
        // Web session signing is not implemented: no login capability.
        assert!(!descriptor.capabilities.authentication);
        assert!(!descriptor.capabilities.playlists);
        assert!(registry.lyrics_provider(ID).is_some());
    }
}
