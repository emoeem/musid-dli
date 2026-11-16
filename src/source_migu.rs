//! Migu Music provider migrated from VoiceFox's `lx-source::mg`.
//! Protocol origin: lx-music `musicSdk/mg/`. Signed search, three-tier
//! anonymous stream resolution (resourceinfo metadata, H5 strategy endpoint
//! with a magic-prefixed obfuscated response, listenSong 302 fallback) and
//! MRC word-timed lyrics (TEA-decrypted containers) are implemented. Migu
//! anonymously serves only PQ (128k) audio in practice: the resolved quality
//! is corrected against the actually served file size, never the metadata
//! claim. Login, playlists, albums, artists and charts remain pending.
use crate::{
    Error, Result,
    lrc::parse_lrc,
    models::{
        AudioFormat, LyricDocument, LyricFormat, LyricLine, LyricWord, Playlist, PlaylistCategory,
        StreamInfo, StreamQuality, Track, TrackId,
    },
    provider::{
        CollectionProvider, LyricsProvider, MusicProvider, PlaylistProvider, ResolveOptions,
        SearchPage, SearchQuery, StreamResolver, TrackSearch,
    },
    session::SessionStore,
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use md5::{Digest, Md5};
use reqwest::{Client, RequestBuilder, header};
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
/// Detail pages use a 50-per-page window.
const PLAYLIST_PAGE_SIZE: u64 = 50;
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
    client: Client,
    /// listenSong.do answers with a 302 whose Location is the CDN link;
    /// redirects must be followed manually to read it.
    listen_client: Client,
    sessions: SessionStore,
}
impl std::fmt::Debug for MiguProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiguProvider")
            .field("session_store", &"redacted")
            .finish()
    }
}
impl Default for MiguProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl MiguProvider {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| Client::new());
        let listen_client = Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            listen_client,
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
    /// Register all currently implemented Migu capabilities in one step.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_lyrics(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_playlist(provider);
    }
    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        request.header(header::USER_AGENT, UA_MOBILE)
    }
    async fn fetch_bytes(&self, url: &str, label: &str) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .header(header::USER_AGENT, LYRIC_UA)
            .header(header::REFERER, LYRIC_REFERER)
            .header("channel", "0146921")
            .send()
            .await
            .map_err(|e| perr(format!("{label}: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("{label}: HTTP {}", response.status())));
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|e| perr(format!("{label}: {e}")))
    }
    async fn fetch_text(&self, url: &str, label: &str) -> Result<String> {
        let response = self
            .client
            .get(url)
            .header(header::USER_AGENT, LYRIC_UA)
            .header(header::REFERER, LYRIC_REFERER)
            .header("channel", "0146921")
            .send()
            .await
            .map_err(|e| perr(format!("{label}: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("{label}: HTTP {}", response.status())));
        }
        response
            .text()
            .await
            .map_err(|e| perr(format!("{label}: {e}")))
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}
/// Migu search signature: MD5 over keyword, two fixed salts, device id and
/// the millisecond timestamp (which is echoed in a header).
fn mg_sign(keyword: &str) -> (String, String) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string();
    let sign = format!(
        "{:x}",
        Md5::digest(format!(
            "{keyword}{SEARCH_SIGN_MD5}yyapp2d16148780a1dcc7408e06336b98cfd50{SEARCH_DEVICE_ID}{timestamp}"
        )
        .as_bytes())
    );
    (sign, timestamp)
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
    Some(Playlist {
        id: TrackId::new(ID, id.clone()),
        title,
        description: None,
        artwork_url: cover,
        track_count: migu_value_u64(&item["musicNum"]).map(|n| n as u32),
        tracks: vec![],
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
        let response = self
            .request(self.client.get(&url))
            .send()
            .await
            .map_err(|e| perr(format!("search all: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("search all: HTTP {}", response.status())));
        }
        response
            .json()
            .await
            .map_err(|e| perr(format!("search all parse failed: {e}")))
    }
    /// One playlist-detail page: (title, songs, total count).
    async fn fetch_playlist_page(
        &self,
        id: &str,
        page: u32,
    ) -> Result<(Option<String>, Vec<Track>, Option<u64>)> {
        let url = format!(
            "{PLAYLIST_SONG_URL}?pageNo={page}&pageSize={PLAYLIST_PAGE_SIZE}&playlistId={id}"
        );
        let value = self
            .request(self.client.get(&url))
            .header(header::REFERER, "https://m.music.migu.cn/")
            .header("channel", "0146921")
            .header(header::USER_AGENT, PLAYLIST_UA)
            .send()
            .await
            .map_err(|e| perr(format!("playlist detail: {e}")))?;
        if !value.status().is_success() {
            return Err(perr(format!("playlist detail: HTTP {}", value.status())));
        }
        let json: Value = value
            .json()
            .await
            .map_err(|e| perr(format!("playlist detail parse failed: {e}")))?;
        if json["code"].as_str() != Some("000000") {
            return Err(perr("playlist detail request failed"));
        }
        let data = &json["data"];
        let title = migu_first_string(data, &["name", "title", "playlistName"]);
        let songs = data["songList"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(mg_parse_song_alias)
            .collect::<Vec<_>>();
        let total = migu_value_u64(&data["totalCount"])
            .or_else(|| migu_value_u64(&json["data"]["songList"]["totalCount"]));
        Ok((title, songs, total))
    }
}
/// Alias so both the song module and the playlist module share one parser.
fn mg_parse_song_alias(item: &Value) -> Option<Track> {
    mg_parse_song(item)
}
#[async_trait]
impl CollectionProvider for MiguProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist> {
        let (title, tracks, total) = self.fetch_playlist_page(id, 1).await?;
        if tracks.is_empty() {
            return Err(Error::NotFound);
        }
        Ok(Playlist {
            id: TrackId::new(ID, id.to_owned()),
            title: title.unwrap_or_else(|| format!("Migu playlist {id}")),
            description: None,
            artwork_url: None,
            track_count: total.map(|n| n as u32),
            tracks: vec![],
        })
    }
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let mut all = Vec::new();
        let mut current_page = 1u32;
        loop {
            let (_, songs, total) = self.fetch_playlist_page(id, current_page).await?;
            let page_empty = songs.is_empty();
            all.extend(songs);
            let done =
                page_empty || all.len() as u64 >= total.unwrap_or(u64::MAX) || current_page >= 40;
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
        }];
        for (name, group, hot) in PLAYLIST_CATEGORY_KEYWORDS {
            result.push(PlaylistCategory {
                id: (*name).into(),
                name: (*name).into(),
                group: Some((*group).into()),
                count: 0,
                hot: *hot,
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
            let response = self
                .request(self.client.get(&url))
                .header(header::REFERER, "https://m.music.migu.cn/")
                .header("channel", "0146921")
                .header(header::USER_AGENT, PLAYLIST_UA)
                .send()
                .await
                .map_err(|e| perr(format!("hot playlists: {e}")))?;
            if !response.status().is_success() {
                return Err(perr(format!("hot playlists: HTTP {}", response.status())));
            }
            let json: Value = response
                .json()
                .await
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
        Ok(json["songListResultData"]["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(migu_parse_searched_playlist)
            .collect())
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
        let response = self
            .request(self.client.get(&url))
            .header("uiVersion", "A_music_3.6.1")
            .header("deviceId", SEARCH_DEVICE_ID)
            .header("timestamp", &timestamp)
            .header("sign", &sign)
            .header("channel", "0146921")
            .send()
            .await
            .map_err(|e| perr(format!("search: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("search: HTTP {}", response.status())));
        }
        let text = response
            .text()
            .await
            .map_err(|e| perr(format!("search: {e}")))?;
        let json: Value = serde_json::from_str(&text).map_err(|e| perr(format!("search: {e}")))?;
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
}
#[async_trait]
impl StreamResolver for MiguProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to Migu".into()));
        }
        let resource = resource_id(track).ok_or(Error::NotFound)?.to_owned();
        let value = self
            .client
            .post(format!("{RESOURCE_INFO_URL}?resourceType=2"))
            .header(header::USER_AGENT, UA_ANDROID)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(format!("resourceId={resource}"))
            .send()
            .await
            .map_err(|e| perr(format!("resource info: {e}")))?;
        if !value.status().is_success() {
            return Err(Error::NotFound);
        }
        let text = value
            .text()
            .await
            .map_err(|e| perr(format!("resource info: {e}")))?;
        let json: Value = serde_json::from_str(&text)
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
        let mut candidates: Vec<String> = Vec::new();
        let mut primary: Option<(String, Option<u64>)> = None;
        for index in ordered {
            let format = &formats[index];
            for variant in FileVariant::ORDER {
                let Some(url) = format_url(format, variant) else {
                    continue;
                };
                // Only the requested tier becomes the primary URL; other tiers
                // stay candidates so quality never silently downgrades first.
                if primary.is_none() && Some(index) == target_index {
                    primary = Some((url.clone(), extract_size(format, variant)));
                }
                if !candidates.contains(&url) && !url.trim().is_empty() {
                    candidates.push(url);
                }
            }
        }
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
            fallback_urls: candidates,
            index_range: None,
            init_range: None,
            max_chunk_size: None,
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
        // answers "channel不能为空" / "PE参数格式错误".
        let response = self
            .client
            .get(&url)
            .header(header::USER_AGENT, UA_IPHONE)
            .header(header::REFERER, MOBILE_PLAY_REFERER)
            .header("birth", "h5page")
            .header("signature", "1")
            .header("channel", "0146921")
            .send()
            .await
            .map_err(|e| perr(format!("strategy request failed: {e}")))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| perr(format!("strategy body read failed: {e}")))?;
        let json = decrypt_strategy_response(&bytes)?;
        json["data"]["url"]
            .as_str()
            .map(str::trim)
            .filter(|url| url.starts_with("http"))
            .map(str::to_owned)
            .ok_or(Error::NotFound)
    }
    /// listenSong.do answers with a 302 whose Location is the CDN link, or a
    /// JSON body with a code (VIP / no rights / bad params).
    async fn fetch_listen_url(
        &self,
        resource_id: &str,
        tone_flag: &str,
        resource_type: &str,
    ) -> Result<String> {
        let url = format!(
            "{LISTEN_SONG_URL}?toneFlag={tone_flag}&netType=00&userId={MAGIC_USER_ID}&ua=Android_migu&version=5.1&copyrightId=0&contentId={resource_id}&resourceType={resource_type}&channel=0"
        );
        let response = self
            .listen_client
            .get(&url)
            .header(header::USER_AGENT, UA_IPHONE)
            .header(header::REFERER, LISTEN_SONG_REFERER)
            .send()
            .await
            .map_err(|e| perr(format!("listen url request failed: {e}")))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or(Error::NotFound)?
                .to_owned();
            if explicit_preview_url(&location) {
                return Err(Error::NotFound);
            }
            return Ok(location);
        }
        Err(Error::NotFound)
    }
    /// Probe the real served size: HEAD first, then a `Range: bytes=0-0` read.
    async fn probe_served_size(&self, url: &str) -> Option<u64> {
        let decorate = |builder: RequestBuilder| {
            builder
                .header(header::USER_AGENT, UA_IPHONE)
                .header(header::REFERER, LISTEN_SONG_REFERER)
        };
        if let Ok(response) = decorate(self.client.head(url)).send().await
            && response.status().is_success()
            && let Some(length) = response.content_length().filter(|length| *length > 0)
        {
            return Some(length);
        }
        let response = decorate(self.client.get(url))
            .header(header::RANGE, "bytes=0-0")
            .send()
            .await
            .ok()?;
        if response.status().as_u16() == 206
            && let Some(total) = response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
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
    use flate2::{Compression, write::ZlibEncoder};
    use serde_json::json;
    use std::io::Write;

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
        assert!(!descriptor.capabilities.authentication);
        assert!(registry.lyrics_provider(ID).is_some());
        assert!(registry.stream_provider(ID).is_some());
        assert!(registry.playlist_provider(ID).is_some());
        assert!(registry.collection_provider(ID).is_some());
    }
}
