//! JOOX (腾讯系海外平台) provider migrated from VoiceFox's `lx_source::joox`
//! (source/src/joox/: mod.rs, api.rs — the two-file port). Unsigned surfaces:
//! OpenJOOX v2 `search_type` (search + playlist sections), OpenJOOX v3
//! `playlist` (playlist/album containers), and the JSONP pair
//! `web_get_songinfo` (song detail + kbps stream ladder) / `web_lyric`.
//!
//! Migration basis: the 2026-11-17 viability probe had found the anonymous
//! surface closed (docs/PROVIDER_EVALUATION.md:23-27), and the 2026-11-19
//! anonymous re-probe performed for this slice found the service **partially
//! revived**. Live facts recorded then (all anonymous read-only GETs, ≤3 per
//! endpoint, no credentials):
//!
//! - **search is alive**: `/openjoox/v2/search_type?type=0` answers HTTP 200
//!   `error_code 0` with 30 `tracks` entries for 晴天, each entry the
//!   array-wrapped shape upstream handles (`[[{song_info}]]`); free tracks
//!   (`vip_flag 0, is_playable true`) exist alongside VIP ones. **No
//!   `section_list`** rides the live response, so the upstream playlist
//!   extraction from search parses to an empty result (upstream parity, never
//!   an error). Only `type=0` is accepted (`type=4` answers the 2026-11-17
//!   `9007001 parameters type error` — recorded, not retried);
//! - **lyrics are alive with a server-side change**: `web_lyric` answers the
//!   JSONP `MusicJsonCallback({code:0, lyric: ...})`, but the `lyric` field is
//!   **base64-encoded LRC** (69 lines decode for the probe track). Upstream
//!   reads the field as plain text (api.rs:350-354, the historical shape);
//!   musid-dli decodes base64-alphabet payloads and passes anything else
//!   through unchanged (upstream parity, `lyric_live.json`);
//! - **stream/track-detail are dead anonymously**: `web_get_songinfo` answers
//!   HTTP 404 with an empty body (live-pinned; the 2026-11-17 record had
//!   empty-body answers). The kbps ladder is ported at upstream parity and
//!   surfaces the refusal as [`Error::Network`]; it works against the
//!   documented shape (`songinfo_stream.txt`, schema-derived — no live capture
//!   exists) should the endpoint revive;
//! - **playlist/album containers are dead anonymously**:
//!   `/openjoox/v3/playlist` answers nginx 404 on the upstream cache host and
//!   403 on the api host (live-pinned, `playlist_dead_404.html`);
//!   `/openjoox/v2/playlist` answers nginx 404 too. The container faces are
//!   ported at upstream parity and surface the refusal as [`Error::Network`];
//! - `JOOX_COOKIE` (upstream api.rs:101-117, an env-var read) maps onto the
//!   musid-dli session architecture: cookies stored in the [`SessionStore`]
//!   under the `joox` provider id ride the OpenJOOX requests exactly like the
//!   upstream Cookie header did (the JSONP endpoints carried no cookie
//!   upstream and still do not).
//!
//! Capability contract: upstream declares playlists, playlist_search, album,
//! link_parse (joox/mod.rs:103-110); musid-dli adds the always-implemented
//! search/stream/lyrics faces. All six ported faces are live-pinned to their
//! current anonymous status in docs/integration/PROVIDER_PARITY_MATRIX.md §5;
//! artists/charts/authentication have no upstream face and stay false.

use crate::{
    Error, ErrorDetail, Result,
    lrc::parse_lrc,
    models::{
        AudioFormat, LyricDocument, LyricFormat, Playlist, StreamInfo, StreamQuality, Track,
        TrackId,
    },
    provider::{
        AlbumProvider, CollectionProvider, LinkResolver, LyricsProvider, MusicProvider,
        PlaylistProvider, ProviderCapabilities, ResolveOptions, ResolvedLink, SearchPage,
        SearchQuery, StreamResolver, TrackSearch,
    },
    session::SessionStore,
    transport::{ClientProfile, Http, RetryPolicy, TransportError, TransportRequest},
};
use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

const ID: &str = "joox";
const NAME: &str = "JOOX";
const UA_WEB: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
/// Fixed egress marker upstream sends on every JOOX request to bypass the
/// region gate (upstream api.rs:19). JOOX is region-sensitive: without it the
/// endpoints answer empty results (upstream api.rs:7-8).
const X_FORWARDED_FOR: &str = "203.0.113.1";

const OPENJOOX_SEARCH: &str = "https://cache.api.joox.com/openjoox/v2/search_type";
const OPENJOOX_PLAYLIST: &str = "https://cache.api.joox.com/openjoox/v3/playlist";
const SONG_INFO_API: &str = "https://api.joox.com/web-fcgi-bin/web_get_songinfo";
const LYRIC_API: &str = "https://api.joox.com/web-fcgi-bin/web_lyric";

#[derive(Clone)]
pub struct JooxProvider {
    http: Http,
    sessions: SessionStore,
}
impl std::fmt::Debug for JooxProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JooxProvider")
            .field("session_store", &"redacted")
            .finish()
    }
}
impl Default for JooxProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl JooxProvider {
    pub fn new() -> Self {
        Self {
            http: Http::from_profile(&ClientProfile::provider_default()),
            sessions: SessionStore::default(),
        }
    }
    /// Test seam: inject a transport handle (a mock in unit tests). The
    /// production constructor is [`JooxProvider::new`]. Test-only so
    /// non-test builds see no dead constructor.
    #[cfg(test)]
    pub(crate) fn with_transport(http: Http) -> Self {
        Self {
            http,
            sessions: SessionStore::default(),
        }
    }
    /// Cookie injection path (the musid-dli form of upstream's `JOOX_COOKIE`
    /// env var, api.rs:101-117): a session stored under the `joox` provider id
    /// contributes its cookies to the OpenJOOX request headers.
    pub fn with_session_store(mut self, sessions: SessionStore) -> Self {
        self.sessions = sessions;
        self
    }
    pub fn session_store(&self) -> SessionStore {
        self.sessions.clone()
    }
    /// Register all currently implemented joox capabilities in one step. No
    /// authentication slot exists upstream (the cookie injection is a session
    /// concern, not a login flow), so none is registered; `artists`/`charts`
    /// have no upstream face either (see [`JooxProvider::capabilities`]).
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
    /// OpenJOOX identity headers (upstream `apply_joox_headers` api.rs:108-117):
    /// UA + Origin + Referer, the optional session cookie, then the region
    /// marker on top.
    fn openjoox_request(&self, request: TransportRequest) -> TransportRequest {
        let request = request
            .header("user-agent", UA_WEB)
            .header("origin", "http://www.joox.com")
            .header("referer", "http://www.joox.com");
        let cookie = self
            .sessions
            .get(ID)
            .map(|session| {
                session
                    .cookies
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .filter(|value| !value.is_empty());
        match cookie {
            Some(cookie) => request.header("cookie", cookie),
            None => request,
        }
        .header("x-forwarded-for", X_FORWARDED_FOR)
    }
    /// JSONP-endpoint identity headers (upstream fetch_song_detail
    /// api.rs:270-273 and fetch_lyric api.rs:338-341): UA + the region marker
    /// only — no Origin/Referer/cookie upstream.
    fn jsonp_request(&self, request: TransportRequest) -> TransportRequest {
        request
            .header("user-agent", UA_WEB)
            .header("x-forwarded-for", X_FORWARDED_FOR)
    }
    /// GET through the unified transport layer, mapping every transport
    /// failure onto [`Error::Network`] with the provider id (the dead-endpoint
    /// surfaces — songinfo 404, v3 playlist nginx 404 — arrive here).
    async fn send_get(&self, request: TransportRequest) -> Result<Vec<u8>> {
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
        Ok(response.body)
    }
    async fn get_json(&self, request: TransportRequest) -> Result<Value> {
        let body = self.send_get(request).await?;
        serde_json::from_slice(&body).map_err(|error| Error::InvalidResponse {
            provider: Some(ID.into()),
            code: None,
            detail: Some(ErrorDetail::new(format!("parse failed: {error}"))),
        })
    }
    async fn get_text(&self, request: TransportRequest) -> Result<String> {
        let body = self.send_get(request).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
    /// OpenJOOX GET with the fixed `country=sg&lang=zh_cn` prefix (upstream
    /// `openjoox_get` api.rs:119-142; upstream builds the query by hand so the
    /// key order is `country, lang, ...params`).
    async fn openjoox_get(&self, api: &str, params: &[(&str, &str)]) -> Result<Value> {
        let mut query = vec![("country", "sg"), ("lang", "zh_cn")];
        query.extend_from_slice(params);
        let url = format!(
            "{api}?{}",
            query
                .iter()
                .map(|(key, value)| format!(
                    "{}={}",
                    urlencoding::encode(key),
                    urlencoding::encode(value)
                ))
                .collect::<Vec<_>>()
                .join("&")
        );
        self.get_json(self.openjoox_request(TransportRequest::get(url)))
            .await
    }
    /// Search envelope: songs, playlists and albums are all extracted from
    /// this one `type=0` call (upstream api.rs:144-147).
    async fn search_raw(&self, keyword: &str) -> Result<Value> {
        self.openjoox_get(OPENJOOX_SEARCH, &[("key", keyword), ("type", "0")])
            .await
    }
    /// Playlist/album container (upstream `playlist_songs` api.rs:171-173;
    /// albums are special playlists upstream, mod.rs:198-213). Live status
    /// 2026-11-19: nginx 404 — surfaces as [`Error::Network`].
    async fn playlist_songs(&self, id: &str) -> Result<Value> {
        self.openjoox_get(OPENJOOX_PLAYLIST, &[("id", id)]).await
    }
    /// Song detail + kbps stream ladder (upstream `fetch_song_detail`
    /// api.rs:261-330). Live status 2026-11-19: HTTP 404 empty body — surfaces
    /// as [`Error::Network`].
    async fn fetch_song_detail(&self, song_id: &str) -> Result<SongDetail> {
        let url = format!(
            "{SONG_INFO_API}?songid={}&country=hk&lang=zh_TW&from_type=-1&channel_id=-1&_={}",
            urlencoding::encode(song_id),
            unix_millis()
        );
        let body = self
            .get_text(self.jsonp_request(TransportRequest::get(url)))
            .await?;
        let json: Value =
            serde_json::from_str(strip_jsonp(&body, "MusicInfoCallback")).map_err(|error| {
                Error::InvalidResponse {
                    provider: Some(ID.into()),
                    code: None,
                    detail: Some(ErrorDetail::new(format!("parse failed: {error}"))),
                }
            })?;
        parse_song_detail(song_id, &json)
    }
    /// Lyric (upstream `fetch_lyric` api.rs:333-355) with the live base64
    /// handling documented on [`decode_lyric_field`].
    async fn fetch_lyric(&self, song_id: &str) -> Result<Option<String>> {
        let url = format!(
            "{LYRIC_API}?musicid={}&country=sg&lang=zh_cn",
            urlencoding::encode(song_id)
        );
        let body = self
            .get_text(self.jsonp_request(TransportRequest::get(url)))
            .await?;
        let json: Value =
            serde_json::from_str(strip_jsonp(&body, "MusicJsonCallback")).map_err(|error| {
                Error::InvalidResponse {
                    provider: Some(ID.into()),
                    code: None,
                    detail: Some(ErrorDetail::new(format!("parse failed: {error}"))),
                }
            })?;
        Ok(json["lyric"]
            .as_str()
            .map(decode_lyric_field)
            .map(|lyric| lyric.trim().to_string())
            .filter(|lyric| !lyric.is_empty()))
    }
    /// Shared playlist/album container assembly (upstream `get_playlist_detail`
    /// mod.rs:186-195 and the parse_link playlist/album branches mod.rs:225-273).
    /// `NotFound` on an empty parsed track list (upstream uses it as the
    /// missing-container signal). The M3 `ParsedLink` songs-vector drop
    /// (MODEL_ALIGNMENT_DESIGN.md §5.2) applies: metadata-only, tracks stay
    /// reachable through `playlist_tracks`/`album_tracks`.
    async fn container(
        &self,
        id: &str,
        label: &str,
        link_of: fn(&str) -> String,
    ) -> Result<(Playlist, Vec<Track>)> {
        let json = self.playlist_songs(id).await?;
        let tracks = songs_from_sections(&json);
        if tracks.is_empty() {
            return Err(Error::NotFound);
        }
        let name = json["name"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{label} {id}"));
        let mut extra = BTreeMap::new();
        extra.insert("playlist_id".to_string(), id.to_string());
        let playlist = Playlist {
            id: TrackId::new(ID, id.to_string()),
            title: name,
            description: None,
            artwork_url: tracks.first().and_then(|track| track.artwork_url.clone()),
            track_count: Some(tracks.len() as u32),
            tracks: vec![],
            play_count: None,
            creator: tracks.first().map(|track| track.artists.join(" / ")),
            link: Some(link_of(id)),
            extra,
        };
        Ok((playlist, tracks))
    }
}

fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

/// 名称里可能带空格与特殊字符，JOOX 的 URL ID 需要还原成原样 (upstream
/// api.rs:80-88).
fn normalize_id(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    urlencoding::decode(trimmed)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| trimmed.to_string())
}

/// 从 JSONP 外壳里取出 JSON：`Callback({...})` → `{...}` (upstream
/// api.rs:91-99).
fn strip_jsonp<'a>(body: &'a str, callback: &str) -> &'a str {
    match body.find(&format!("{callback}(")) {
        Some(index) => {
            let inner = &body[index + callback.len() + 1..];
            inner.trim_end().strip_suffix(')').unwrap_or(inner).trim()
        }
        None => body.trim(),
    }
}

fn value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_u64().map(|value| value.to_string()))
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .unwrap_or_default()
}

fn value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

/// Cover preference: the 300px image first, else the first entry (upstream
/// `pick_image` api.rs:220-228).
fn pick_image(images: &Value) -> Option<String> {
    let list = images.as_array()?;
    list.iter()
        .find(|image| image["width"].as_u64() == Some(300))
        .or_else(|| list.first())
        .and_then(|image| image["url"].as_str())
        .filter(|url| !url.is_empty())
        .map(str::to_string)
}

/// Shared song parser (upstream `parse_song_info` api.rs:195-217). `None`
/// without a usable id or a non-empty title. Artist names keep one entry per
/// `artist_list` element (the musid-dli `Track.artists` slot is a vector;
/// upstream joins them with " / " into its single `singer` string). The
/// search/container responses carry no kbps data, so `available_qualities`
/// stays empty here — tiers arrive only from `fetch_song_detail`.
fn parse_song_info(info: &Value) -> Option<Track> {
    let id = normalize_id(&value_string(&info["id"]));
    let name = info["name"].as_str().unwrap_or_default().trim().to_string();
    if id.is_empty() || name.is_empty() {
        return None;
    }
    let artists: Vec<String> = info["artist_list"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|artist| artist["name"].as_str())
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
        .collect();
    let album_name = info["album_name"].as_str().unwrap_or_default().trim();
    let album_id = value_string(&info["album_id"]);
    let mut extra = BTreeMap::new();
    extra.insert("songid".to_string(), id.clone());
    Some(Track {
        id: TrackId::new(ID, id),
        title: name,
        artists,
        album: (!album_name.is_empty()).then(|| album_name.to_string()),
        album_id: (!album_id.is_empty()).then_some(album_id),
        duration: {
            let secs = value_u64(&info["play_duration"]).unwrap_or_default();
            (secs > 0).then(|| Duration::from_secs(secs))
        },
        artwork_url: pick_image(&info["images"]),
        isrc: None,
        available_qualities: vec![],
        audio: None,
        extra,
    })
}

/// v2 `search_type` 的歌曲结果位于 `tracks`，部分版本会把单项再包一层数组
/// (upstream `songs_from_search` api.rs:150-168).
fn songs_from_search(json: &Value) -> Vec<Track> {
    let mut songs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in json["tracks"].as_array().into_iter().flatten() {
        let info = if entry.is_array() {
            entry.as_array().and_then(|items| items.first())
        } else {
            Some(entry)
        };
        let Some(info) = info else { continue };
        let Some(track) = parse_song_info(info) else {
            continue;
        };
        if seen.insert(track.id.id.clone()) {
            songs.push(track);
        }
    }
    songs
}

/// 从搜索/容器结果里提取歌曲（`section_list[].item_list[].song[].song_info`，
/// upstream `songs_from_sections` api.rs:176-193).
fn songs_from_sections(json: &Value) -> Vec<Track> {
    let mut songs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for section in json["section_list"].as_array().into_iter().flatten() {
        for item in section["item_list"].as_array().into_iter().flatten() {
            for entry in item["song"].as_array().into_iter().flatten() {
                let info = &entry["song_info"];
                let Some(track) = parse_song_info(info) else {
                    continue;
                };
                if seen.insert(track.id.id.clone()) {
                    songs.push(track);
                }
            }
        }
    }
    songs
}

/// 从搜索结果里提取歌单（`item_list[].editor_playlist`，upstream
/// `playlists_from_sections` api.rs:231-252). The live search response carries
/// no `section_list` (2026-11-19), so this parses to an empty result —
/// upstream parity, never an error.
fn playlists_from_sections(json: &Value) -> Vec<Playlist> {
    let mut playlists = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for section in json["section_list"].as_array().into_iter().flatten() {
        for item in section["item_list"].as_array().into_iter().flatten() {
            let info = &item["editor_playlist"];
            let id = normalize_id(&value_string(&info["id"]));
            let name = info["name"].as_str().unwrap_or_default().trim().to_string();
            if id.is_empty() || name.is_empty() || !seen.insert(id.clone()) {
                continue;
            }
            let mut extra = BTreeMap::new();
            extra.insert("playlist_id".to_string(), id.clone());
            playlists.push(Playlist {
                id: TrackId::new(ID, id.clone()),
                title: name,
                description: None,
                artwork_url: pick_image(&info["images"]),
                track_count: {
                    let count = value_u64(&info["song_count"]).unwrap_or_default();
                    (count > 0).then_some(count as u32)
                },
                tracks: vec![],
                play_count: None,
                creator: None,
                link: Some(format!("https://www.joox.com/sg/playlist/{id}")),
                extra,
            });
        }
    }
    playlists
}

/// Song metadata + available stream URLs (upstream `SongDetail` api.rs:254-258).
/// The URL slot carries the serving field's format — `r320Url`/`r192Url`/
/// `mp3Url` are MP3 containers, `m4aUrl` is M4A — real data from the parse
/// path, filling the musid-dli `StreamInfo::format` slot upstream's `SongUrl`
/// has no equivalent for.
#[derive(Debug)]
struct SongDetail {
    song: Track,
    urls: Vec<(StreamQuality, AudioFormat, String)>,
}

/// kbps tier → quality bucket (upstream api.rs:308-313): the `320`/`192`
/// rows map to High320 (musid-dli [`StreamQuality::High`], MP3), the
/// `128`/`96` rows map to Low128 ([`StreamQuality::Low`]; `mp3Url` is an MP3
/// container, `m4aUrl` M4A).
fn quality_of_field(field: &str) -> (StreamQuality, AudioFormat) {
    match field {
        "r320Url" | "r192Url" => (StreamQuality::High, AudioFormat::Mp3),
        "mp3Url" => (StreamQuality::Low, AudioFormat::Mp3),
        _ => (StreamQuality::Low, AudioFormat::M4a),
    }
}

/// Song-detail parser, split from the fetch so fixture tests can drive it.
/// `kbps_map` arrives either as a JSON-encoded string or as an object (the
/// upstream dual branch, api.rs:303-306); a value of `"0"`/empty/0 means the
/// tier is unavailable (api.rs:314-318); the URL fields pair with
/// [`quality_of_field`].
fn parse_song_detail(song_id: &str, json: &Value) -> Result<SongDetail> {
    let name = json["msong"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return Err(perr("JOOX 单曲详情为空"));
    }
    let singer = json["msinger"].as_str().unwrap_or_default().trim();
    let album_name = json["malbum"].as_str().unwrap_or_default().trim();
    let mut song = Track {
        id: TrackId::new(ID, song_id.to_string()),
        title: name,
        artists: (!singer.is_empty())
            .then(|| singer.to_string())
            .into_iter()
            .collect(),
        album: (!album_name.is_empty()).then(|| album_name.to_string()),
        album_id: None,
        duration: {
            let secs = value_u64(&json["minterval"]).unwrap_or_default();
            (secs > 0).then(|| Duration::from_secs(secs))
        },
        artwork_url: json["img"]
            .as_str()
            .filter(|url| !url.is_empty())
            .map(str::to_string),
        isrc: None,
        available_qualities: vec![],
        audio: None,
        extra: [("songid".to_string(), song_id.to_string())]
            .into_iter()
            .collect(),
    };
    // 可用档位由 kbps_map 决定：值为空或 0 表示该档位不可用 (upstream
    // api.rs:302-325).
    let kbps: Value = json["kbps_map"]
        .as_str()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_else(|| json["kbps_map"].clone());
    let mut urls = Vec::new();
    for (key, field) in [
        ("320", "r320Url"),
        ("192", "r192Url"),
        ("128", "mp3Url"),
        ("96", "m4aUrl"),
    ] {
        let available = kbps[key]
            .as_str()
            .map(|value| value != "0" && !value.is_empty())
            .or_else(|| kbps[key].as_f64().map(|value| value > 0.0))
            .unwrap_or(true);
        if !available {
            continue;
        }
        if let Some(url) = json[field].as_str().filter(|url| !url.is_empty()) {
            let (quality, format) = quality_of_field(field);
            urls.push((quality, format, url.to_string()));
        }
    }
    let mut qualities: Vec<StreamQuality> = urls.iter().map(|(quality, _, _)| *quality).collect();
    qualities.sort();
    qualities.dedup();
    song.available_qualities = qualities;
    Ok(SongDetail { song, urls })
}

/// Tier selection (upstream mod.rs:139-147): candidates sort quality
/// descending; the first tier at or below the request wins; when every
/// candidate is above the request, the top tier is served rather than failing.
fn select_candidate(
    mut candidates: Vec<(StreamQuality, AudioFormat, String)>,
    requested: StreamQuality,
) -> Option<(StreamQuality, AudioFormat, String)> {
    candidates.sort_by_key(|(quality, _, _)| std::cmp::Reverse(*quality));
    candidates
        .iter()
        .find(|(candidate, _, _)| *candidate <= requested)
        .or_else(|| candidates.first())
        .cloned()
}

/// Lyric-field decoder. Live deviation (2026-11-19): `web_lyric` returns the
/// `lyric` field **base64-encoded** (upstream api.rs:350-354 reads it as plain
/// text — the historical shape this decoder keeps working for). A payload
/// whose bytes are all in the base64 alphabet and whose decode is valid UTF-8
/// is decoded; anything else (real plain LRC always carries `[`, `:` or
/// spaces, none of which are base64) passes through unchanged.
fn decode_lyric_field(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let is_base64 = trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='));
    if is_base64
        && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(trimmed)
        && let Ok(text) = String::from_utf8(bytes)
    {
        return text;
    }
    raw.to_string()
}

/// 链接识别：单曲 `/single/{id}`、歌单 `/playlist/{id}`、专辑 `/album/{id}`；
/// 裸 ID（分享文案里常见的 base64 风格长串）按长度 + 字符集双重判断 (upstream
/// `link_target` mod.rs:52-84). Every capture is URL-decoded through
/// [`normalize_id`] (upstream mod.rs:67).
fn link_target(link: &str) -> Option<(&'static str, String)> {
    let trimmed = link.trim();
    let patterns = [
        ("single", "song"),
        ("playlist", "playlist"),
        ("album", "album"),
    ];
    for (segment, kind) in patterns {
        let marker = format!("/{segment}/");
        if let Some(index) = trimmed.find(&marker) {
            let rest = &trimmed[index + marker.len()..];
            let id = rest
                .split(['/', '?', '#'])
                .next()
                .map(normalize_id)
                .unwrap_or_default();
            if !id.is_empty() {
                return Some((kind, id));
            }
        }
    }
    if !trimmed.contains('/')
        && trimmed.len() > 10
        && trimmed
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "+/-_=".contains(character))
    {
        return Some(("song", normalize_id(trimmed)));
    }
    None
}

fn playlist_link_of(id: &str) -> String {
    format!("https://www.joox.com/sg/playlist/{id}")
}

fn album_link_of(id: &str) -> String {
    format!("https://www.joox.com/hk/album/{id}")
}

impl MusicProvider for JooxProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        NAME
    }
    /// Upstream declares playlists, playlist_search, album, link_parse
    /// (joox/mod.rs:103-110); musid-dli adds the always-implemented
    /// search/stream/lyrics faces. Artists/charts/authentication have no
    /// upstream face (the cookie injection is a session concern, not a login
    /// flow). Live per-column status: PROVIDER_PARITY_MATRIX.md §5.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            lyrics: true,
            playlists: true,
            albums: true,
            link_resolve: true,
            ..Default::default()
        }
    }
    /// Upstream tier order Low128/High320 (mod.rs:171-173).
    fn supported_qualities(&self) -> Vec<StreamQuality> {
        vec![StreamQuality::Low, StreamQuality::High]
    }
}

#[async_trait]
impl TrackSearch for JooxProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.keyword.trim().is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        let json = self.search_raw(q.keyword.trim()).await?;
        let items = songs_from_search(&json);
        // Upstream parity: total = item count, has_more always false
        // (mod.rs:123-128) — the envelope's `has_more`/`next_page` fields are
        // never read upstream.
        let total = items.len() as u64;
        Ok(SearchPage {
            items,
            total: Some(total),
            has_more: false,
        })
    }
}

#[async_trait]
impl StreamResolver for JooxProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to joox".into()));
        }
        let song_id = track
            .extra
            .get("songid")
            .cloned()
            .unwrap_or_else(|| track.id.id.clone());
        if song_id.is_empty() {
            return Err(Error::NotFound);
        }
        let detail = self.fetch_song_detail(&song_id).await?;
        let Some((achieved, format, url)) = select_candidate(detail.urls, options.quality) else {
            return Err(perr("JOOX 未返回可用地址"));
        };
        if achieved != options.quality {
            tracing::debug!(
                "joox requested {:?} but served {:?} (song {song_id})",
                options.quality,
                achieved
            );
        }
        // song_info carries no file size/md5/expiry (upstream `song_url_from`
        // mod.rs:21-35: size None, size_is_advisory true — the download engine
        // only does single-connection streaming for this source).
        Ok(StreamInfo {
            url,
            quality: achieved,
            format,
            duration: track.duration,
            expires_at_unix: None,
            content_length: None,
            checksum: None,
            checksum_algorithm: None,
            headers: vec![],
            fallback_urls: vec![],
            fallback_endpoints: vec![],
            index_range: None,
            init_range: None,
            max_chunk_size: None,
            size_is_advisory: true,
        })
    }
}

#[async_trait]
impl LyricsProvider for JooxProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to joox".into()));
        }
        let song_id = track
            .extra
            .get("songid")
            .cloned()
            .unwrap_or_else(|| track.id.id.clone());
        if song_id.is_empty() {
            return Err(Error::NotFound);
        }
        let original = self.fetch_lyric(&song_id).await?.ok_or(Error::NotFound)?;
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
impl CollectionProvider for JooxProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist> {
        if id.trim().is_empty() {
            return Err(Error::InvalidInput("playlist id is empty".into()));
        }
        Ok(self
            .container(id.trim(), "JOOX 歌单", playlist_link_of)
            .await?
            .0)
    }
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        if id.trim().is_empty() {
            return Err(Error::InvalidInput("playlist id is empty".into()));
        }
        Ok(self
            .container(id.trim(), "JOOX 歌单", playlist_link_of)
            .await?
            .1)
    }
}

#[async_trait]
impl PlaylistProvider for JooxProvider {
    /// 歌单分类目前没有稳定的公开接口 (upstream module docs mod.rs:4) — no
    /// category face exists upstream, the slot stays Unsupported.
    async fn categories(&self) -> Result<Vec<crate::models::PlaylistCategory>> {
        Err(Error::Unsupported(ID.into()))
    }
    async fn playlists(
        &self,
        _category: &str,
        _page: u32,
        _page_size: u32,
    ) -> Result<Vec<Playlist>> {
        Err(Error::Unsupported(ID.into()))
    }
    /// Keyword playlist search rides the same `type=0` search as songs and
    /// reads `section_list[].item_list[].editor_playlist` (upstream
    /// `search_playlists` mod.rs:175-184 → api.rs:231-252). Live status
    /// 2026-11-19: the search response carries no `section_list`, so this
    /// parses to an empty result (upstream parity, never an error).
    async fn search_playlists(&self, keyword: &str, _page: u32) -> Result<Vec<Playlist>> {
        if keyword.trim().is_empty() {
            return Err(Error::InvalidInput(
                "playlist search keyword is empty".into(),
            ));
        }
        let json = self.search_raw(keyword.trim()).await?;
        Ok(playlists_from_sections(&json))
    }
    async fn user_playlists(&self, _page: u32, _page_size: u32) -> Result<Vec<Playlist>> {
        Err(Error::Unsupported(ID.into()))
    }
}

#[async_trait]
impl AlbumProvider for JooxProvider {
    /// 专辑曲目：OpenJOOX 的专辑是特殊歌单，走同一接口 (upstream
    /// `get_album_songs` mod.rs:198-213). Album metadata comes from the same
    /// call the upstream parse_link album branch builds it from (name from
    /// `json["name"]`, cover/artist from the first song, mod.rs:248-272).
    async fn album(&self, id: &str) -> Result<crate::models::Album> {
        if id.trim().is_empty() {
            return Err(Error::InvalidInput("album id is empty".into()));
        }
        let (playlist, tracks) = self
            .container(id.trim(), "JOOX 专辑", album_link_of)
            .await?;
        Ok(crate::models::Album {
            id: playlist.id,
            title: playlist.title,
            artists: tracks
                .first()
                .map(|track| track.artists.clone())
                .unwrap_or_default(),
            artwork_url: playlist.artwork_url,
            track_count: playlist.track_count,
        })
    }
    async fn album_tracks(&self, id: &str) -> Result<Vec<Track>> {
        if id.trim().is_empty() {
            return Err(Error::InvalidInput("album id is empty".into()));
        }
        Ok(self
            .container(id.trim(), "JOOX 专辑", album_link_of)
            .await?
            .1)
    }
}

#[async_trait]
impl LinkResolver for JooxProvider {
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink> {
        let (kind, id) =
            link_target(url).ok_or_else(|| Error::InvalidInput("无法识别 JOOX 链接".into()))?;
        match kind {
            "song" => Ok(ResolvedLink::Track(self.fetch_song_detail(&id).await?.song)),
            "playlist" => Ok(ResolvedLink::Playlist(
                self.container(&id, "JOOX 歌单", playlist_link_of).await?.0,
            )),
            "album" => Ok(ResolvedLink::Album(self.album(&id).await?)),
            _ => Err(Error::InvalidInput("unrecognized joox link".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{MockTransport, TransportResponse};
    use serde_json::json;
    use std::sync::Arc;

    /// Loads a fixture from tests/fixtures/joox (see the README there for the
    /// derivation: trimmed verbatim live captures from the 2026-11-19
    /// anonymous re-probe, the upstream inline vectors, and the clearly marked
    /// schema-derived songinfo vector).
    macro_rules! fixture {
        ($name:expr) => {
            include_str!(concat!("../tests/fixtures/joox/", $name))
        };
    }

    macro_rules! fixture_value {
        ($name:literal) => {
            serde_json::from_str::<Value>(fixture!($name)).expect("joox fixture must be valid JSON")
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

    async fn provider_with_script(
        responses: Vec<Result<TransportResponse, TransportError>>,
    ) -> (JooxProvider, Arc<MockTransport>) {
        let mock = Arc::new(MockTransport::script(responses));
        let provider = JooxProvider::with_transport(Http::from_shared_mock(mock.clone()));
        (provider, mock)
    }

    fn track_with_songid(song_id: &str) -> Track {
        Track {
            id: TrackId::new(ID, song_id),
            title: "晴天".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: Some(Duration::from_secs(269)),
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: [("songid".to_string(), song_id.to_string())]
                .into_iter()
                .collect(),
        }
    }

    // ── pure-function ports of the upstream inline vectors ──

    #[test]
    fn strips_jsonp_wrappers() {
        // Upstream api.rs:361-368 inline vector.
        assert_eq!(
            strip_jsonp("MusicInfoCallback({\"a\":1})", "MusicInfoCallback"),
            "{\"a\":1}"
        );
        assert_eq!(strip_jsonp("{\"a\":1}", "MusicJsonCallback"), "{\"a\":1}");
    }

    #[test]
    fn ids_are_url_decoded() {
        // Upstream api.rs:400-404 inline vector.
        assert_eq!(normalize_id("abc%2Bdef"), "abc+def");
        assert_eq!(normalize_id("  plain  "), "plain");
        assert_eq!(normalize_id(""), "");
    }

    #[test]
    fn parses_songs_and_playlists_from_sections() {
        // Upstream api.rs:371-398 inline vector.
        let json = json!({
            "section_list": [{
                "item_list": [
                    { "type": 5, "song": [{ "song_info": {
                        "id": "S1",
                        "name": "晴天",
                        "album_name": "叶惠美",
                        "album_id": "A1",
                        "play_duration": 269,
                        "artist_list": [{ "name": "周杰伦" }],
                        "images": [{ "width": 150, "url": "small.jpg" }, { "width": 300, "url": "big.jpg" }]
                    }}]},
                    { "type": 1, "editor_playlist": { "id": "P1", "name": "华语", "images": [] } },
                    { "type": 2, "album": { "id": "A1", "name": "叶惠美", "artist_list": [{ "name": "周杰伦" }] } }
                ]
            }]
        });
        let songs = songs_from_sections(&json);
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].id.id, "S1");
        assert_eq!(songs[0].artists, vec!["周杰伦".to_string()]);
        assert_eq!(songs[0].artwork_url.as_deref(), Some("big.jpg"));

        let playlists = playlists_from_sections(&json);
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].title, "华语");
        assert_eq!(playlists[0].id.id, "P1");
        assert_eq!(
            playlists[0].link.as_deref(),
            Some("https://www.joox.com/sg/playlist/P1")
        );
    }

    #[test]
    fn rejects_songs_without_an_id_or_title() {
        assert!(parse_song_info(&json!({ "name": "无 ID" })).is_none());
        assert!(parse_song_info(&json!({ "id": "S1" })).is_none());
        assert!(parse_song_info(&json!({ "id": "S1", "name": "  " })).is_none());
        // Zero duration stays None; empty album fields fold to None.
        let track = parse_song_info(&json!({
            "id": "S2", "name": "歌", "play_duration": 0, "album_name": "", "album_id": ""
        }))
        .unwrap();
        assert_eq!(track.duration, None);
        assert_eq!(track.album, None);
        assert_eq!(track.album_id, None);
    }

    #[test]
    fn search_entries_may_be_array_wrapped_or_plain() {
        // The live 2026-11-19 shape wraps every entry (tracks: [[{...}]]),
        // upstream also tolerates plain objects (api.rs:153-159).
        let json = json!({ "tracks": [
            [{ "id": "A", "name": "数组包裹" }],
            { "id": "B", "name": "直接对象" }
        ]});
        let songs = songs_from_search(&json);
        assert_eq!(songs.len(), 2);
        assert_eq!(songs[0].id.id, "A");
        assert_eq!(songs[1].id.id, "B");
        // Dedup by id (upstream api.rs:163-165).
        let dup = json!({ "tracks": [
            [{ "id": "A", "name": "x" }],
            [{ "id": "A", "name": "y" }]
        ]});
        assert_eq!(songs_from_search(&dup).len(), 1);
    }

    #[test]
    fn song_detail_parses_the_kbps_ladder() {
        // Schema-derived vector (songinfo_stream.txt): kbps_map as a
        // JSON-encoded string, 96 flagged "0" → m4aUrl dropped, no m4a URL
        // present anyway.
        let body = fixture!("songinfo_stream.txt");
        let json: Value = serde_json::from_str(strip_jsonp(body, "MusicInfoCallback")).unwrap();
        let detail = parse_song_detail("bLnv0PqDX_qAlIqapc+Okw==", &json).unwrap();
        assert_eq!(detail.song.title, "晴天");
        assert_eq!(detail.song.artists, vec!["周杰倫".to_string()]);
        assert_eq!(detail.song.album.as_deref(), Some("葉惠美"));
        assert_eq!(detail.song.duration.map(|d| d.as_secs()), Some(269));
        assert_eq!(
            detail.song.extra.get("songid").map(String::as_str),
            Some("bLnv0PqDX_qAlIqapc+Okw==")
        );
        assert_eq!(
            detail.song.available_qualities,
            vec![StreamQuality::Low, StreamQuality::High],
            "320/192 map High, 128 maps Low, 96 is flagged off"
        );
        assert_eq!(detail.urls.len(), 3);
        assert_eq!(detail.urls[0].0, StreamQuality::High);
        assert_eq!(detail.urls[0].1, AudioFormat::Mp3);
        assert_eq!(
            detail.urls[2].0,
            StreamQuality::Low,
            "mp3Url is the 128 tier"
        );
        assert_eq!(detail.urls[2].1, AudioFormat::Mp3);
        // The m4aUrl field maps Low/M4a (upstream 96 row, api.rs:311-313).
        assert_eq!(
            quality_of_field("m4aUrl"),
            (StreamQuality::Low, AudioFormat::M4a)
        );
    }

    #[test]
    fn song_detail_kbps_map_object_form_and_zero_drop() {
        // The dual branch (api.rs:303-306): kbps_map as a JSON object with
        // 192 flagged 0 (songinfo_stream_object_kbps.txt).
        let body = fixture!("songinfo_stream_object_kbps.txt");
        let json: Value = serde_json::from_str(strip_jsonp(body, "MusicInfoCallback")).unwrap();
        let detail = parse_song_detail("S1", &json).unwrap();
        assert_eq!(
            detail.song.available_qualities,
            vec![StreamQuality::Low, StreamQuality::High],
            "320 High + 128 Low survive; 192 and 96 are flagged 0"
        );
        assert_eq!(detail.urls.len(), 2, "192 dropped, 96 dropped");
        assert_eq!(detail.urls[0].0, StreamQuality::High);
        assert_eq!(detail.urls[1].0, StreamQuality::Low);
    }

    #[test]
    fn song_detail_without_a_title_answers_provider_error() {
        let error = parse_song_detail("S1", &json!({ "msong": "  " })).unwrap_err();
        assert!(matches!(error, Error::Provider { .. }), "got {error:?}");
        assert!(error.to_string().contains("JOOX 单曲详情为空"));
    }

    #[test]
    fn select_candidate_serves_the_requested_tier_and_falls_high() {
        let candidates = vec![
            (StreamQuality::High, AudioFormat::Mp3, "high".to_string()),
            (StreamQuality::Low, AudioFormat::Mp3, "low".to_string()),
        ];
        // Requested Low: the first tier at-or-below wins (never High).
        assert_eq!(
            select_candidate(candidates.clone(), StreamQuality::Low)
                .unwrap()
                .2,
            "low"
        );
        // Requested High: High wins.
        assert_eq!(
            select_candidate(candidates.clone(), StreamQuality::High)
                .unwrap()
                .2,
            "high"
        );
        // Requested Lossless with only High/Low available: the top tier is
        // served rather than failing (upstream mod.rs:144-146).
        assert_eq!(
            select_candidate(candidates.clone(), StreamQuality::Lossless)
                .unwrap()
                .2,
            "high"
        );
        // No candidates at all: None → the provider error.
        assert!(select_candidate(Vec::new(), StreamQuality::Low).is_none());
    }

    #[test]
    fn lyric_field_decodes_base64_and_keeps_plain_parity() {
        // Live 2026-11-19 shape: base64-encoded LRC.
        let encoded = base64::engine::general_purpose::STANDARD.encode("[ti:晴天]\n[00:01.00]line");
        assert_eq!(decode_lyric_field(&encoded), "[ti:晴天]\n[00:01.00]line");
        // Upstream historical shape: plain LRC passes through unchanged (the
        // brackets/colons/spaces make it invalid base64 by charset).
        let plain = "[ti:晴天]\n[00:01.00]词：x";
        assert_eq!(decode_lyric_field(plain), plain);
        assert_eq!(decode_lyric_field(""), "");
    }

    #[test]
    fn fixture_link_targets_follow_upstream_router() {
        for case in fixture_value!("links.json")["cases"].as_array().unwrap() {
            let source = case["source"].as_str().unwrap();
            let target = link_target(case["link"].as_str().unwrap());
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

    // ── live-capture fixtures (2026-11-19 anonymous re-probe) ──

    #[test]
    fn fixture_search_songs_parses_live_tracks() {
        let value = fixture_value!("search_songs.json");
        assert_eq!(value["error_code"], 0);
        let items = songs_from_search(&value);
        assert_eq!(items.len(), 3, "three live entries kept verbatim");
        let first = &items[0];
        assert_eq!(first.id.id, "bLnv0PqDX_qAlIqapc+Okw==");
        assert_eq!(first.title, "晴天");
        assert_eq!(first.artists, vec!["周杰倫".to_string()]);
        assert_eq!(first.album.as_deref(), Some("葉惠美"));
        assert_eq!(first.album_id.as_deref(), Some("YiT3xohoUs5qeo9aJ2yjDw=="));
        assert_eq!(first.duration.map(|d| d.as_secs()), Some(269));
        assert_eq!(
            first.artwork_url.as_deref(),
            Some("https://image.joox.com/JOOXcover/0/aaf707b7fa2e2c8c/300"),
            "the 300px image wins over the 1000px first entry"
        );
        assert_eq!(
            first.extra.get("songid").map(String::as_str),
            Some("bLnv0PqDX_qAlIqapc+Okw==")
        );
        // The live envelope has no section_list: the same body feeds the
        // playlist faces with an empty result (upstream parity).
        assert!(value["section_list"].is_null());
        assert!(playlists_from_sections(&value).is_empty());
        assert!(songs_from_sections(&value).is_empty());
    }

    #[test]
    fn fixture_lyric_live_decodes_the_base64_field() {
        let value = fixture_value!("lyric_live.json");
        assert_eq!(value["code"], 0);
        let raw = value["lyric"].as_str().unwrap();
        assert!(
            raw.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')),
            "the live field is pure base64"
        );
        let decoded = decode_lyric_field(raw);
        assert!(
            decoded.starts_with("[ti:晴天]"),
            "got {}",
            &decoded[..40.min(decoded.len())]
        );
        assert_eq!(decoded.lines().count(), 69, "69 live lyric lines");
        // The provider face wraps this into an Lrc document.
        let document = LyricDocument {
            format: LyricFormat::Lrc,
            lines: parse_lrc(&decoded),
            original: decoded,
            translation: None,
            romanization: None,
        };
        assert_eq!(document.format, LyricFormat::Lrc);
        assert!(!document.lines.is_empty());
    }

    // ── unified transport tests (mock transport, no network) ──

    #[tokio::test]
    async fn search_rides_the_joox_identity_and_parses() {
        let live = fixture_value!("search_songs.json").to_string();
        let (provider, mock) = provider_with_script(vec![Ok(mock_json(200, &live))]).await;
        let page = provider
            .search(&SearchQuery::new("晴天"))
            .await
            .expect("live search body parses");
        assert_eq!(page.items.len(), 3);
        assert_eq!(page.total, Some(3));
        assert!(!page.has_more, "upstream parity: has_more always false");
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        let request = &recorded[0];
        assert_eq!(request.method, crate::transport::HttpMethod::Get);
        assert!(request.idempotent);
        assert!(
            request
                .url
                .starts_with("https://cache.api.joox.com/openjoox/v2/search_type?"),
            "{}",
            request.url
        );
        assert!(
            request.url.contains("country=sg")
                && request.url.contains("lang=zh_cn")
                && request.url.contains("key=%E6%99%B4%E5%A4%A9")
                && request.url.contains("type=0"),
            "the fixed country/lang prefix and the encoded keyword must ride the query: {}",
            request.url
        );
        for (name, value) in [
            ("user-agent", UA_WEB),
            ("origin", "http://www.joox.com"),
            ("referer", "http://www.joox.com"),
            ("x-forwarded-for", X_FORWARDED_FOR),
        ] {
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(key, sent)| key == name && sent == value),
                "header {name} must ride the OpenJOOX request"
            );
        }
    }

    #[tokio::test]
    async fn empty_search_keyword_is_rejected_without_a_request() {
        let (provider, mock) = provider_with_script(vec![]).await;
        let error = provider
            .search(&SearchQuery::new("  "))
            .await
            .expect_err("empty keyword must not reach the endpoint");
        assert!(matches!(error, Error::InvalidInput(_)), "got {error:?}");
        assert!(mock.recorded().is_empty());
    }

    #[tokio::test]
    async fn resolve_on_the_dead_songinfo_surfaces_network_error() {
        // Live-pinned 2026-11-19: web_get_songinfo answers HTTP 404 with an
        // empty body. The ported ladder surfaces the refusal honestly.
        let (provider, mock) = provider_with_script(vec![Ok(mock_json(404, ""))]).await;
        let track = track_with_songid("bLnv0PqDX_qAlIqapc+Okw==");
        let error = provider
            .resolve(&track, &ResolveOptions::default())
            .await
            .expect_err("the dead endpoint must surface an error");
        assert!(matches!(error, Error::Network { .. }), "got {error:?}");
        assert!(error.provider_id() == Some(ID));
        assert!(error.to_string().contains("HTTP 404"), "got {error}");
        assert_eq!(mock.recorded().len(), 1);
        let request = &mock.recorded()[0];
        assert!(
            request
                .url
                .starts_with("https://api.joox.com/web-fcgi-bin/web_get_songinfo?songid="),
            "{}",
            request.url
        );
        assert!(
            request.url.contains("country=hk") && request.url.contains("lang=zh_TW"),
            "{}",
            request.url
        );
        // The JSONP endpoints ride UA + region marker only (no Origin/Referer).
        assert!(
            request
                .headers
                .iter()
                .any(|(key, sent)| key == "user-agent" && sent == UA_WEB)
        );
        assert!(
            request
                .headers
                .iter()
                .any(|(key, sent)| key == "x-forwarded-for" && sent == X_FORWARDED_FOR)
        );
        assert!(
            !request.headers.iter().any(|(key, _)| key == "referer"),
            "upstream parity: the JSONP endpoints carry no Referer"
        );
    }

    #[tokio::test]
    async fn resolve_serves_the_requested_tier_from_the_documented_shape() {
        let body = fixture!("songinfo_stream.txt");
        let (provider, mock) = provider_with_script(vec![Ok(mock_json(200, body))]).await;
        let track = track_with_songid("bLnv0PqDX_qAlIqapc+Okw==");
        let stream = provider
            .resolve(&track, &ResolveOptions::default())
            .await
            .expect("the documented shape resolves");
        assert_eq!(stream.quality, StreamQuality::High);
        assert_eq!(stream.format, AudioFormat::Mp3);
        assert_eq!(stream.url, "https://od.stream.joox.example/r320/song.mp3");
        assert_eq!(stream.duration.map(|d| d.as_secs()), Some(269));
        // Upstream contract (mod.rs:21-35): no size, advisory true.
        assert_eq!(stream.content_length, None);
        assert!(stream.size_is_advisory);
        assert!(stream.checksum.is_none());
        assert!(stream.headers.is_empty());
        assert!(stream.fallback_urls.is_empty());
        assert!(stream.fallback_endpoints.is_empty());
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn resolve_songid_falls_back_to_the_track_id() {
        let body = fixture!("songinfo_stream.txt");
        let (provider, _) = provider_with_script(vec![Ok(mock_json(200, body))]).await;
        let track = Track {
            extra: BTreeMap::new(),
            ..track_with_songid("S1")
        };
        let stream = provider
            .resolve(
                &track,
                &ResolveOptions {
                    quality: StreamQuality::Low,
                },
            )
            .await
            .expect("the track id is used when extra has no songid");
        // Requested Low with High+Low available: the at-or-below tier wins.
        assert_eq!(stream.quality, StreamQuality::Low);
        assert_eq!(stream.url, "https://od.stream.joox.example/mp3/song.mp3");
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
    async fn lyrics_decode_the_live_base64_field() {
        let inner = fixture_value!("lyric_live.json").to_string();
        let jsonp = format!("MusicJsonCallback({inner})\n");
        let (provider, mock) = provider_with_script(vec![Ok(mock_json(200, &jsonp))]).await;
        let track = track_with_songid("bLnv0PqDX_qAlIqapc+Okw==");
        let doc = provider
            .lyrics(&track)
            .await
            .expect("lyrics resolve")
            .expect("lyric present");
        assert_eq!(doc.format, LyricFormat::Lrc);
        assert!(doc.original.starts_with("[ti:晴天]"));
        assert!(!doc.lines.is_empty());
        let request = &mock.recorded()[0];
        assert!(
            request
                .url
                .starts_with("https://api.joox.com/web-fcgi-bin/web_lyric?musicid="),
            "{}",
            request.url
        );
        assert!(
            request.url.contains("country=sg") && request.url.contains("lang=zh_cn"),
            "{}",
            request.url
        );
    }

    #[tokio::test]
    async fn lyrics_without_a_lyric_answer_not_found() {
        let jsonp = r#"MusicJsonCallback({"code":0,"lyric":"","msg":"","type":0})"#;
        let (provider, mock) = provider_with_script(vec![Ok(mock_json(200, jsonp))]).await;
        let track = track_with_songid("S1");
        let error = provider
            .lyrics(&track)
            .await
            .expect_err("empty lyric must answer NotFound");
        assert!(matches!(error, Error::NotFound), "got {error:?}");
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn container_faces_surface_the_dead_endpoint_honestly() {
        // Live-pinned 2026-11-19: /openjoox/v3/playlist answers nginx 404
        // (playlist_dead_404.html is the verbatim body).
        let dead = fixture!("playlist_dead_404.html");
        let (provider, mock) = provider_with_script(vec![Ok(mock_json(404, dead))]).await;
        for error in [
            provider.playlist("P1").await.unwrap_err(),
            provider.playlist_tracks("P1").await.unwrap_err(),
            provider.album("A1").await.unwrap_err(),
            provider.album_tracks("A1").await.unwrap_err(),
            provider
                .resolve_link("https://www.joox.com/hk/playlist/P1")
                .await
                .unwrap_err(),
            provider
                .resolve_link("https://www.joox.com/hk/album/A1")
                .await
                .unwrap_err(),
        ] {
            assert!(
                matches!(error, Error::Network { .. }),
                "dead container must surface Network, got {error:?}"
            );
        }
        assert_eq!(mock.recorded().len(), 6);
        for request in mock.recorded() {
            assert!(
                request
                    .url
                    .starts_with("https://cache.api.joox.com/openjoox/v3/playlist?"),
                "{}",
                request.url
            );
        }
    }

    #[tokio::test]
    async fn empty_container_answers_not_found() {
        // Upstream parity (mod.rs:191-193, :230-232): an empty parsed track
        // list is the missing-container signal.
        let (provider, _) = provider_with_script(vec![Ok(mock_json(
            200,
            r#"{"name":"空歌单","section_list":[]}"#,
        ))])
        .await;
        let error = provider.playlist("P1").await.unwrap_err();
        assert!(matches!(error, Error::NotFound), "got {error:?}");
    }

    #[tokio::test]
    async fn container_metadata_comes_from_the_documented_shape() {
        // Upstream parse_link playlist branch (mod.rs:225-247): name from
        // json["name"] with the JOOX 歌单 fallback, cover/song_count from the
        // parsed songs, link built from the id.
        let body = json!({
            "name": " 华语经典 ",
            "section_list": [{
                "item_list": [
                    { "type": 5, "song": [{ "song_info": {
                        "id": "S1",
                        "name": "晴天",
                        "album_name": "叶惠美",
                        "play_duration": 269,
                        "artist_list": [{ "name": "周杰伦" }],
                        "images": [{ "width": 300, "url": "cover.jpg" }]
                    }}]}
                ]
            }]
        })
        .to_string();
        let (provider, _) =
            provider_with_script(vec![Ok(mock_json(200, &body)), Ok(mock_json(200, &body))]).await;
        let (playlist, tracks) = provider
            .container("P1", "JOOX 歌单", playlist_link_of)
            .await
            .expect("container parses");
        assert_eq!(playlist.title, "华语经典", "the live name is trimmed");
        assert_eq!(playlist.track_count, Some(1));
        assert_eq!(playlist.artwork_url.as_deref(), Some("cover.jpg"));
        assert_eq!(playlist.creator.as_deref(), Some("周杰伦"));
        assert_eq!(
            playlist.link.as_deref(),
            Some("https://www.joox.com/sg/playlist/P1")
        );
        assert_eq!(
            playlist.extra.get("playlist_id").map(String::as_str),
            Some("P1")
        );
        assert_eq!(tracks.len(), 1);
        // Album metadata derives the artist list from the first track
        // (upstream mod.rs:266: album.creator = first song's singer).
        let album = provider.album("A1").await.expect("album parses");
        assert_eq!(album.title, "华语经典");
        assert_eq!(album.artists, vec!["周杰伦".to_string()]);
        assert_eq!(album.track_count, Some(1));
    }

    #[tokio::test]
    async fn playlist_search_parses_empty_without_sections() {
        // Live status 2026-11-19: the search response carries no
        // `section_list`; upstream parity reads it unconditionally (api.rs
        // section walks skip missing keys), so the result is empty, never an
        // error — the same class as the qianqian live-refused playlist search.
        let live = fixture_value!("search_songs.json").to_string();
        let (provider, mock) = provider_with_script(vec![Ok(mock_json(200, &live))]).await;
        let found = provider
            .search_playlists("晴天", 1)
            .await
            .expect("the section-less body parses to an empty result");
        assert!(found.is_empty());
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn playlist_search_extracts_editor_playlists_when_present() {
        // Upstream shape (api.rs:231-252): playlist rows ride
        // section_list[].item_list[].editor_playlist.
        let body = json!({
            "section_list": [{
                "item_list": [
                    { "type": 1, "editor_playlist": {
                        "id": "P1",
                        "name": "华语",
                        "song_count": 42,
                        "images": [{ "width": 300, "url": "p.jpg" }]
                    }}
                ]
            }]
        })
        .to_string();
        let (provider, _) = provider_with_script(vec![Ok(mock_json(200, &body))]).await;
        let found = provider
            .search_playlists("华语", 1)
            .await
            .expect("sections parse");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id.id, "P1");
        assert_eq!(found[0].track_count, Some(42));
        assert_eq!(found[0].artwork_url.as_deref(), Some("p.jpg"));
    }

    #[tokio::test]
    async fn unsupported_playlist_faces_answer_unsupported() {
        let (provider, mock) = provider_with_script(vec![]).await;
        for error in [
            provider.categories().await.unwrap_err(),
            provider.playlists("x", 1, 30).await.unwrap_err(),
            provider.user_playlists(1, 30).await.unwrap_err(),
        ] {
            assert!(
                matches!(error, Error::Unsupported(ref id) if id == ID),
                "got {error:?}"
            );
        }
        assert!(mock.recorded().is_empty(), "no request may leave");
    }

    #[tokio::test]
    async fn link_song_resolves_through_songinfo() {
        let body = fixture!("songinfo_stream.txt");
        let (provider, _) = provider_with_script(vec![Ok(mock_json(200, body))]).await;
        match provider
            .resolve_link("https://www.joox.com/hk/single/abc123")
            .await
            .expect("song link resolves")
        {
            ResolvedLink::Track(track) => {
                assert_eq!(track.id, TrackId::new(ID, "abc123"));
                assert_eq!(track.title, "晴天");
            }
            other => panic!("expected track, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unrecognized_link_answers_invalid_input_without_a_request() {
        let (provider, mock) = provider_with_script(vec![]).await;
        let error = provider
            .resolve_link("https://www.joox.com/")
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidInput(_)), "got {error:?}");
        assert!(error.to_string().contains("无法识别 JOOX 链接"));
        assert!(mock.recorded().is_empty());
    }

    #[tokio::test]
    async fn session_cookies_ride_the_openjoox_requests_only() {
        // The musid-dli form of upstream's JOOX_COOKIE (api.rs:101-117): the
        // cookie rides the OpenJOOX endpoints; the JSONP endpoints carried no
        // cookie upstream and still do not.
        let sessions = SessionStore::default();
        let mut session = crate::session::ProviderSession::default();
        session.cookies.insert("wmid".into(), "h9".into());
        sessions.set(ID, session).expect("session stores");
        let provider = JooxProvider::new().with_session_store(sessions);
        let request =
            provider.openjoox_request(TransportRequest::get("https://cache.api.joox.com/x"));
        assert!(
            request
                .headers
                .iter()
                .any(|(key, sent)| key == "cookie" && sent == "wmid=h9"),
            "the session cookie must ride the OpenJOOX request"
        );
        let jsonp = provider.jsonp_request(TransportRequest::get("https://api.joox.com/y"));
        assert!(
            !jsonp.headers.iter().any(|(key, _)| key == "cookie"),
            "upstream parity: the JSONP endpoints carry no cookie"
        );
    }

    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = JooxProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.lyrics);
        assert!(descriptor.capabilities.playlists);
        assert!(descriptor.capabilities.albums);
        assert!(descriptor.capabilities.link_resolve);
        // No upstream face: no artist/chart/auth slot exists in source/src/joox/.
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
        assert_eq!(crate::models::provider_to_voicefox_source(ID), Some("joox"));
        assert_eq!(crate::models::voicefox_source_to_provider("joox"), Some(ID));
    }
}
