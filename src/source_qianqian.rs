//! Qianqian Music (千千音乐, 91q.com) provider migrated from VoiceFox's
//! `lx_source::qianqian` (source/src/qianqian/: mod, crypto, song, url,
//! lyric, playlist, album, parse). All endpoints sign with
//! `MD5(sorted `k=v&` pairs (unencoded) + timestamp + fixed secret)`; search
//! distinguishes songs/albums/playlists by `type`.
//!
//! Migration basis: the 2026-11-17 viability probe had found the search paths
//! dead (docs/PROVIDER_EVALUATION.md:15-21), and the 2026-11-19 anonymous
//! re-probe performed for this slice found the service revived. Live facts
//! recorded then (all anonymous read-only GETs, ≤3 per endpoint):
//!
//! - song search (`/v1/search?type=1`), album search (`type=3`), the playlist
//!   category directory, category lists, playlist detail, song info and album
//!   info all answer HTTP 200 `errno 22000` anonymously;
//! - **playlist keyword search is dead live**: `/v1/search?type=6` answers
//!   `{"state":false,"errno":23001,"errmsg":"不被支持的搜索类型(10000)"}`;
//!   the upstream code is still ported (upstream parity: it reads
//!   `data.typeSonglist` unconditionally, so a refusal parses to an empty
//!   result, never an error);
//! - **tracklink signing changed**: the upstream recipe excludes `rate` from
//!   the signed text (crypto.rs:22-25); the live server answers the excluded
//!   shape `errno 22001 签名错误:sign无效` and accepts `rate` inside the
//!   signed text. musid-dli signs `rate` like any other parameter — a
//!   documented deviation pinned by the live probe and `sign_vectors.json`;
//! - a VIP track (`isVip:1`, `pay_model:3`) answers tracklink with full
//!   metadata (size/filemd5/expireTime) but **no `path`**, so anonymous
//!   resolution of a paid track exhausts the ladder and answers `NotFound`
//!   (upstream parity: `url.rs` requires a non-empty path); a free track
//!   (`isVip:0`) carries a real pre-signed CDN URL with `size`, `filemd5`,
//!   `expireTime` and `format`, which backfill the M3 stream fields;
//! - `/v1/album/info` trackList rows carry **no `TSID` key — only
//!   `assetId`** (same value). Upstream `parse_song` probes only
//!   `TSID|tsid` and would drop every row (NotFound); musid-dli extends the
//!   id probe with `assetId` — the second documented deviation, pinned by
//!   `tests/fixtures/qianqian/album_info.json`.

use crate::{
    Error, ErrorDetail, Result,
    lrc::parse_lrc,
    models::{
        AudioFormat, LyricDocument, LyricFormat, Playlist, PlaylistCategory, StreamInfo,
        StreamQuality, Track, TrackId,
    },
    provider::{
        AlbumProvider, CollectionProvider, LinkResolver, LyricsProvider, MusicProvider,
        PlaylistProvider, ResolveOptions, ResolvedLink, SearchPage, SearchQuery, StreamResolver,
        TrackSearch,
    },
    transport::{ClientProfile, Http, RetryPolicy, TransportError, TransportRequest},
};
use async_trait::async_trait;
use md5::{Digest, Md5};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

const ID: &str = "qianqian";
const NAME: &str = "千千音乐";

/// Qianqian web appid and signing secret (upstream crypto.rs:11-12).
const APP_ID: &str = "16073360";
const SECRET: &str = "0b50b02fd0d73a9c4c8c3a781c30845f";
const UA_WEB: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const REFERER: &str = "https://music.91q.com/player";

const SEARCH_API: &str = "https://music.91q.com/v1/search";
const TRACKLINK_API: &str = "https://music.91q.com/v1/song/tracklink";
const SONG_INFO_API: &str = "https://music.91q.com/v1/song/info";
const PLAYLIST_CATEGORY_API: &str = "https://music.91q.com/v1/tracklist/category";
const PLAYLIST_LIST_API: &str = "https://music.91q.com/v1/tracklist/list";
const PLAYLIST_INFO_API: &str = "https://music.91q.com/v1/tracklist/info";
const ALBUM_INFO_API: &str = "https://music.91q.com/v1/album/info";
const ALBUM_ID2PSID_API: &str = "https://music.91q.com/v1/album/albumid2psid";

/// `/v1/search` `type` values (upstream song.rs:17-19).
const TYPE_SONG: &str = "1";
const TYPE_PLAYLIST: &str = "6";

#[derive(Clone)]
pub struct QianqianProvider {
    http: Http,
}

impl std::fmt::Debug for QianqianProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QianqianProvider").finish_non_exhaustive()
    }
}

impl Default for QianqianProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl QianqianProvider {
    pub fn new() -> Self {
        Self {
            http: Http::from_profile(&ClientProfile::provider_default()),
        }
    }
    /// Test seam: inject a transport handle (a mock in unit tests). The
    /// production constructor is [`QianqianProvider::new`]. Test-only so
    /// non-test builds see no dead constructor.
    #[cfg(test)]
    pub(crate) fn with_transport(http: Http) -> Self {
        Self { http }
    }
    /// Register all currently implemented qianqian capabilities in one step.
    /// No authentication slot exists upstream (no login surface on these
    /// endpoints), so none is registered; `artists`/`charts` are not
    /// implemented either (see [`QianqianProvider::capabilities`]).
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_lyrics(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_playlist(provider.clone());
        registry.register_album(provider.clone());
        registry.register_link_resolver(provider);
    }
    /// Adds the fixed web identity headers to a [`TransportRequest`]. The
    /// anonymous web endpoints expect the Chrome UA plus the player Referer
    /// (upstream crypto.rs:14-15, attached at song.rs:52-53).
    fn request(&self, request: TransportRequest) -> TransportRequest {
        request
            .header("user-agent", UA_WEB)
            .header("referer", REFERER)
    }
    /// Signed GET through the unified transport layer.
    ///
    /// Qianqian ignores `Accept-Encoding` and always answers gzip (upstream
    /// song.rs:44-49 documents the quirk), so the body is sniffed for the
    /// `1f 8b` magic and gunzipped here instead of enabling reqwest's global
    /// gzip feature.
    async fn signed_get(&self, url: &str) -> Result<Value> {
        let request = self.request(TransportRequest::get(url));
        let response = match self.http.send_raw(&request, &RetryPolicy::standard()).await {
            Ok(response) => response,
            Err(TransportError::Status(message)) => {
                return Err(Error::Network {
                    provider: Some(ID.into()),
                    detail: Some(ErrorDetail::new(message)),
                });
            }
            Err(error) => {
                return Err(Error::Network {
                    provider: Some(ID.into()),
                    detail: Some(ErrorDetail::new(error.to_string())),
                });
            }
        };
        decode_signed_body(&response.body)
    }
    /// Plain GET for the lyric file on the static CDN (upstream lyric.rs:34-43;
    /// unsigned, same web identity).
    async fn fetch_lyric_text(&self, url: &str) -> Result<String> {
        let request = self.request(TransportRequest::get(url));
        let response = self
            .http
            .send_raw(&request, &RetryPolicy::standard())
            .await
            .map_err(|error| Error::Network {
                provider: Some(ID.into()),
                detail: Some(ErrorDetail::new(error.to_string())),
            })?;
        let text = response.text().trim().to_owned();
        if text.is_empty() {
            return Err(Error::NotFound);
        }
        Ok(text)
    }
    async fn search_by_type(
        &self,
        keyword: &str,
        search_type: &str,
        page: u32,
        limit: u32,
    ) -> Result<Value> {
        let query = signed_query(&[
            ("word", keyword),
            ("type", search_type),
            ("pageNo", &page.max(1).to_string()),
            ("pageSize", &limit.max(1).to_string()),
            ("appid", APP_ID),
        ]);
        self.signed_get(&format!("{SEARCH_API}?{query}")).await
    }
}

fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}

/// gzip-sniffing body decoder of `signed_get` (upstream song.rs:60-69), as a
/// pure function so fixture tests can drive it directly.
fn decode_signed_body(body: &[u8]) -> Result<Value> {
    let decoded = if body.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = flate2::read::GzDecoder::new(body);
        let mut text = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut text).map_err(|error| {
            Error::InvalidResponse {
                provider: Some(ID.into()),
                code: None,
                detail: Some(ErrorDetail::new(format!("gzip decode failed: {error}"))),
            }
        })?;
        text
    } else {
        body.to_vec()
    };
    serde_json::from_slice(&decoded).map_err(|error| Error::InvalidResponse {
        provider: Some(ID.into()),
        code: None,
        detail: Some(ErrorDetail::new(format!("parse failed: {error}"))),
    })
}

/// Generates the signed query string (upstream crypto.rs:18-59): the
/// parameters plus `timestamp` are sorted by key, joined as unencoded
/// `k=v&...`, suffixed with the secret and MD5-hexed as `sign`; the final
/// query carries the URL-encoded pairs in sorted order followed by `sign`.
///
/// Deviation from upstream `signed_tracklink_query` (crypto.rs:22-25): the
/// tracklink `rate` rides inside `params`, i.e. it IS signed. Upstream
/// excludes it (a historical web quirk); the 2026-11-19 live probe showed the
/// excluded shape now answers `errno 22001 签名错误:sign无效` while the
/// signed shape is accepted, so this port signs `rate` normally (module docs,
/// `sign_vectors.json`).
fn signed_query(params: &[(&str, &str)]) -> String {
    signed_query_at(params, unix_seconds())
}

/// Same computation at a fixed timestamp — the unit-testable core that the
/// `sign_vectors.json` goldens pin.
fn signed_query_at(params: &[(&str, &str)], timestamp: u64) -> String {
    let mut pairs: Vec<(String, String)> = params
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    pairs.push(("timestamp".to_string(), timestamp.to_string()));
    pairs.sort_by(|left, right| left.0.cmp(&right.0));
    let joined = pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let sign = hex::encode(Md5::digest(format!("{joined}{SECRET}").as_bytes()));
    pairs.push(("sign".to_string(), sign));
    pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                urlencoding::encode(key),
                urlencoding::encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// Case-tolerant field probe (upstream song.rs:111-116): the API returned
/// both `TSID` and `tsid` spellings across versions.
fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .get(name)
        .or_else(|| value.get(name.to_ascii_uppercase()))
        .or_else(|| value.get(name.to_ascii_lowercase()))
}

/// Track id extraction for a resource row. Upstream probes `field(resource,
/// "tsid")` only; musid-dli adds the `assetId` fallback because the live
/// `/v1/album/info` trackList rows carry the TSID value under `assetId` and
/// no `TSID` key at all (module docs, album_info.json fixture).
fn track_tsid(resource: &Value) -> Option<&Value> {
    field(resource, "tsid").or_else(|| resource.get("assetId"))
}

fn value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
        .unwrap_or_default()
}

fn value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

/// Artist names as the model's `Vec<String>` (upstream `join_artists`
/// song.rs:135-144 joins them with `、` into one string; the musid-dli
/// `Track.artists` slot is a vector, so each element keeps its own entry and
/// the join is dropped).
fn artist_names(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|artist| artist["name"].as_str())
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// Quality tiers: a `rateFileInfo` entry counts only with a size above 0
/// (upstream song.rs:147-170); `3000`→Lossless, `320`→High, `128`/`64`→Low.
/// Returned tier-ascending (the model's `available_qualities` contract).
fn qualities_from_rates(value: &Value) -> Vec<StreamQuality> {
    let mut qualities = BTreeMap::new();
    if let Some(rates) = value.as_object() {
        for (rate, info) in rates {
            if value_u64(&info["size"]).unwrap_or_default() == 0 {
                continue;
            }
            let quality = match rate.as_str() {
                "3000" => StreamQuality::Lossless,
                "320" => StreamQuality::High,
                "128" | "64" => StreamQuality::Low,
                _ => continue,
            };
            qualities.insert(quality, ());
        }
    }
    qualities.into_keys().collect()
}

/// Shared track parser (upstream song.rs:172-211). `None` without a usable
/// id or a non-empty title.
fn parse_track(resource: &Value) -> Option<Track> {
    let tsid = track_tsid(resource).map(value_string)?;
    if tsid.is_empty() {
        return None;
    }
    let title = resource["title"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if title.is_empty() {
        return None;
    }
    let album_code = resource["albumAssetCode"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut extra = BTreeMap::new();
    extra.insert("tsid".to_string(), tsid.clone());
    if !album_code.is_empty() {
        extra.insert("album_asset_code".to_string(), album_code.clone());
    }
    Some(Track {
        id: TrackId::new(ID, tsid),
        title,
        artists: artist_names(&resource["artist"]),
        album: (!resource["albumTitle"]
            .as_str()
            .unwrap_or_default()
            .is_empty())
        .then(|| {
            resource["albumTitle"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }),
        album_id: (!album_code.is_empty()).then_some(album_code),
        duration: {
            let secs = value_u64(&resource["duration"]).unwrap_or_default();
            (secs > 0).then(|| Duration::from_secs(secs))
        },
        artwork_url: resource["pic"]
            .as_str()
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        isrc: None,
        available_qualities: qualities_from_rates(&resource["rateFileInfo"]),
        audio: None,
        extra,
    })
}

/// Ladder of rates per requested quality; a request never tries above its
/// tier (upstream url.rs:16-22). `Lossless`/`HiRes` start at 3000.
fn rate_ladder(quality: StreamQuality) -> &'static [&'static str] {
    match quality {
        StreamQuality::Lossless | StreamQuality::HiRes => &["3000", "320", "128", "64"],
        StreamQuality::High => &["320", "128", "64"],
        StreamQuality::Low | StreamQuality::Standard | StreamQuality::Unknown => &["128", "64"],
    }
}

/// The achieved tier of a served rate (upstream url.rs:25-31).
fn quality_for_rate(rate: &str) -> StreamQuality {
    match rate {
        "3000" => StreamQuality::Lossless,
        "320" => StreamQuality::High,
        _ => StreamQuality::Low,
    }
}

fn audio_format_of(format: &str) -> AudioFormat {
    match format {
        "mp3" => AudioFormat::Mp3,
        "flac" => AudioFormat::Flac,
        "aac" => AudioFormat::Aac,
        "m4a" => AudioFormat::M4a,
        "ogg" => AudioFormat::Ogg,
        "opus" => AudioFormat::Opus,
        "wav" => AudioFormat::Wav,
        _ => AudioFormat::Unknown,
    }
}

/// M3 stream fields from a successful tracklink `data` block (upstream
/// `build_song_url` url.rs:72-87 reads only `size`; the live data block also
/// carries `filemd5`/`expireTime`/`format`, which backfill the structured
/// slots). `size_is_advisory` stays `true` with the upstream reason: the
/// reported size may drift a few bytes from the served file (url.rs:81-82).
fn stream_info_from_data(path: &str, rate: &str, data: &Value, track: &Track) -> StreamInfo {
    StreamInfo {
        url: path.to_string(),
        quality: quality_for_rate(rate),
        format: audio_format_of(data["format"].as_str().unwrap_or_default()),
        duration: track.duration,
        expires_at_unix: value_u64(&data["expireTime"]),
        content_length: value_u64(&data["size"]).filter(|size| *size > 0),
        // The endpoint names the digest `filemd5`; `checksum_algorithm` stays
        // `None` (the MD5 default) so the engine's MD5 gate consumes it.
        checksum: data["filemd5"]
            .as_str()
            .filter(|md5| !md5.is_empty())
            .map(str::to_string),
        checksum_algorithm: None,
        headers: vec![],
        fallback_urls: vec![],
        fallback_endpoints: vec![],
        index_range: None,
        init_range: None,
        max_chunk_size: None,
        size_is_advisory: true,
    }
}

/// The `state`/`errno` gate of the two tracklist endpoints (upstream
/// playlist.rs:58-60, :110-112): `state == true` OR `errno == 22000` passes;
/// anything else is a server refusal.
fn tracklist_gate(json: &Value, label: &str) -> Result<()> {
    if json["state"] == Value::Bool(true) || json["errno"].as_i64() == Some(22000) {
        return Ok(());
    }
    Err(Error::Unavailable {
        provider: Some(ID.into()),
        detail: Some(ErrorDetail::new(format!(
            "{label} (errno {})",
            json["errno"].as_i64().unwrap_or_default()
        ))),
    })
}

/// Playlist entry from a search/list row (upstream playlist.rs:15-36). The
/// M3 slots are real producers here: `play_count` from `playCount`,
/// `link` built from the id, `extra.playlist_id` routing data.
fn parse_playlist(item: &Value) -> Option<Playlist> {
    let id = field(item, "id").map(value_string)?;
    let title = item["title"].as_str()?.trim().to_string();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    let mut extra = BTreeMap::new();
    extra.insert("playlist_id".to_string(), id.clone());
    Some(Playlist {
        id: TrackId::new(ID, id.clone()),
        title,
        description: item["desc"]
            .as_str()
            .or_else(|| item["tag"].as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string),
        artwork_url: item["pic"]
            .as_str()
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        track_count: {
            let count = value_u64(&item["trackCount"]).unwrap_or_default();
            (count > 0).then_some(count as u32)
        },
        tracks: vec![],
        play_count: value_u64(&item["playCount"]),
        creator: None,
        link: Some(format!("https://music.91q.com/songlist/{id}")),
        extra,
    })
}

/// Playlist detail + metadata (upstream playlist.rs:138-177). `NotFound`
/// when the parsed track list is empty (upstream uses it as the
/// missing-playlist signal). The creator probe keeps the upstream five-key
/// order; the 2026-11-19 live detail carries none of them, so `creator` is
/// `None` live (documented, not dead code: a future shape may restore it).
async fn get_playlist_detail_with_meta(
    provider: &QianqianProvider,
    id: &str,
) -> Result<(Playlist, Vec<Track>)> {
    let query = signed_query(&[("id", id), ("appid", APP_ID), ("type", "0")]);
    let json = provider
        .signed_get(&format!("{PLAYLIST_INFO_API}?{query}"))
        .await?;
    let data = &json["data"];
    let tracks = data["trackList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_track)
        .collect::<Vec<_>>();
    if tracks.is_empty() {
        return Err(Error::NotFound);
    }
    let name = data["title"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("千千歌单 {id}"));
    let mut extra = BTreeMap::new();
    extra.insert("playlist_id".to_string(), id.to_string());
    let playlist = Playlist {
        id: TrackId::new(ID, id.to_string()),
        title: name,
        description: data["desc"]
            .as_str()
            .or_else(|| data["description"].as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string),
        artwork_url: data["pic"]
            .as_str()
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        track_count: {
            let count = value_u64(&data["trackCount"]).unwrap_or(tracks.len() as u64);
            (count > 0).then_some(count as u32)
        },
        tracks: vec![],
        play_count: value_u64(&data["playCount"]),
        creator: ["creator", "author", "userName", "nickName", "ownerName"]
            .iter()
            .find_map(|key| data[*key].as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string),
        link: Some(format!("https://music.91q.com/songlist/{id}")),
        extra,
    };
    Ok((playlist, tracks))
}

/// Album AssetCode sanitization (upstream album.rs:16-25): keep ASCII
/// alphanumerics only.
fn normalize_asset_code(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect()
}

/// Numeric albumid → albumAssetCode swap (upstream album.rs:104-128); an
/// already-alphanumeric code returns as-is.
async fn resolve_asset_code(provider: &QianqianProvider, id: &str) -> Result<String> {
    let normalized = normalize_asset_code(id);
    if normalized.is_empty() {
        return Err(perr("千千专辑 ID 为空"));
    }
    if !normalized
        .chars()
        .all(|character| character.is_ascii_digit())
    {
        return Ok(normalized);
    }
    let query = signed_query(&[("albumid", &normalized), ("appid", APP_ID)]);
    let json = provider
        .signed_get(&format!("{ALBUM_ID2PSID_API}?{query}"))
        .await?;
    field(&json["data"], "albumAssetCode")
        .map(value_string)
        .map(|value| normalize_asset_code(&value))
        .filter(|value| !value.is_empty())
        .ok_or(Error::NotFound)
}

/// Album info + tracks (upstream album.rs:66-101). `NotFound` on an empty
/// parsed track list, exactly like upstream.
async fn get_album_with_meta(
    provider: &QianqianProvider,
    id: &str,
) -> Result<(crate::models::Album, Vec<Track>)> {
    let album_code = resolve_asset_code(provider, id).await?;
    let query = signed_query(&[("albumAssetCode", &album_code), ("appid", APP_ID)]);
    let json = provider
        .signed_get(&format!("{ALBUM_INFO_API}?{query}"))
        .await?;
    let data = &json["data"];
    let tracks = data["trackList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(parse_track)
        .collect::<Vec<_>>();
    if tracks.is_empty() {
        return Err(Error::NotFound);
    }
    let name = data["title"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("千千专辑 {album_code}"));
    let album = crate::models::Album {
        id: TrackId::new(ID, album_code),
        title: name,
        artists: artist_names(&data["artist"]),
        artwork_url: data["pic"]
            .as_str()
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        track_count: Some(tracks.len() as u32),
    };
    Ok((album, tracks))
}

/// Song metadata by TSID (upstream parse.rs fetch_song, lyric.rs shared
/// endpoint): `/v1/song/info` first item, `NotFound` without a titled item.
/// The shared `parse_track` parser is a superset of the upstream hand-built
/// `SongInfo` (same fields plus `rateFileInfo`), so link-resolved tracks
/// carry qualities too.
async fn fetch_track(provider: &QianqianProvider, tsid: &str) -> Result<Track> {
    let query = signed_query(&[("TSID", tsid), ("appid", APP_ID)]);
    let json = provider
        .signed_get(&format!("{SONG_INFO_API}?{query}"))
        .await?;
    json["data"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(parse_track)
        .ok_or(Error::NotFound)
}

/// Lyric URL from the `/v1/song/info` first item (upstream lyric.rs:26-32).
fn lyric_url_of(json: &Value) -> Option<String> {
    json["data"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| field(item, "lyric"))
        .map(value_string)
        .filter(|url| !url.is_empty())
}

/// Link-target router (upstream parse.rs:23-62). Shapes: song
/// `music.91q.com/song/{TSID}`, playlist `(songlist|tracklist|playlist)/{id}`
/// plus `(songlistid|tracklistid|playlistid)=`, album `album/{code}`,
/// `albumAssetCode=` and numeric `albumid=`; a bare alphanumeric share id is
/// a playlist. Every capture is unanchored (no scheme/host validation
/// upstream — inherited gap).
fn qq_link_target(link: &str) -> Option<(String, String)> {
    let trimmed = link.trim();
    if let Some(tsid) = capture(r"music\.91q\.com/song/([A-Za-z0-9]+)", trimmed) {
        return Some(("song".into(), tsid));
    }
    for pattern in ["songlist", "tracklist", "playlist"] {
        if let Some(id) = capture(
            &format!(r"music\.91q\.com/{pattern}/([A-Za-z0-9]+)"),
            trimmed,
        ) {
            return Some(("playlist".into(), id));
        }
    }
    if let Some(id) = capture(r"music\.91q\.com/album/([A-Za-z0-9]+)", trimmed) {
        return Some(("album".into(), id));
    }
    if let Some(id) = capture(r"albumAssetCode=([A-Za-z0-9]+)", trimmed) {
        return Some(("album".into(), id));
    }
    if let Some(id) = capture(r"albumid=(\d+)", trimmed) {
        return Some(("album".into(), id));
    }
    for key in ["songlistid", "tracklistid", "playlistid"] {
        if let Some(id) = capture(&format!(r"{key}=([A-Za-z0-9]+)"), trimmed) {
            return Some(("playlist".into(), id));
        }
    }
    // Bare alphanumeric share id (common in share texts) is a playlist
    // (upstream parse.rs:50-60, music-lib parity).
    if !trimmed.is_empty()
        && !trimmed.contains('/')
        && trimmed
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Some(("playlist".into(), trimmed.to_string()));
    }
    None
}

fn capture(pattern: &str, value: &str) -> Option<String> {
    regex::Regex::new(pattern)
        .ok()?
        .captures(value)
        .and_then(|captures| captures.get(1))
        .map(|matched| matched.as_str().to_string())
}

impl MusicProvider for QianqianProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        NAME
    }
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            lyrics: true,
            playlists: true,
            albums: true,
            link_resolve: true,
            ..Default::default()
        }
    }
    /// Upstream tier order Low128/High320/Flac (mod.rs:81-83).
    fn supported_qualities(&self) -> Vec<StreamQuality> {
        vec![
            StreamQuality::Low,
            StreamQuality::High,
            StreamQuality::Lossless,
        ]
    }
}

#[async_trait]
impl TrackSearch for QianqianProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.keyword.trim().is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        let json = self
            .search_by_type(q.keyword.trim(), TYPE_SONG, q.page, q.page_size)
            .await?;
        // Upstream parity: the search envelope is never gated on
        // `state`/`errno` (song.rs:96-108) — a refusal body carries
        // `data: []`, which parses to an empty page.
        let items = json["data"]["typeTrack"]
            .as_array()
            .map(|items| items.iter().filter_map(parse_track).collect::<Vec<_>>())
            .unwrap_or_default();
        let total = items.len() as u64;
        Ok(SearchPage {
            items,
            total: Some(total),
            has_more: (total as u32) >= q.page_size.max(1),
        })
    }
}

#[async_trait]
impl StreamResolver for QianqianProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to qianqian".into(),
            ));
        }
        let tsid = track
            .extra
            .get("tsid")
            .cloned()
            .unwrap_or_else(|| track.id.id.clone());
        if tsid.is_empty() {
            return Err(Error::NotFound);
        }
        // Rate ladder with the tracklink preview fallback (upstream url.rs:33-70):
        // every rate whose `data.path` is missing is skipped; the first
        // `trail_audio_info.path` is remembered and served when no full file
        // resolves. A VIP track answers path-less metadata anonymously, so
        // the ladder exhausts into `NotFound` (upstream parity, live-pinned).
        let mut preview: Option<(String, String)> = None;
        for rate in rate_ladder(options.quality) {
            let query = signed_query(&[("TSID", &tsid), ("appid", APP_ID), ("rate", rate)]);
            let json = match self.signed_get(&format!("{TRACKLINK_API}?{query}")).await {
                Ok(json) => json,
                Err(error) => {
                    tracing::debug!("qianqian tracklink failed for rate {rate}: {error}");
                    continue;
                }
            };
            let data = &json["data"];
            let path = data["path"].as_str().unwrap_or_default();
            if !path.is_empty() {
                return Ok(stream_info_from_data(path, rate, data, track));
            }
            if preview.is_none()
                && let Some(trail) = data["trail_audio_info"]["path"].as_str()
                && !trail.is_empty()
            {
                preview = Some((trail.to_string(), (*rate).to_string()));
            }
        }
        let (url, rate) = preview.ok_or(Error::NotFound)?;
        tracing::debug!("qianqian only returned a preview clip at rate {rate}");
        Ok(stream_info_from_data(&url, &rate, &Value::Null, track))
    }
}

#[async_trait]
impl LyricsProvider for QianqianProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to qianqian".into(),
            ));
        }
        let tsid = track
            .extra
            .get("tsid")
            .cloned()
            .unwrap_or_else(|| track.id.id.clone());
        if tsid.is_empty() {
            return Err(Error::NotFound);
        }
        let query = signed_query(&[("TSID", &tsid), ("appid", APP_ID)]);
        let json = self.signed_get(&format!("{SONG_INFO_API}?{query}")).await?;
        let lyric_url = lyric_url_of(&json).ok_or(Error::NotFound)?;
        let original = self.fetch_lyric_text(&lyric_url).await?;
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
impl CollectionProvider for QianqianProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist> {
        if id.trim().is_empty() {
            return Err(Error::InvalidInput("playlist id is empty".into()));
        }
        Ok(get_playlist_detail_with_meta(self, id.trim()).await?.0)
    }
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        if id.trim().is_empty() {
            return Err(Error::InvalidInput("playlist id is empty".into()));
        }
        Ok(get_playlist_detail_with_meta(self, id.trim()).await?.1)
    }
}

#[async_trait]
impl PlaylistProvider for QianqianProvider {
    /// Category directory (upstream playlist.rs:51-93): the synthetic leading
    /// 全部 category, then each group's `subCate` entries.
    async fn categories(&self) -> Result<Vec<PlaylistCategory>> {
        let query = signed_query(&[("appid", APP_ID)]);
        let json = self
            .signed_get(&format!("{PLAYLIST_CATEGORY_API}?{query}"))
            .await?;
        tracklist_gate(&json, "千千音乐歌单分类请求失败")?;
        let mut categories = vec![PlaylistCategory {
            id: String::new(),
            name: "全部".to_string(),
            provider: Some(ID.to_string()),
            group: Some("全部".to_string()),
            count: 0,
            hot: true,
            extra: BTreeMap::new(),
        }];
        for group in json["data"].as_array().into_iter().flatten() {
            let group_name = group["categoryName"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            for item in group["subCate"].as_array().into_iter().flatten() {
                let id = value_string(&item["id"]);
                let name = item["categoryName"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if id.is_empty() || name.is_empty() {
                    continue;
                }
                categories.push(PlaylistCategory {
                    id,
                    name,
                    provider: Some(ID.to_string()),
                    group: (!group_name.is_empty()).then(|| group_name.clone()),
                    count: value_u64(&item["count"]).unwrap_or_default() as u32,
                    hot: false,
                    extra: BTreeMap::new(),
                });
            }
        }
        Ok(categories)
    }
    /// Category playlists (upstream playlist.rs:96-135): `pageSize` is the
    /// upstream-fixed 30; the trait's `page_size` caps the logical fill.
    async fn playlists(&self, category: &str, page: u32, page_size: u32) -> Result<Vec<Playlist>> {
        let page_no = page.max(1).to_string();
        let mut params = vec![
            ("appid", APP_ID),
            ("pageNo", page_no.as_str()),
            ("pageSize", "30"),
        ];
        if !category.trim().is_empty() {
            params.push(("subCateId", category.trim()));
        }
        let query = signed_query(&params);
        let json = self
            .signed_get(&format!("{PLAYLIST_LIST_API}?{query}"))
            .await?;
        tracklist_gate(&json, "千千音乐分类歌单请求失败")?;
        let fill = if page_size == 0 {
            30
        } else {
            page_size.min(30)
        } as usize;
        Ok(json["data"]["result"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let mut playlist = parse_playlist(item)?;
                // tagList backfill when neither desc nor tag exists (upstream
                // playlist.rs:117-131); live rows carry an empty desc, so the
                // joined tags are the real description.
                if playlist.description.is_none()
                    && let Some(tags) = item["tagList"].as_array()
                {
                    let joined = tags
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("、");
                    playlist.description = (!joined.is_empty()).then_some(joined);
                }
                playlist
                    .extra
                    .insert("category_id".to_string(), category.trim().to_string());
                Some(playlist)
            })
            .take(fill)
            .collect())
    }
    /// Playlist keyword search (upstream playlist.rs:38-48, `/v1/search`
    /// `type=6`). Live status 2026-11-19: refused with
    /// `errno 23001 不被支持的搜索类型(10000)`; upstream parity parses the
    /// refusal body to an empty result instead of an error, so the port keeps
    /// that shape (see the fixture README and the matrix row).
    async fn search_playlists(&self, keyword: &str, page: u32) -> Result<Vec<Playlist>> {
        if keyword.trim().is_empty() {
            return Err(Error::InvalidInput(
                "playlist search keyword is empty".into(),
            ));
        }
        let json = self
            .search_by_type(keyword.trim(), TYPE_PLAYLIST, page, 30)
            .await?;
        Ok(json["data"]["typeSonglist"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_playlist)
            .collect())
    }
    /// No upstream implementation exists (voicefox qianqian ships no user
    /// playlist call).
    async fn user_playlists(&self, _page: u32, _page_size: u32) -> Result<Vec<Playlist>> {
        Err(Error::Unsupported(ID.into()))
    }
}

#[async_trait]
impl AlbumProvider for QianqianProvider {
    async fn album(&self, id: &str) -> Result<crate::models::Album> {
        if id.trim().is_empty() {
            return Err(Error::InvalidInput("album id is empty".into()));
        }
        Ok(get_album_with_meta(self, id.trim()).await?.0)
    }
    async fn album_tracks(&self, id: &str) -> Result<Vec<Track>> {
        if id.trim().is_empty() {
            return Err(Error::InvalidInput("album id is empty".into()));
        }
        Ok(get_album_with_meta(self, id.trim()).await?.1)
    }
}

#[async_trait]
impl LinkResolver for QianqianProvider {
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink> {
        let (kind, id) = qq_link_target(url)
            .ok_or_else(|| Error::InvalidInput("无法识别千千音乐链接中的歌曲/歌单/专辑".into()))?;
        match kind.as_str() {
            "song" => Ok(ResolvedLink::Track(fetch_track(self, &id).await?)),
            // The M3 `ParsedLink` songs-vector drop (MODEL_ALIGNMENT_DESIGN.md
            // §5.2): the container resolves metadata-only and the tracks stay
            // reachable through `playlist_tracks`/`album_tracks`.
            "playlist" => Ok(ResolvedLink::Playlist(
                get_playlist_detail_with_meta(self, &id).await?.0,
            )),
            "album" => Ok(ResolvedLink::Album(get_album_with_meta(self, &id).await?.0)),
            _ => Err(Error::InvalidInput("unrecognized qianqian link".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, MockTransport, TransportResponse};
    use serde_json::json;
    use std::sync::Arc;

    /// Loads a fixture from tests/fixtures/qianqian (see the README there for
    /// the derivation: trimmed verbatim live captures from the 2026-11-19
    /// anonymous re-probe, upstream inline goldens, and formula-derived
    /// signature vectors).
    macro_rules! fixture {
        ($name:expr) => {
            serde_json::from_str::<Value>(include_str!(concat!(
                "../tests/fixtures/qianqian/",
                $name
            )))
            .expect("qianqian fixture must be valid JSON")
        };
    }

    fn mock_json(status: u16, body: &str) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: body.as_bytes().to_vec(),
        }
    }

    fn mock_bytes(status: u16, body: Vec<u8>) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body,
        }
    }

    #[test]
    fn signature_matches_upstream_shape_and_live_goldens() {
        // Upstream crypto.rs:73-80 stability/shape contract.
        let query = signed_query(&[("word", "周杰伦"), ("type", "1"), ("appid", APP_ID)]);
        assert!(query.contains("appid=16073360"));
        assert!(query.contains("sign="));
        assert!(query.contains("timestamp="));
        assert!(
            query.starts_with("appid="),
            "key-sorted order puts appid first"
        );
        // The three sign_vectors.json goldens (formula-derived with an
        // independent Python transcription; the search and categories vectors
        // were additionally live-accepted 2026-11-19).
        let cases = fixture!("sign_vectors.json")["cases"]
            .as_array()
            .unwrap()
            .to_vec();
        assert!(cases.len() >= 3);
        for case in &cases {
            let empty_extra: Vec<Value> = Vec::new();
            let extra = case["extra"].as_array().unwrap_or(&empty_extra);
            let mut params: Vec<(&str, &str)> = case["params"]
                .as_array()
                .unwrap()
                .iter()
                .map(|pair| (pair[0].as_str().unwrap(), pair[1].as_str().unwrap()))
                .collect();
            for pair in extra {
                params.push((pair[0].as_str().unwrap(), pair[1].as_str().unwrap()));
            }
            let timestamp = case["timestamp"].as_str().unwrap().parse::<u64>().unwrap();
            let query = signed_query_at(&params, timestamp);
            // The signed-text section (everything before `&sign=`) must equal
            // the expected sorted pairs, URL-encoded; the sign must be the
            // pinned MD5.
            let (encoded_signed, sign) = query.rsplit_once("&sign=").unwrap();
            let expected_signed = case["signed_text"]
                .as_str()
                .unwrap()
                .split('&')
                .map(|pair| {
                    let (key, value) = pair.split_once('=').unwrap();
                    format!(
                        "{}={}",
                        urlencoding::encode(key),
                        urlencoding::encode(value)
                    )
                })
                .collect::<Vec<_>>()
                .join("&");
            assert_eq!(
                encoded_signed,
                expected_signed,
                "case {}",
                case["name"].as_str().unwrap()
            );
            assert_eq!(
                sign,
                case["sign"].as_str().unwrap(),
                "case {}",
                case["name"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn rate_is_inside_the_signed_text() {
        // The documented deviation (module docs): upstream excludes `rate`
        // from the tracklink signed text; musid-dli signs it. With rate
        // signed, the sorted signed text reads TSID..appid..rate..timestamp.
        let query = signed_query_at(
            &[("TSID", "T1"), ("appid", APP_ID), ("rate", "128")],
            1_795_045_949,
        );
        let signed = query.split("&sign=").next().unwrap();
        let plain: String = signed
            .split('&')
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap();
                format!("{key}={}", urlencoding::decode(value).unwrap())
            })
            .collect::<Vec<_>>()
            .join("&");
        assert_eq!(
            plain,
            "TSID=T1&appid=16073360&rate=128&timestamp=1795045949"
        );
    }

    #[test]
    fn decodes_gzip_and_plain_bodies() {
        let value = json!({"state": true, "errno": 22000});
        let plain = serde_json::to_vec(&value).unwrap();
        assert_eq!(decode_signed_body(&plain).unwrap(), value);
        let mut gzipped = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gzipped, &plain).unwrap();
        let gzipped = gzipped.finish().unwrap();
        assert!(gzipped.starts_with(&[0x1f, 0x8b]));
        assert_eq!(decode_signed_body(&gzipped).unwrap(), value);
        assert!(decode_signed_body(b"not json").is_err());
    }

    #[test]
    fn parses_track_payload_and_rates() {
        // Upstream song.rs:218-242 inline vector, with `TSID` spelling.
        let item = json!({
            "TSID": "T100",
            "title": "晴天",
            "albumTitle": "叶惠美",
            "albumAssetCode": "A1",
            "pic": "https://cdn/cover.jpg",
            "duration": 269,
            "artist": [{ "name": "周杰伦" }],
            "rateFileInfo": {
                "320": { "size": 10752000 },
                "128": { "size": 4300800 }
            }
        });
        let track = parse_track(&item).unwrap();
        assert_eq!(track.id, TrackId::new(ID, "T100"));
        assert_eq!(track.artists, vec!["周杰伦".to_string()]);
        assert_eq!(track.album.as_deref(), Some("叶惠美"));
        assert_eq!(track.album_id.as_deref(), Some("A1"));
        assert_eq!(track.duration.map(|d| d.as_secs()), Some(269));
        assert_eq!(track.artwork_url.as_deref(), Some("https://cdn/cover.jpg"));
        assert_eq!(track.extra.get("tsid").map(String::as_str), Some("T100"));
        assert_eq!(
            track.extra.get("album_asset_code").map(String::as_str),
            Some("A1")
        );
        assert_eq!(
            track.available_qualities,
            vec![StreamQuality::Low, StreamQuality::High]
        );
        assert_eq!(track.audio, None);
    }

    #[test]
    fn rejects_tracks_without_an_id_or_title_and_lowercase_tsid_parses() {
        assert!(parse_track(&json!({ "title": "无 ID" })).is_none());
        assert!(parse_track(&json!({ "TSID": "T1" })).is_none());
        assert!(parse_track(&json!({ "TSID": "T1", "title": "  " })).is_none());
        // Lowercase `tsid` spelling (upstream song.rs:110 field probe).
        let track = parse_track(&json!({ "tsid": "T2", "title": "歌" })).unwrap();
        assert_eq!(track.id.id, "T2");
        // Zero duration stays None.
        let track = parse_track(&json!({ "TSID": "T3", "title": "歌", "duration": 0 })).unwrap();
        assert_eq!(track.duration, None);
    }

    #[test]
    fn asset_id_fallback_serves_the_live_album_shape() {
        // /v1/album/info rows carry only `assetId` (live 2026-11-19); the
        // upstream probe would drop them — documented deviation.
        let track = parse_track(&json!({
            "assetId": "T10059185827",
            "title": "逆流而上",
            "duration": 236,
            "rateFileInfo": { "3000": { "size": 48886890 } }
        }))
        .expect("assetId row must parse");
        assert_eq!(track.id.id, "T10059185827");
        assert_eq!(track.available_qualities, vec![StreamQuality::Lossless]);
    }

    #[test]
    fn zero_size_rate_tiers_are_dropped() {
        let track = parse_track(&json!({
            "TSID": "T4",
            "title": "歌",
            "rateFileInfo": {
                "3000": { "size": 0 },
                "320": { "size": 1 },
                "128": { "size": 2 },
                "64": { "size": 0 }
            }
        }))
        .unwrap();
        assert_eq!(
            track.available_qualities,
            vec![StreamQuality::Low, StreamQuality::High]
        );
    }

    #[test]
    fn fixture_search_songs_parses_live_items() {
        let value = fixture!("search_songs.json");
        let items: Vec<Track> = value["data"]["typeTrack"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_track)
            .collect();
        assert_eq!(items.len(), 2, "two live items kept verbatim");
        let first = &items[0];
        assert_eq!(first.id.id, "T10065400429");
        assert_eq!(first.title, "等晴天");
        assert_eq!(first.artists, vec!["周深".to_string()]);
        assert_eq!(first.album.as_deref(), Some("等晴天"));
        assert_eq!(first.duration.map(|d| d.as_secs()), Some(268));
        assert_eq!(
            first.available_qualities,
            vec![
                StreamQuality::Low,
                StreamQuality::High,
                StreamQuality::Lossless
            ]
        );
        assert_eq!(
            first.extra.get("album_asset_code").map(String::as_str),
            Some("P10004348672")
        );
    }

    #[test]
    fn fixture_tracklist_category_parses_groups() {
        let value = fixture!("tracklist_category.json");
        // The gate passes on the captured envelope.
        tracklist_gate(&value, "千千音乐歌单分类请求失败").unwrap();
        let groups = value["data"].as_array().unwrap();
        assert_eq!(groups.len(), 5, "five live groups");
        let sub_total: usize = groups
            .iter()
            .map(|group| group["subCate"].as_array().map(Vec::len).unwrap_or(0))
            .sum();
        assert!(
            sub_total > 10,
            "live directory carries {sub_total} categories"
        );
        let first_sub = &groups[0]["subCate"][0];
        assert_eq!(first_sub["categoryName"].as_str(), Some("华语"));
        assert_eq!(value_string(&first_sub["id"]), "2368");
    }

    #[test]
    fn fixture_tracklist_list_rows_backfill_tags() {
        let value = fixture!("tracklist_list.json");
        tracklist_gate(&value, "千千音乐分类歌单请求失败").unwrap();
        let rows: Vec<Playlist> = value["data"]["result"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| {
                let mut playlist = parse_playlist(item)?;
                if playlist.description.is_none()
                    && let Some(tags) = item["tagList"].as_array()
                {
                    let joined = tags
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("、");
                    playlist.description = (!joined.is_empty()).then_some(joined);
                }
                playlist
                    .extra
                    .insert("category_id".to_string(), "2368".to_string());
                Some(playlist)
            })
            .collect();
        assert_eq!(rows.len(), 2);
        let first = &rows[0];
        assert_eq!(first.id.id, "309558");
        assert_eq!(first.title, "怀旧经典 | 华语至醇金曲");
        // Live desc is an empty string, so the joined tagList wins.
        assert_eq!(
            first.description.as_deref(),
            Some("流行、经典、国语、国语流行")
        );
        assert_eq!(first.track_count, Some(26));
        assert_eq!(
            first.link.as_deref(),
            Some("https://music.91q.com/songlist/309558")
        );
        assert_eq!(
            first.extra.get("category_id").map(String::as_str),
            Some("2368")
        );
        assert_eq!(first.id.provider, "qianqian");
    }

    #[test]
    fn fixture_tracklist_info_parses_live_detail() {
        let value = fixture!("tracklist_info.json");
        let data = &value["data"];
        let tracks: Vec<Track> = data["trackList"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_track)
            .collect();
        assert_eq!(tracks.len(), 3, "three live tracks kept verbatim");
        let first = &tracks[0];
        assert_eq!(first.title, "凤凰花开的路口");
        assert_eq!(
            first.available_qualities,
            vec![
                StreamQuality::Low,
                StreamQuality::High,
                StreamQuality::Lossless
            ]
        );
        // M3 slots on the derived metadata: creator keys are absent live,
        // trackCount comes from the detail block.
        let creator = ["creator", "author", "userName", "nickName", "ownerName"]
            .iter()
            .find_map(|key| data[*key].as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        assert_eq!(creator, None, "live detail carries no creator key");
        assert_eq!(value_u64(&data["trackCount"]), Some(26));
        assert_eq!(data["title"].as_str(), Some("怀旧经典 | 华语至醇金曲"));
        assert_eq!(data["desc"].as_str(), Some(""));
        // The empty live desc filters out, so the derived description is None.
        let description = data["desc"]
            .as_str()
            .or_else(|| data["description"].as_str())
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        assert_eq!(description, None);
    }

    #[test]
    fn fixture_song_info_extractes_lyric_url_and_track() {
        let value = fixture!("song_info.json");
        let lyric_url = lyric_url_of(&value).expect("live lyric URL present");
        assert!(
            lyric_url.starts_with("https://static-qianqian.taihe.com/"),
            "got {lyric_url}"
        );
        assert!(lyric_url.ends_with(".lrc"));
        // Upstream lyric.rs:58-68 inline vector (first item wins).
        let upstream = json!({ "data": [{ "lyric": "https://cdn/lyric.lrc" }] });
        assert_eq!(
            lyric_url_of(&upstream).as_deref(),
            Some("https://cdn/lyric.lrc")
        );
        let track = value["data"]
            .as_array()
            .unwrap()
            .first()
            .and_then(parse_track)
            .expect("live song info item parses");
        assert_eq!(track.id.id, "T10065400429");
        assert_eq!(track.title, "等晴天");
        assert_eq!(track.album.as_deref(), Some("等晴天"));
        // Upstream fetch_song left album_id empty through the field(…, "")
        // quirk; the shared parser reads the real albumAssetCode.
        assert_eq!(track.album_id.as_deref(), Some("P10004348672"));
    }

    #[test]
    fn fixture_tracklink_free_resolves_stream_fields() {
        let value = fixture!("tracklink_free_128.json");
        let data = &value["data"];
        let path = data["path"].as_str().unwrap_or_default();
        assert!(
            path.starts_with("https://audio04.dmhmusic.com/"),
            "live pre-signed CDN URL: {path}"
        );
        assert!(path.contains("xcode="));
        let track = Track {
            id: TrackId::new(ID, "T10063944833"),
            title: "晴れの日(晴天)".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: Some(Duration::from_secs(268)),
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: BTreeMap::new(),
        };
        let stream = stream_info_from_data(path, "128", data, &track);
        assert_eq!(stream.quality, StreamQuality::Low);
        assert_eq!(stream.format, AudioFormat::Mp3);
        assert_eq!(stream.duration.map(|d| d.as_secs()), Some(268));
        // M3 backfill from the live block: size, filemd5, expireTime.
        assert_eq!(stream.content_length, Some(4794100));
        assert_eq!(
            stream.checksum.as_deref(),
            Some("05450caaef4a160e7042082e77196599")
        );
        assert_eq!(stream.checksum_algorithm, None, "filemd5 is MD5");
        assert!(stream.md5_checksum().is_some());
        assert!(stream.expires_at_unix.is_some(), "live expireTime present");
        assert!(stream.size_is_advisory, "upstream advisory contract");
        assert!(stream.fallback_urls.is_empty());
        assert!(stream.fallback_endpoints.is_empty());
        assert!(stream.headers.is_empty());
        // Rate 3000 maps to Lossless with the flac format.
        let stream = stream_info_from_data(path, "3000", data, &track);
        assert_eq!(stream.quality, StreamQuality::Lossless);
    }

    #[test]
    fn fixture_tracklink_vip_answers_pathless_metadata() {
        let value = fixture!("tracklink_vip_nopath.json");
        assert_eq!(value["state"], Value::Bool(true));
        assert_eq!(value["errno"], 22000);
        let data = &value["data"];
        assert!(
            data["path"].as_str().is_none(),
            "VIP shape carries no path key at all"
        );
        assert!(data["trail_audio_info"].as_str().is_none());
        // The resolver's ladder treats the pathless shape as a skip and the
        // whole request exhausts into NotFound (upstream parity).
        let path = data["path"].as_str().unwrap_or_default();
        assert!(path.is_empty());
    }

    #[test]
    fn fixture_playlist_search_refusal_parses_to_empty() {
        let value = fixture!("search_playlist_refused.json");
        assert_eq!(value["state"], Value::Bool(false));
        assert_eq!(value["errno"], 23001);
        assert_eq!(value["errmsg"].as_str(), Some("不被支持的搜索类型(10000)"));
        // Upstream parity (song.rs:99, playlist.rs:42): `data.typeSonglist`
        // on a `data: []` refusal body is an empty array — an empty result,
        // never an error.
        let playlists: Vec<Playlist> = value["data"]["typeSonglist"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(parse_playlist)
            .collect();
        assert!(playlists.is_empty());
    }

    #[test]
    fn fixture_album_info_parses_metadata_and_asset_id_tracks() {
        let value = fixture!("album_info.json");
        let data = &value["data"];
        let tracks: Vec<Track> = data["trackList"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_track)
            .collect();
        assert_eq!(
            tracks.len(),
            1,
            "the assetId-only row parses (deviation pin)"
        );
        assert_eq!(tracks[0].id.id, "T10059185827");
        assert_eq!(tracks[0].title, "逆流而上");
        let album = crate::models::Album {
            id: TrackId::new(ID, "P10003660422"),
            title: data["title"].as_str().unwrap_or_default().to_string(),
            artists: artist_names(&data["artist"]),
            artwork_url: data["pic"]
                .as_str()
                .filter(|url| !url.is_empty())
                .map(str::to_string),
            track_count: Some(tracks.len() as u32),
        };
        assert_eq!(album.title, "逆流而上");
        assert_eq!(album.artists.len(), 7, "live artist array");
        assert_eq!(album.artists[0], "叶勇威");
        assert!(
            album
                .artwork_url
                .as_deref()
                .unwrap()
                .starts_with("https://img01.dmhmusic.com/")
        );
        assert_eq!(album.track_count, Some(1));
        // `introduce` is live non-empty but the Album model has no slot for it
        // (upstream kept it on its Playlist-shaped album; dropped here).
        assert!(!data["introduce"].as_str().unwrap_or_default().is_empty());
    }

    #[test]
    fn fixture_link_targets_follow_upstream_router() {
        for case in fixture!("links.json")["cases"].as_array().unwrap() {
            let source = case["source"].as_str().unwrap();
            let target = qq_link_target(case["link"].as_str().unwrap());
            match case["kind"].as_str() {
                Some(kind) => {
                    let (got_kind, got_id) = target.expect("link must resolve");
                    assert_eq!(got_kind, kind, "{source}");
                    assert_eq!(got_id, case["id"].as_str().unwrap(), "{source}");
                }
                None => {
                    assert!(target.is_none(), "must reject: {source}");
                }
            }
        }
    }

    #[test]
    fn parses_searched_playlists_and_rejects_incomplete() {
        // Upstream playlist.rs:188-206 inline vector.
        let item = json!({
            "id": 12345,
            "title": "华语流行",
            "pic": "https://cdn/p.jpg",
            "trackCount": 42,
            "playCount": 777,
            "tag": "流行"
        });
        let playlist = parse_playlist(&item).unwrap();
        assert_eq!(playlist.id.id, "12345");
        assert_eq!(playlist.title, "华语流行");
        assert_eq!(playlist.track_count, Some(42));
        assert_eq!(playlist.play_count, Some(777), "M3 play_count producer");
        assert_eq!(playlist.description.as_deref(), Some("流行"));
        assert_eq!(
            playlist.link.as_deref(),
            Some("https://music.91q.com/songlist/12345")
        );
        assert!(parse_playlist(&json!({ "title": "无 ID" })).is_none());
        assert!(parse_playlist(&json!({ "id": "1" })).is_none());
    }

    #[test]
    fn asset_code_is_sanitized() {
        // Upstream album.rs:162-169 inline vector.
        assert_eq!(normalize_asset_code(" A-100 "), "A100");
        assert_eq!(normalize_asset_code(""), "");
        let numeric = normalize_asset_code("123456");
        assert!(numeric.chars().all(|character| character.is_ascii_digit()));
    }

    #[test]
    fn rate_ladder_starts_at_the_requested_quality() {
        // Upstream url.rs:94-108 inline vectors.
        assert_eq!(rate_ladder(StreamQuality::Lossless)[0], "3000");
        assert_eq!(rate_ladder(StreamQuality::HiRes)[0], "3000");
        assert_eq!(rate_ladder(StreamQuality::High)[0], "320");
        assert_eq!(rate_ladder(StreamQuality::Low)[0], "128");
        assert!(!rate_ladder(StreamQuality::Low).contains(&"3000"));
        // Standard/Unknown never try above the low tier.
        assert_eq!(rate_ladder(StreamQuality::Standard), &["128", "64"]);
        assert_eq!(rate_ladder(StreamQuality::Unknown), &["128", "64"]);
        assert_eq!(quality_for_rate("3000"), StreamQuality::Lossless);
        assert_eq!(quality_for_rate("320"), StreamQuality::High);
        assert_eq!(quality_for_rate("64"), StreamQuality::Low);
    }

    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = QianqianProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.lyrics);
        assert!(descriptor.capabilities.playlists);
        assert!(descriptor.capabilities.albums);
        assert!(descriptor.capabilities.link_resolve);
        // voicefox declares `artist: true` (qianqian/mod.rs:54) with no artist
        // function anywhere in source/src/qianqian/ — the phantom-flag class
        // of docs/integration/PROVIDER_PARITY_MATRIX.md §6.2; honestly false.
        assert!(!descriptor.capabilities.artists);
        assert!(!descriptor.capabilities.charts);
        assert!(!descriptor.capabilities.authentication);
        assert!(registry.search_provider(ID).is_some());
        assert!(registry.stream_provider(ID).is_some());
        assert!(registry.lyrics_provider(ID).is_some());
        assert!(registry.collection_provider(ID).is_some());
        assert!(registry.playlist_provider(ID).is_some());
        assert!(registry.album_provider(ID).is_some());
        assert!(registry.link_provider(ID).is_some());
        assert!(registry.authentication_provider(ID).is_none());
        assert!(registry.artist_provider(ID).is_none());
        assert!(registry.chart_provider(ID).is_none());
        // The namespace table agrees with the module id.
        assert_eq!(
            crate::models::provider_to_voicefox_source(ID),
            Some("qianqian")
        );
    }

    // ── unified transport tests (mock transport, no network) ──

    async fn provider_with_script(
        responses: Vec<Result<TransportResponse, TransportError>>,
    ) -> (QianqianProvider, Arc<MockTransport>) {
        let mock = Arc::new(MockTransport::script(responses));
        let provider = QianqianProvider::with_transport(Http::from_shared_mock(mock.clone()));
        (provider, mock)
    }

    #[tokio::test]
    async fn signed_get_rides_the_web_identity_and_the_query_carries_sign() {
        let (provider, mock) = provider_with_script(vec![Ok(mock_json(
            200,
            r#"{"state":true,"errno":22000,"data":{"typeTrack":[]}}"#,
        ))])
        .await;
        let page = provider
            .search(&SearchQuery::new("晴天"))
            .await
            .expect("empty page parses");
        assert!(page.items.is_empty());
        assert_eq!(page.total, Some(0));
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        let request = &recorded[0];
        assert_eq!(request.method, HttpMethod::Get);
        assert!(request.idempotent);
        assert!(
            request.url.starts_with("https://music.91q.com/v1/search?"),
            "{}",
            request.url
        );
        assert!(
            request.url.contains("appid=16073360")
                && request.url.contains("type=1")
                && request.url.contains("pageNo=1")
                && request.url.contains("pageSize=30")
                && request.url.contains("%E6%99%B4%E5%A4%A9")
                && request.url.contains("&sign="),
            "the signed query must carry appid/type/encoded word/sign: {}",
            request.url
        );
        for (name, value) in [("user-agent", UA_WEB), ("referer", REFERER)] {
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(key, sent)| key == name && sent == value),
                "header {name} must ride the request"
            );
        }
    }

    #[tokio::test]
    async fn resolve_walks_the_ladder_and_backfills_stream_fields() {
        // High starts at 320: the first send hits the VIP-style pathless
        // shape (skip), the second (rate 128) carries the free-track live
        // shape with path + size + md5 + expireTime + format.
        let vip = fixture!("tracklink_vip_nopath.json").to_string();
        let free = fixture!("tracklink_free_128.json").to_string();
        let (provider, mock) =
            provider_with_script(vec![Ok(mock_json(200, &vip)), Ok(mock_json(200, &free))]).await;
        let track = Track {
            id: TrackId::new(ID, "T10063944833"),
            title: "晴れの日(晴天)".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: Some(Duration::from_secs(268)),
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: [("tsid".to_string(), "T10063944833".to_string())]
                .into_iter()
                .collect(),
        };
        let stream = provider
            .resolve(&track, &ResolveOptions::default())
            .await
            .expect("free track resolves after the pathless skip");
        assert_eq!(
            stream.quality,
            StreamQuality::Low,
            "320 skipped, 128 served"
        );
        assert_eq!(stream.format, AudioFormat::Mp3);
        assert_eq!(stream.duration.map(|d| d.as_secs()), Some(268));
        assert_eq!(stream.content_length, Some(4794100));
        assert_eq!(
            stream.checksum.as_deref(),
            Some("05450caaef4a160e7042082e77196599")
        );
        assert!(stream.expires_at_unix.is_some());
        assert!(stream.size_is_advisory);
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2, "one skipped rate + one served rate");
        assert!(recorded[0].url.contains("rate=320"), "{}", recorded[0].url);
        assert!(recorded[1].url.contains("rate=128"), "{}", recorded[1].url);
    }

    #[tokio::test]
    async fn resolve_vip_pathless_shape_exhausts_into_not_found() {
        let vip = fixture!("tracklink_vip_nopath.json").to_string();
        let (provider, mock) = provider_with_script(vec![
            Ok(mock_json(200, &vip)),
            Ok(mock_json(200, &vip)),
            Ok(mock_json(200, &vip)),
            Ok(mock_json(200, &vip)),
        ])
        .await;
        let track = Track {
            id: TrackId::new(ID, "T10065400429"),
            title: "等晴天".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: BTreeMap::new(),
        };
        let error = provider
            .resolve(
                &track,
                &ResolveOptions {
                    quality: StreamQuality::Lossless,
                },
            )
            .await
            .expect_err("pathless VIP metadata must exhaust the ladder");
        assert!(matches!(error, Error::NotFound), "got {error:?}");
        let recorded = mock.recorded();
        assert_eq!(
            recorded.len(),
            4,
            "Lossless walks all four rates (3000/320/128/64)"
        );
        for (rate, request) in ["3000", "320", "128", "64"].iter().zip(recorded.iter()) {
            assert!(
                request.url.contains(&format!("rate={rate}")),
                "expected rate={rate} in {}",
                request.url
            );
        }
    }

    #[tokio::test]
    async fn lyrics_fetches_the_song_info_url_then_the_lrc_file() {
        let info = fixture!("song_info.json").to_string();
        let (provider, mock) = provider_with_script(vec![
            Ok(mock_json(200, &info)),
            Ok(mock_bytes(
                200,
                b"[00:01.00]test line\r\n[00:02.00]second\r\n".to_vec(),
            )),
        ])
        .await;
        let track = Track {
            id: TrackId::new(ID, "T10065400429"),
            title: "等晴天".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: BTreeMap::new(),
        };
        let doc = provider
            .lyrics(&track)
            .await
            .expect("lyrics resolve")
            .expect("lyric present");
        assert_eq!(doc.format, LyricFormat::Lrc);
        assert_eq!(doc.lines.len(), 2);
        assert!(doc.original.starts_with("[00:01.00]"));
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2, "song info + static lrc fetch");
        assert!(
            recorded[0]
                .url
                .starts_with("https://music.91q.com/v1/song/info?"),
            "{}",
            recorded[0].url
        );
        assert!(
            recorded[1]
                .url
                .starts_with("https://static-qianqian.taihe.com/"),
            "{}",
            recorded[1].url
        );
    }

    #[tokio::test]
    async fn lyrics_without_a_lyric_url_answer_not_found() {
        let (provider, mock) = provider_with_script(vec![Ok(mock_json(
            200,
            r#"{"state":true,"errno":22000,"data":[{"TSID":"T1","title":"歌","lyric":""}]}"#,
        ))])
        .await;
        let track = Track {
            id: TrackId::new(ID, "T1"),
            title: "歌".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: BTreeMap::new(),
        };
        let error = provider.lyrics(&track).await.expect_err("empty lyric URL");
        assert!(matches!(error, Error::NotFound), "got {error:?}");
        assert_eq!(mock.recorded().len(), 1, "no second fetch after empty URL");
    }

    #[tokio::test]
    async fn categories_gate_maps_a_refusal_to_unavailable() {
        let (provider, mock) = provider_with_script(vec![Ok(mock_json(
            200,
            r#"{"state":false,"errno":23001,"errmsg":"x","data":[]}"#,
        ))])
        .await;
        let error = provider.categories().await.expect_err("gate refuses");
        assert!(matches!(error, Error::Unavailable { .. }), "got {error:?}");
        assert!(error.provider_id() == Some(ID));
        assert!(error.to_string().contains("千千音乐歌单分类请求失败"));
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn foreign_track_ids_are_rejected_before_any_request() {
        let (provider, mock) = provider_with_script(vec![]).await;
        let foreign = Track {
            id: TrackId::new("netease", "1"),
            title: "x".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: BTreeMap::new(),
        };
        for error in [
            provider
                .resolve(&foreign, &ResolveOptions::default())
                .await
                .unwrap_err(),
            provider.lyrics(&foreign).await.unwrap_err(),
        ] {
            assert!(matches!(error, Error::InvalidInput(_)), "got {error:?}");
        }
        assert!(mock.recorded().is_empty(), "no request may leave");
    }

    #[tokio::test]
    async fn unrecognized_link_answers_invalid_input() {
        let (provider, mock) = provider_with_script(vec![]).await;
        let error = provider
            .resolve_link("https://music.91q.com/player")
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidInput(_)), "got {error:?}");
        assert!(mock.recorded().is_empty());
    }

    #[tokio::test]
    async fn search_refusal_body_parses_to_an_empty_page() {
        let (provider, mock) = provider_with_script(vec![Ok(mock_json(
            200,
            r#"{"state":false,"errno":23001,"errmsg":"不被支持的搜索类型(10000)","data":[]}"#,
        ))])
        .await;
        let page = provider
            .search(&SearchQuery::new("晴天"))
            .await
            .expect("refusal body is an empty page (upstream parity)");
        assert!(page.items.is_empty());
        assert_eq!(page.total, Some(0));
        assert!(!page.has_more);
        assert_eq!(mock.recorded().len(), 1);
    }
}
