//! 5sing (Kugou original-music community) provider migrated from VoiceFox's
//! `lx-source::fivesing`. Unsigned JSON endpoints: search (songs + playlists),
//! song metadata with plain-text LRC lyrics (`dynamicWords`), three-tier
//! stream URLs (SQ/HQ/LQ with backups) and playlist details parsed from the
//! site's HTML pages. Track ids carry the song type suffix (`{id}|{kind}`)
//! because the same numeric id can exist as 原创/翻唱/伴奏.
use crate::{
    Error, ErrorDetail, Result,
    lrc::parse_lrc,
    models::{
        AudioFormat, FallbackEndpoint, LyricDocument, LyricFormat, Playlist, StreamInfo,
        StreamQuality, Track, TrackId,
    },
    provider::{
        AuthCapability, AuthState, AuthenticationProvider, CollectionProvider, LinkResolver,
        LoginStatus, LyricsProvider, MusicProvider, PlaylistProvider, ResolveOptions, ResolvedLink,
        SearchPage, SearchQuery, StreamResolver, TrackSearch,
    },
    session::SessionStore,
    transport::{ClientProfile, Http, RetryPolicy, TransportError, TransportRequest},
};
use async_trait::async_trait;
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

const ID: &str = "fivesing";
const UA_WEB: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/143.0 Safari/537.36";
const REFERER: &str = "http://5sing.kugou.com/";
const SEARCH_API: &str = "http://search.5sing.kugou.com/home/json";
const SONG_INFO_API: &str = "http://mobileapi.5sing.kugou.com/song/newget";
const SONG_URL_API: &str = "http://mobileapi.5sing.kugou.com/song/getSongUrl";
const PLAYLIST_INFO_API: &str = "http://mobileapi.5sing.kugou.com/song/getsonglist";

#[derive(Clone)]
pub struct FivesingProvider {
    http: Http,
    sessions: SessionStore,
}
impl std::fmt::Debug for FivesingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FivesingProvider")
            .field("session_store", &"redacted")
            .finish()
    }
}
impl Default for FivesingProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl FivesingProvider {
    pub fn new() -> Self {
        // Client construction moved onto the unified transport layer: the
        // profile carries exactly today's knobs (15s total timeout, follow
        // redirects); the build fallback shape lives in `build_client`
        // (TRANSPORT_DESIGN.md §4.3).
        let http = Http::from_profile(&ClientProfile::provider_default());
        Self {
            http,
            sessions: SessionStore::default(),
        }
    }
    /// Test seam: inject a transport handle (a mock in unit tests). The
    /// production constructor is [`FivesingProvider::new`]. Test-only so
    /// non-test builds see no dead constructor.
    #[cfg(test)]
    pub(crate) fn with_transport(http: Http) -> Self {
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
    /// Register all currently implemented 5sing capabilities in one step.
    /// The AuthenticationProvider slot is registered too (dead-impl decision,
    /// AUTH_REGISTRY_DESIGN.md §9.6): the impl is the honest constant-LoggedOut
    /// view (no login surface exists on these endpoints), so hosts querying
    /// login state uniformly see `LoggedOut` instead of a missing slot, while
    /// the `authentication` capability bool and the `AuthCapability::None`
    /// facet both keep saying "no account system".
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_lyrics(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_playlist(provider.clone());
        registry.register_link_resolver(provider.clone());
        registry.register_authentication(provider);
    }
    /// Adds the fixed web identity headers to a [`TransportRequest`].
    /// Platform-specific and kept in the provider: the UA/Referer pair is the
    /// web identity the anonymous 5sing endpoints expect (TRANSPORT_DESIGN.md
    /// §1).
    fn request(&self, request: TransportRequest) -> TransportRequest {
        request
            .header("user-agent", UA_WEB)
            .header("referer", REFERER)
    }
    /// GET one anonymous JSON endpoint through the unified transport layer.
    ///
    /// Declared behavior upgrade (TRANSPORT_DESIGN.md §5.4/§5.5 row 3, slice
    /// 8): this face was single-shot — `get_json` had no retry loop, so any
    /// transport error or 429/5xx was terminal. Through `send_raw` it joins
    /// the [`RetryPolicy::standard()`] ladder: transport errors, 429 and 5xx
    /// retry up to 4 extra attempts with exponential backoff (0.5/1/2/4s, 8s
    /// cap) + ≤250ms jitter + a server `Retry-After` clamped to 10s — worst
    /// case ≈83.5s per call (5 sends × 15s + 7.5s backoff + ≤1s jitter),
    /// ≈116s when a `Retry-After` rides every retry. A mid-body connection
    /// reset (terminal "{label}: ..." today) also becomes retryable (§5.5
    /// row 4).
    ///
    /// Error texts keep today's shapes with one declared drift: terminal
    /// statuses read "{label}: {classify_http text}" — "HTTP 404" instead of
    /// today's "{label}: HTTP 404 Not Found" (the numeric status replaces
    /// reqwest's reason phrase; §5.5 row 7 class). Send failures keep
    /// "{label}: {error}" (the adapter forwards reqwest's display text), and
    /// parse failures keep the "{label}: parse failed: " prefix but now carry
    /// the bare serde_json message instead of reqwest's "error decoding
    /// response body: ..." wrapper.
    async fn get_json(&self, url: &str, label: &str) -> Result<Value> {
        let request = self.request(TransportRequest::get(url));
        let response = match self.http.send_raw(&request, &RetryPolicy::standard()).await {
            Ok(response) => response,
            // classify_http Fatal text is "HTTP {status}", so this keeps the
            // "{label}: HTTP {status}" shape (numeric status, see above).
            Err(TransportError::Status(message)) => {
                return Err(perr(format!("{label}: {message}")));
            }
            Err(error) => return Err(perr(format!("{label}: {error}"))),
        };
        response
            .json()
            .map_err(|error| perr(format!("{label}: parse failed: {error}")))
    }
    async fn search_by_kind(&self, keyword: &str, kind: &str) -> Result<Value> {
        self.get_json(
            &format!(
                "{SEARCH_API}?keyword={}&sort=1&page=1&filter=0&type={kind}",
                urlencoding::encode(keyword.trim())
            ),
            "search",
        )
        .await
    }
    /// Track ids embed the song type: `{songid}|{yc|fc|bz}`.
    fn split_track_id(track: &Track) -> Result<(String, String)> {
        let raw = track
            .extra
            .get("songid")
            .map(|id| {
                format!(
                    "{id}|{}",
                    track.extra.get("songtype").cloned().unwrap_or_default()
                )
            })
            .unwrap_or_else(|| track.id.id.clone());
        let (song_id, kind) = raw
            .split_once('|')
            .ok_or_else(|| perr("track id is missing the song type"))?;
        Ok((song_id.to_owned(), kind.to_owned()))
    }
    async fn song_info(&self, song_id: &str, song_type: &str) -> Result<Value> {
        let json = self
            .get_json(
                &format!("{SONG_INFO_API}?songid={song_id}&songtype={song_type}"),
                "song info",
            )
            .await?;
        if json["data"]["SN"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            return Err(Error::NotFound);
        }
        Ok(json)
    }
    async fn audio_links(
        &self,
        song_id: &str,
        song_type: &str,
    ) -> Result<Vec<(StreamQuality, String, Vec<String>)>> {
        let json = self
            .get_json(
                &format!("{SONG_URL_API}?songid={song_id}&songtype={song_type}"),
                "stream url",
            )
            .await?;
        fs_parse_get_song_url(&json)
    }
    /// 5sing serves file sizes only; estimate duration from the 320 kbps size.
    fn duration_from_size(size: u64) -> Duration {
        Duration::from_secs(size.saturating_mul(8) / 320_000)
    }
}
fn perr(message: impl Into<String>) -> Error {
    Error::Provider {
        provider: ID.into(),
        message: message.into(),
    }
}
/// The `getSongUrl` code gate (`code != 1000`) split by what the refusal
/// envelope declares (AUTH_REGISTRY_DESIGN.md §9.6; upstream
/// fivesing/api.rs:272-274 collapses every refusal into one string). No
/// refusal code table exists upstream or in the fixtures — the only
/// live-captured refusal (2026-11-18) is the declared
/// `{"code":1001,"message":"缺少songid","success":false}` shape — so the two
/// M2 categories key on the envelope's own refusal flag:
/// - `"success": false` is the endpoint's declared refusal of the
///   stream-url grant for this request → `AuthenticationRequired` (not
///   retryable: no blind resend lifts a platform refusal, and the recover
///   identity would be a platform login this no-login provider never
///   performs; the detail slot keeps the code and the platform's message);
/// - any other `code != 1000` shape answers off-contract, without a declared
///   refusal → `Unavailable` (retryable; the standard-ladder resend has
///   already run and stays the sensible recovery). This arm is defensive
///   today: no fixture or capture shows an undeclared refusal shape (the
///   kugou §9.2 "defensive arm, doc'd" precedent).
fn stream_url_gate_error(json: &Value) -> Error {
    let code = json["code"]
        .as_i64()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "missing".to_owned());
    let message = json["message"]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| format!(", {text}"))
        .unwrap_or_default();
    if json["success"] == Value::Bool(false) {
        return Error::AuthenticationRequired {
            provider: Some(ID.into()),
            detail: Some(ErrorDetail::new(format!(
                "stream url: code {code}{message}"
            ))),
        };
    }
    Error::Unavailable {
        provider: Some(ID.into()),
        detail: Some(ErrorDetail::new(format!(
            "stream url: code {code}, no declared refusal"
        ))),
    }
}

/// Parses the `getSongUrl` envelope: `code == 1000` gates success, then the
/// sq/hq/lq tiers are read with their `_backup` URLs; an empty string or `"0"`
/// marks a missing tier or backup (upstream fivesing/api.rs:272-305).
/// Refusals go through [`stream_url_gate_error`] (M2 categories, §9.6).
fn fs_parse_get_song_url(json: &Value) -> Result<Vec<(StreamQuality, String, Vec<String>)>> {
    if json["code"].as_i64() != Some(1000) {
        return Err(stream_url_gate_error(json));
    }
    let data = &json["data"];
    let mut entries = Vec::new();
    for (url_key, backup_key, quality) in [
        ("squrl", "squrl_backup", StreamQuality::Lossless),
        ("hqurl", "hqurl_backup", StreamQuality::High),
        ("lqurl", "lqurl_backup", StreamQuality::Low),
    ] {
        let Some(url) = data[url_key]
            .as_str()
            .map(str::trim)
            .filter(|url| !url.is_empty() && *url != "0")
        else {
            continue;
        };
        let backups = data[backup_key]
            .as_str()
            .map(str::trim)
            .filter(|url| !url.is_empty() && *url != "0")
            .map(|url| vec![url.to_owned()])
            .unwrap_or_default();
        entries.push((quality, url.to_owned(), backups));
    }
    if entries.is_empty() {
        return Err(Error::NotFound);
    }
    Ok(entries)
}
/// The `newget` `dynamicWords` field is the plain-text LRC lyric
/// (upstream fivesing/api.rs:210-224).
fn fs_lyric_text(json: &Value) -> Option<String> {
    json["data"]["dynamicWords"]
        .as_str()
        .map(str::trim)
        .filter(|lyric| !lyric.is_empty())
        .map(str::to_owned)
}
/// Parses a `newget` `data` block into a track for link resolution; `None`
/// when the title (`SN`) is empty (upstream fivesing/api.rs:184-208).
fn fs_parse_song_detail(song_id: &str, song_type: &str, json: &Value) -> Option<Track> {
    let data = &json["data"];
    let title = clean_text(data["SN"].as_str().unwrap_or_default());
    if title.is_empty() {
        return None;
    }
    let mut extra = BTreeMap::new();
    extra.insert("songid".into(), song_id.to_owned());
    extra.insert("songtype".into(), song_type.to_owned());
    Some(Track {
        id: TrackId::new(ID, format!("{song_id}|{song_type}")),
        title,
        artists: vec![data["user"]["NN"].as_str().unwrap_or_default().to_owned()],
        album: None,
        album_id: None,
        duration: None,
        artwork_url: data["user"]["I"]
            .as_str()
            .filter(|cover| !cover.is_empty())
            .map(str::to_owned),
        isrc: None,
        available_qualities: vec![
            StreamQuality::Lossless,
            StreamQuality::High,
            StreamQuality::Low,
        ],
        audio: None,
        extra,
    })
}
/// Parses the `getsonglist` envelope; `data` may be an empty array when the
/// playlist is missing or private (upstream fivesing/api.rs:308-340).
fn fs_parse_playlist_info(id: &str, json: &Value) -> Result<Playlist> {
    let data = &json["data"];
    if !data.is_object() {
        return Err(perr("5sing playlist missing or inaccessible"));
    }
    let title = clean_text(data["T"].as_str().unwrap_or_default());
    let user_id = fs_value_string(&data["user"]["ID"]);
    if title.is_empty() || user_id.is_empty() {
        return Err(perr("5sing playlist info is incomplete"));
    }
    let _ = user_id;
    Ok(Playlist {
        id: TrackId::new(ID, id.to_owned()),
        title,
        description: {
            let description = clean_text(data["C"].as_str().unwrap_or_default());
            (!description.is_empty() && description != "0").then_some(description)
        },
        artwork_url: data["P"]
            .as_str()
            .filter(|url| !url.is_empty())
            .map(str::to_owned),
        track_count: fs_value_u64(&data["E"]).map(|n| n as u32),
        tracks: vec![],
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
    })
}
/// Search titles carry `<em>` highlights and HTML entities.
fn clean_text(raw: &str) -> String {
    let decoded = raw
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    let mut out = String::with_capacity(decoded.len());
    let mut in_tag = false;
    for character in decoded.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_owned()
}
fn fs_value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
        .or_else(|| value.as_i64().map(|n| n.to_string()))
        .unwrap_or_default()
}
fn fs_value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|v| u64::try_from(v).ok()))
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}
fn fs_parse_search_song(item: &Value) -> Option<Track> {
    let song_id = fs_value_string(&item["songId"]);
    let kind = item["typeEname"].as_str().unwrap_or_default().to_owned();
    let title = clean_text(item["songName"].as_str().unwrap_or_default());
    if song_id.is_empty() || kind.is_empty() || title.is_empty() {
        return None;
    }
    let mut extra = BTreeMap::new();
    extra.insert("songid".into(), song_id.clone());
    extra.insert("songtype".into(), kind.clone());
    Some(Track {
        id: TrackId::new(ID, format!("{song_id}|{kind}")),
        title,
        artists: vec![clean_text(item["singer"].as_str().unwrap_or_default())],
        album: None,
        album_id: None,
        duration: fs_value_u64(&item["songSize"])
            .filter(|size| *size > 0)
            .map(FivesingProvider::duration_from_size),
        artwork_url: item["user"]["I"]
            .as_str()
            .filter(|url| !url.is_empty())
            .map(str::to_owned),
        isrc: None,
        available_qualities: vec![
            StreamQuality::Lossless,
            StreamQuality::High,
            StreamQuality::Low,
        ],
        audio: None,
        extra,
    })
}
fn fs_parse_playlist(item: &Value) -> Option<Playlist> {
    let id = fs_value_string(&item["songListId"]);
    let title = clean_text(item["title"].as_str().unwrap_or_default());
    if id.is_empty() || title.is_empty() {
        return None;
    }
    let user_id = fs_value_string(&item["userId"]);
    let creator = item["userName"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let description = clean_text(item["content"].as_str().unwrap_or_default());
    let _ = user_id;
    let _ = creator;
    Some(Playlist {
        id: TrackId::new(ID, id.clone()),
        title,
        description: (!description.is_empty() && description != "0").then_some(description),
        artwork_url: item["pictureUrl"]
            .as_str()
            .filter(|url| !url.is_empty())
            .map(str::to_owned),
        track_count: fs_value_u64(&item["songCnt"]).map(|n| n as u32),
        tracks: vec![],
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
    })
}
/// Link shapes: `/{yc|fc|bz}/{id}.html` songs and `/{uid}/dj/{id}.html` playlists.
fn fs_link_target(link: &str) -> Result<(String, String)> {
    let lower = link.to_ascii_lowercase();
    if let Some(pos) = lower.find("5sing.kugou.com/") {
        let tail = &link[pos + "5sing.kugou.com/".len()..];
        let segments: Vec<&str> = tail.split('/').collect();
        if segments.len() >= 2 {
            if matches!(segments[0], "yc" | "fc" | "bz") {
                let id = segments[1].split('.').next().unwrap_or_default().to_owned();
                if !id.is_empty() {
                    return Ok((segments[0].to_owned(), id));
                }
            }
            if segments.len() >= 3 && segments[1] == "dj" {
                let id = segments[2].split('.').next().unwrap_or_default().to_owned();
                if !id.is_empty() {
                    return Ok(("dj".into(), id));
                }
            }
        }
    }
    Err(Error::InvalidInput(
        "unrecognized 5sing song/playlist link".into(),
    ))
}
impl MusicProvider for FivesingProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        "5sing"
    }
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            lyrics: true,
            playlists: true,
            link_resolve: true,
            ..Default::default()
        }
    }
    /// The single `authentication` bool stays false (there is no login flow;
    /// the registration test pins it), but the descriptor's auth facet names
    /// the state explicitly instead of relying on the default derivation:
    /// `AuthCapability::None` — 5sing has no account system on these
    /// endpoints at all (AUTH_REGISTRY_DESIGN.md §5.4; the kuwo §9.3
    /// explicit-facet precedent).
    fn auth_capability(&self) -> AuthCapability {
        AuthCapability::None
    }
}
#[async_trait]
impl TrackSearch for FivesingProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.keyword.trim().is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        let value = self.search_by_kind(q.keyword.trim(), "0").await?;
        let items = value["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(fs_parse_search_song)
            .collect::<Vec<_>>();
        let total = items.len() as u64;
        Ok(SearchPage {
            items,
            total: Some(total),
            has_more: false,
        })
    }
}
/// Resolve-timeout reconciliation (TRANSPORT_DESIGN.md §5.4/§5.5 row 3,
/// slice 8): `resolve` is network-bound — it always runs `audio_links`
/// through `get_json` on the `standard()` ladder (worst case ≈83.5s per
/// call, ≈116s when a server `Retry-After` rides every retry at the 10s
/// cap). Hosts tuned for today's single-shot 5sing (≈15s per call) must
/// raise the resolve-level `per_provider_timeout` (resolve.rs) above that —
/// or pass a reduced `transport_retries` — or the ladder is cancelled
/// mid-flight and masked as "provider resolution timed out" plus a
/// circuit-breaker failure (resolve.rs:150-157). The lyrics and link faces
/// ride the same ladder but sit outside the resolve timeout.
#[async_trait]
impl StreamResolver for FivesingProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to 5sing".into()));
        }
        let (song_id, song_type) = Self::split_track_id(track)?;
        let links = self.audio_links(&song_id, &song_type).await?;
        // Prefer the highest tier not above the request; fall back to whatever
        // exists when the request exceeds the catalog.
        let mut sorted = links.clone();
        sorted.sort_by_key(|(quality, _, _)| std::cmp::Reverse(*quality));
        let (quality, url, backups) = sorted
            .iter()
            .find(|(quality, _, _)| *quality <= options.quality)
            .or_else(|| sorted.first())
            .cloned()
            .ok_or(Error::NotFound)?;
        // Structured slot backfill (MODEL_ALIGNMENT_DESIGN.md §2.4): 5sing
        // backups mirror the selected tier (`squrl_backup`/`hqurl_backup`/
        // `lqurl_backup`), so each endpoint carries that tier explicitly.
        // The legacy string slot keeps the same URLs verbatim (§2.4 defines
        // the dual fill: legacy strings first, cross-slot dedup keeps the
        // first occurrence).
        let fallback_endpoints = backups
            .iter()
            .map(|url| FallbackEndpoint {
                url: url.clone(),
                headers: vec![],
                expires_at_unix: None,
                content_length: None,
                quality: Some(quality),
            })
            .collect();
        Ok(StreamInfo {
            url,
            quality,
            format: AudioFormat::Mp3,
            duration: track.duration,
            expires_at_unix: None,
            content_length: None,
            checksum: None,
            headers: vec![],
            fallback_urls: backups,
            index_range: None,
            init_range: None,
            max_chunk_size: None,
            size_is_advisory: true,
            checksum_algorithm: None,
            fallback_endpoints,
        })
    }
}
#[async_trait]
impl LyricsProvider for FivesingProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput("track does not belong to 5sing".into()));
        }
        let (song_id, song_type) = Self::split_track_id(track)?;
        let json = self.song_info(&song_id, &song_type).await?;
        let original = fs_lyric_text(&json);
        match original {
            Some(original) => Ok(Some(LyricDocument {
                format: LyricFormat::Lrc,
                lines: parse_lrc(&original),
                original,
                translation: None,
                romanization: None,
            })),
            None => Ok(None),
        }
    }
}
#[async_trait]
impl CollectionProvider for FivesingProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist> {
        self.playlist_info(id).await
    }
    // The track list is JS-rendered on the site and no anonymous API returns
    // it (live-verified 2026-11-17).
    async fn playlist_tracks(&self, _id: &str) -> Result<Vec<Track>> {
        Err(Error::Unsupported(
            "5sing playlist track lists require a JS-rendered page".into(),
        ))
    }
}
/// 5sing exposes no login surface to these endpoints: the auth-state view is
/// the constant `AuthState::LoggedOut` (no session is ever created), and the
/// legacy `LoginStatus` is its explicit fold through `to_login_status`
/// (AUTH_REGISTRY_DESIGN.md §2.3) rather than a hand-written literal. The
/// impl stays and is now registered (dead-impl decision, §9.6): hosts get an
/// honest LoggedOut/logout surface, while the `authentication` bool and the
/// `AuthCapability::None` facet both say no account system exists.
#[async_trait]
impl AuthenticationProvider for FivesingProvider {
    async fn login_status(&self) -> Result<LoginStatus> {
        Ok(AuthState::LoggedOut.to_login_status(None))
    }
    async fn logout(&self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl PlaylistProvider for FivesingProvider {
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
    async fn search_playlists(&self, keyword: &str, _page: u32) -> Result<Vec<Playlist>> {
        if keyword.trim().is_empty() {
            return Err(Error::InvalidInput(
                "playlist search keyword is empty".into(),
            ));
        }
        let value = self.search_by_kind(keyword.trim(), "1").await?;
        Ok(value["list"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(fs_parse_playlist)
            .collect())
    }
    async fn user_playlists(&self, _page: u32, _page_size: u32) -> Result<Vec<Playlist>> {
        Err(Error::Unsupported(ID.into()))
    }
}
#[async_trait]
impl LinkResolver for FivesingProvider {
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink> {
        let (kind, id) = fs_link_target(url)?;
        match kind.as_str() {
            "dj" => {
                let playlist = self.playlist_info(&id).await?;
                Ok(ResolvedLink::Playlist(playlist))
            }
            song_type => {
                let json = self.song_info(&id, song_type).await?;
                let track = fs_parse_song_detail(&id, song_type, &json).ok_or(Error::NotFound)?;
                Ok(ResolvedLink::Track(track))
            }
        }
    }
}
impl FivesingProvider {
    /// Playlist metadata from the JSON endpoint. playlist_tracks stays
    /// Unsupported by contract; the 2026-11-18 fixture audit re-probed the
    /// static page and found the 2026-11-17 "JS-rendered, no song data"
    /// evidence stale (the page now carries partial server-side rows), so
    /// re-implementing the upstream HTML parser is left to a later slice.
    async fn playlist_info(&self, id: &str) -> Result<Playlist> {
        let json = self
            .get_json(
                &format!("{PLAYLIST_INFO_API}?id={id}&songfields=ID,user"),
                "playlist info",
            )
            .await?;
        fs_parse_playlist_info(id, &json)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCategory;
    use crate::transport::{HttpMethod, MockTransport, TransportResponse};
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn cleans_highlight_tags_and_entities() {
        assert_eq!(clean_text("<em>晴天</em>"), "晴天");
        assert_eq!(clean_text("A&amp;B"), "A&B");
        assert_eq!(clean_text("  <b>周杰伦</b>  "), "周杰伦");
    }
    #[test]
    fn parses_search_songs_with_type_suffix() {
        let item = json!({
            "songId": 123456,
            "typeEname": "yc",
            "songName": "<em>晴天</em>",
            "singer": "周杰伦",
            "songSize": 4000000,
            "user": {"I": "https://cdn/cover.jpg"}
        });
        let track = fs_parse_search_song(&item).unwrap();
        assert_eq!(track.id.id, "123456|yc");
        assert_eq!(track.title, "晴天");
        assert_eq!(
            track.extra.get("songid").map(String::as_str),
            Some("123456")
        );
        assert!(fs_parse_search_song(&json!({"songId": 1, "typeEname": ""})).is_none());
    }
    #[test]
    fn picks_the_closest_quality_with_backups() {
        let links = vec![
            (
                StreamQuality::Lossless,
                "sq".to_owned(),
                vec!["sq2".to_owned()],
            ),
            (StreamQuality::Low, "lq".to_owned(), Vec::new()),
        ];
        let mut sorted = links.clone();
        sorted.sort_by_key(|(quality, _, _)| std::cmp::Reverse(*quality));
        let picked = sorted
            .iter()
            .find(|(quality, _, _)| *quality <= StreamQuality::High)
            .or_else(|| sorted.first())
            .unwrap();
        assert_eq!(picked.1, "lq", "320k missing: falls back to 128k");
        let picked = sorted
            .iter()
            .find(|(quality, _, _)| *quality <= StreamQuality::Lossless)
            .unwrap();
        assert_eq!(picked.1, "sq");
    }
    #[test]
    fn audio_links_pick_tiers_from_response() {
        let data = json!({
            "code": 1000,
            "data": {
                "squrl": "http://cdn/sq.flac",
                "squrl_backup": "http://cdn/sq2.flac",
                "hqurl": "0",
                "lqurl": "http://cdn/lq.mp3"
            }
        });
        let entries = fs_parse_get_song_url(&data).expect("code 1000");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, StreamQuality::Lossless);
        assert_eq!(entries[0].2, vec!["http://cdn/sq2.flac".to_string()]);
        assert_eq!(entries[1].0, StreamQuality::Low);
    }
    /// Loads a fixture from tests/fixtures/fivesing (see the README there for
    /// the derivation: trimmed verbatim live captures from 2026-11-18 plus
    /// upstream inline-test goldens and clearly marked schema-derived cases).
    macro_rules! fixture {
        ($name:expr) => {
            serde_json::from_str::<Value>(include_str!(concat!(
                "../tests/fixtures/fivesing/",
                $name
            )))
            .expect("fivesing fixture must be valid JSON")
        };
    }
    #[test]
    fn fixture_clean_text_matches_upstream_and_live_vectors() {
        for case in fixture!("clean_text.json").as_array().unwrap() {
            assert_eq!(
                clean_text(case["raw"].as_str().unwrap()),
                case["want"].as_str().unwrap(),
                "case {}",
                case["source"].as_str().unwrap()
            );
        }
    }
    fn track_with(id: &str, extra: &[(&str, &str)]) -> Track {
        let mut map = BTreeMap::new();
        for (key, value) in extra {
            map.insert((*key).to_owned(), (*value).to_owned());
        }
        Track {
            id: TrackId::new(ID, id.to_owned()),
            title: String::new(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: None,
            artwork_url: None,
            isrc: None,
            available_qualities: vec![],
            audio: None,
            extra: map,
        }
    }
    #[test]
    fn fixture_split_track_id_vectors_and_deviation() {
        for case in fixture!("split_id.json").as_array().unwrap() {
            let source = case["source"].as_str().unwrap();
            let extra: Vec<(String, String)> = case["extra"]
                .as_object()
                .map(|object| {
                    object
                        .iter()
                        .map(|(key, value)| {
                            (key.clone(), value.as_str().unwrap_or_default().to_owned())
                        })
                        .collect()
                })
                .unwrap_or_default();
            let extra_ref: Vec<(&str, &str)> = extra
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            let track = track_with(case["id"].as_str().unwrap(), &extra_ref);
            let split = FivesingProvider::split_track_id(&track);
            if case
                .get("musid_dli_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                assert!(split.is_err(), "case {source}");
            } else if case
                .get("musid_dli_deviation")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let (song, kind) = split.expect("deviation pin must stay Ok");
                assert_eq!(
                    song,
                    case["songid"].as_str().unwrap_or_default(),
                    "{source}"
                );
                assert_eq!(
                    kind,
                    case["songtype"].as_str().unwrap_or_default(),
                    "{source}"
                );
            } else {
                let (song, kind) = split.expect("golden case must split");
                assert_eq!(song, case["songid"].as_str().unwrap(), "{source}");
                assert_eq!(kind, case["songtype"].as_str().unwrap(), "{source}");
            }
        }
    }
    #[test]
    fn fixture_link_target_vectors() {
        for case in fixture!("links.json").as_array().unwrap() {
            let source = case["source"].as_str().unwrap();
            let target = fs_link_target(case["link"].as_str().unwrap());
            match case["kind"].as_str() {
                Some(kind) => {
                    let (got_kind, got_id) = target.expect("link must resolve");
                    assert_eq!(got_kind, kind, "{source}");
                    assert_eq!(got_id, case["id"].as_str().unwrap(), "{source}");
                }
                None => assert!(target.is_err(), "non-5sing link must be rejected: {source}"),
            }
        }
    }
    #[test]
    fn fixture_search_songs_parses_live_items() {
        let value = fixture!("search_songs.json");
        let items: Vec<Track> = value["list"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(fs_parse_search_song)
            .collect();
        assert_eq!(items.len(), 2);
        let first = &items[0];
        assert_eq!(first.id.id, "15622111|fc");
        assert_eq!(
            first.title, "晴天",
            "live <em class=\"keyword\"> highlight stripped"
        );
        assert_eq!(first.artists, vec!["Vk".to_string()]);
        // Live items carry only songSize (bytes); duration is the 320kbps
        // estimate: 10462607 * 8 / 320000 = 261 s.
        assert_eq!(first.duration.map(|d| d.as_secs()), Some(261));
        // Live search items carry no `user` object at all, so no cover.
        assert_eq!(first.artwork_url, None);
        assert_eq!(
            first.extra.get("songid").map(String::as_str),
            Some("15622111")
        );
        assert_eq!(first.extra.get("songtype").map(String::as_str), Some("fc"));
        let second = &items[1];
        assert_eq!(second.id.id, "15089953|fc");
        assert_eq!(second.artists, vec!["盒纸".to_string()]);
        assert_eq!(second.duration.map(|d| d.as_secs()), Some(233));
    }
    #[test]
    fn fixture_search_songs_drops_incomplete_items() {
        let value = fixture!("search_songs_filtered.json");
        let items: Vec<Track> = value["list"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(fs_parse_search_song)
            .collect();
        assert!(
            items.is_empty(),
            "items missing songId, typeEname or songName must all be dropped"
        );
    }
    #[test]
    fn fixture_search_playlists_parses_live_item() {
        let value = fixture!("search_playlists.json");
        let playlists: Vec<Playlist> = value["list"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(fs_parse_playlist)
            .collect();
        assert_eq!(playlists.len(), 1);
        let playlist = &playlists[0];
        assert_eq!(playlist.id.id, "5c198e848788ade7cbe97542");
        assert_eq!(
            playlist.title, "翻唱",
            "live <em class=\"keyword\"> highlight stripped"
        );
        assert_eq!(playlist.track_count, Some(4));
        assert_eq!(
            playlist.description, None,
            "live content is an empty string"
        );
        assert_eq!(
            playlist.artwork_url.as_deref(),
            Some("http://static.5sing.kugou.com/public/images/app/NEWcover@2x.png")
        );
    }
    #[test]
    fn fixture_song_detail_and_lyric_from_newget() {
        let value = fixture!("song_info.json");
        let track = fs_parse_song_detail("15622111", "fc", &value)
            .expect("live newget data block must parse");
        assert_eq!(track.id.id, "15622111|fc");
        assert_eq!(track.title, "晴天");
        assert_eq!(track.artists, vec!["傲娇少年_Vk".to_string()]);
        assert_eq!(
            track.artwork_url.as_deref(),
            Some("https://wsingbssdl.kugou.com/d78267d58dd6192eb32961d9c6b1f77a.jpg")
        );
        assert_eq!(track.extra.get("songtype").map(String::as_str), Some("fc"));
        // Live dynamicWords is CRLF LRC (63 \r\n lines, zero bare-\n).
        let lyric = fs_lyric_text(&value).expect("live dynamicWords must be Some");
        assert!(lyric.starts_with("[00:03.14]作词：周杰伦"));
        assert!(lyric.contains("\r\n"));
        // The live missing-song shape has an empty-array data block: no title,
        // no lyric.
        let missing = fixture!("song_info_missing.json");
        assert_eq!(fs_lyric_text(&missing), None);
        assert!(fs_parse_song_detail("0", "fc", &missing).is_none());
    }
    #[test]
    fn fixture_get_song_url_tiers_from_live_response() {
        let entries = fs_parse_get_song_url(&fixture!("get_song_url.json"))
            .expect("live getSongUrl answers code 1000");
        assert_eq!(
            entries.len(),
            3,
            "all three tiers present in the live capture"
        );
        assert_eq!(entries[0].0, StreamQuality::Lossless);
        assert_eq!(
            entries[0].1,
            "https://wsaudiobssdlbig.kugou.com/202611180449/7de097966984f81de648b88ca6827b47/bss/extname/wsaudio/e16fe9656014667c92d5c7a158251a0d.mp3"
        );
        assert_eq!(
            entries[0].2,
            vec!["https://wsaudiobssdlbig.cloud.kugou.com/202611180449/8c6b6222a85405d54e8de4f1f15a1762/bss/extname/wsaudio/e16fe9656014667c92d5c7a158251a0d.mp3".to_string()]
        );
        assert_eq!(entries[1].0, StreamQuality::High);
        assert_eq!(entries[2].0, StreamQuality::Low);
        assert_eq!(
            entries[2].2,
            vec!["https://wsaudiobssdlbig.cloud.kugou.com/202611180449/f0fe886812abbb326292f6c0c25cfae5/bss/extname/wsaudio/f9d989ccbb5ac496d548e2686e628aa4.mp3".to_string()]
        );
    }
    #[test]
    fn fixture_get_song_url_sparse_drops_empty_and_zero_tiers() {
        let entries = fs_parse_get_song_url(&fixture!("get_song_url_sparse.json"))
            .expect("schema-derived case answers code 1000");
        // hqurl "" drops the High tier entirely; squrl_backup "0" drops the
        // backup while lqurl_backup keeps its URL (upstream api.rs:284-294).
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, StreamQuality::Lossless);
        assert!(
            entries[0].2.is_empty(),
            "squrl_backup \"0\" must drop the backup"
        );
        assert_eq!(entries[1].0, StreamQuality::Low);
        assert_eq!(entries[1].2.len(), 1);
    }
    #[test]
    fn fixture_get_song_url_gate_rejects_non_1000() {
        let error =
            fs_parse_get_song_url(&fixture!("get_song_url_missing_param.json")).unwrap_err();
        // Renovated pin (this slice, AUTH_REGISTRY_DESIGN.md §9.6): the
        // legacy Provider string "provider fivesing failed: stream url
        // request failed" is replaced by the M2 gate split. The live 1001
        // shape carries the envelope's own refusal declaration
        // ("success": false), so it lands on AuthenticationRequired with the
        // code and the platform message preserved in the detail slot (§1.2).
        assert_eq!(
            error.category(),
            ErrorCategory::AuthenticationRequired,
            "live missing-songid shape ({error}) must hit the code gate"
        );
        assert_eq!(error.provider_id(), Some("fivesing"));
        assert_eq!(error.detail(), Some("stream url: code 1001, 缺少songid"));
        assert!(!error.is_retryable());
        assert_eq!(
            error.to_string(),
            "login required: stream url: code 1001, 缺少songid"
        );
    }
    /// The gate classifier's two arms (AUTH_REGISTRY_DESIGN.md §9.6): the
    /// declared-refusal arm is fixture-pinned above; the off-contract arm is
    /// defensive (no capture shows an undeclared refusal shape) and is
    /// pinned here with synthetic envelopes.
    #[test]
    fn stream_url_gate_splits_declared_refusal_from_off_contract_envelope() {
        // A declared refusal without a message still keys on the flag.
        let error = stream_url_gate_error(&json!({"code": 1002, "success": false}));
        assert_eq!(error.category(), ErrorCategory::AuthenticationRequired);
        assert_eq!(error.provider_id(), Some("fivesing"));
        assert_eq!(error.detail(), Some("stream url: code 1002"));
        assert!(!error.is_retryable());
        // Off-contract envelope (synthetic): Unavailable, retryable.
        let error = stream_url_gate_error(&json!({"code": 500}));
        assert_eq!(error.category(), ErrorCategory::Unavailable);
        assert_eq!(
            error.detail(),
            Some("stream url: code 500, no declared refusal")
        );
        assert!(error.is_retryable());
        // A missing code is likewise off-contract, never a refusal claim.
        let error = stream_url_gate_error(&json!({"error": "boom"}));
        assert_eq!(error.category(), ErrorCategory::Unavailable);
        assert_eq!(
            error.detail(),
            Some("stream url: code missing, no declared refusal")
        );
    }
    #[test]
    fn fixture_playlist_info_parses_live_metadata() {
        let value = fixture!("playlist_info.json");
        let playlist = fs_parse_playlist_info("5c198e848788ade7cbe97542", &value)
            .expect("live getsonglist data block must parse");
        assert_eq!(playlist.id.id, "5c198e848788ade7cbe97542");
        assert_eq!(playlist.title, "翻唱");
        assert_eq!(playlist.track_count, Some(4));
        // The live description is the site's literal placeholder 暂无简介
        // ("no intro yet"): non-empty and != "0", so both implementations
        // keep it as the description.
        assert_eq!(playlist.description.as_deref(), Some("暂无简介"));
        assert_eq!(
            playlist.artwork_url.as_deref(),
            Some("https://5sstatic.kugou.com/public/images/app/NEWcover@2x.png")
        );
    }
    #[test]
    fn fixture_playlist_info_missing_data_array_is_error() {
        let error = fs_parse_playlist_info("00000000", &fixture!("playlist_info_missing.json"))
            .unwrap_err();
        assert!(
            matches!(
                &error,
                Error::Provider { message, .. }
                    if message == "5sing playlist missing or inaccessible"
            ),
            "live missing-playlist shape (data = []) must hit the empty-array guard"
        );
    }
    #[test]
    fn cleans_playlist_metadata_shape() {
        let playlist = fs_parse_playlist(&json!({
            "songListId": "5c198e84",
            "title": "<em>翻唱</em>精选",
            "userId": "63056612",
            "songCnt": 4,
            "content": "",
            "pictureUrl": "https://cdn/p.png"
        }))
        .unwrap();
        assert_eq!(playlist.id.id, "5c198e84");
        assert_eq!(playlist.title, "翻唱精选");
        assert_eq!(playlist.track_count, Some(4));
        assert!(playlist.description.is_none());
        assert!(fs_parse_playlist(&json!({"songListId": "x", "title": ""})).is_none());
    }
    #[test]
    fn recognises_fivesing_links() {
        assert_eq!(
            fs_link_target("http://5sing.kugou.com/yc/123456.html").unwrap(),
            ("yc".into(), "123456".into())
        );
        assert_eq!(
            fs_link_target("http://5sing.kugou.com/12345/dj/abc123.html").unwrap(),
            ("dj".into(), "abc123".into())
        );
        assert!(fs_link_target("https://www.kugou.com/song/").is_err());
    }
    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = FivesingProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.lyrics);
        assert!(descriptor.capabilities.playlists);
        assert!(descriptor.capabilities.link_resolve);
        // 5sing exposes no login surface to these endpoints.
        assert!(!descriptor.capabilities.authentication);
        assert!(!descriptor.capabilities.albums);
        assert!(!descriptor.capabilities.charts);
        assert!(registry.stream_provider(ID).is_some());
        assert!(registry.playlist_provider(ID).is_some());
        assert!(registry.link_provider(ID).is_some());
    }

    /// The dead AuthenticationProvider impl is now registered (dead-impl
    /// decision, AUTH_REGISTRY_DESIGN.md §9.6): the auth slot is live with an
    /// explicit `AuthCapability::None` facet, the `authentication` bool stays
    /// false, and the constant-LoggedOut state-machine view is reachable
    /// through the slot.
    #[tokio::test]
    async fn auth_slot_is_registered_with_an_explicit_none_facet() {
        let provider = FivesingProvider::new();
        assert_eq!(provider.auth_capability(), AuthCapability::None);
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert_eq!(descriptor.auth, AuthCapability::None);
        assert!(!descriptor.capabilities.authentication);
        let auth = registry.authentication_provider(ID).unwrap();
        assert_eq!(auth.login_status().await.unwrap(), LoginStatus::LoggedOut);
        assert_eq!(auth.validate_session().await.unwrap(), AuthState::LoggedOut);
        auth.logout().await.unwrap();
        assert_eq!(auth.login_status().await.unwrap(), LoginStatus::LoggedOut);
    }

    // ── unified transport migration tests (mock transport, no network):
    // the provider runs against Http::from_shared_mock and the recorded
    // request log pins the migrated request face (TRANSPORT_DESIGN.md §7.8).

    fn mock_json(status: u16, body: &str) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: body.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn get_json_runs_through_send_raw_with_web_identity() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert_eq!(request.method, HttpMethod::Get);
            assert_eq!(
                request.url,
                "http://mobileapi.5sing.kugou.com/song/newget?songid=123&songtype=yc"
            );
            assert!(request.idempotent, "GET defaults to idempotent");
            for (name, value) in [("user-agent", UA_WEB), ("referer", REFERER)] {
                assert!(
                    request
                        .headers
                        .iter()
                        .any(|(key, sent)| key == name && sent == value),
                    "header {name} must ride the request"
                );
            }
            Ok(mock_json(200, r#"{"code":200,"data":{"SN":"晴天"}}"#))
        }));
        let provider = FivesingProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let value = provider
            .get_json(
                "http://mobileapi.5sing.kugou.com/song/newget?songid=123&songtype=yc",
                "song info",
            )
            .await
            .unwrap();
        assert_eq!(value["data"]["SN"], "晴天");
        assert_eq!(mock.recorded().len(), 1, "success path is a single send");
    }

    #[tokio::test]
    async fn get_json_retries_transport_errors_and_5xx_then_succeeds() {
        // Declared upgrade (TRANSPORT_DESIGN.md §5.5 row 3): the single-shot
        // face now retries a transport error and a 503 under the unified
        // ladder and lands the third send.
        let mock = Arc::new(MockTransport::script([
            Err(TransportError::Transport("conn reset".to_owned())),
            Ok(mock_json(503, "")),
            Ok(mock_json(200, r#"{"code":200,"data":{}}"#)),
        ]));
        let provider = FivesingProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let value = provider
            .get_json(
                "http://mobileapi.5sing.kugou.com/song/newget?songid=1&songtype=yc",
                "song info",
            )
            .await
            .unwrap();
        assert_eq!(value["code"], 200);
        let recorded = mock.recorded();
        assert_eq!(
            recorded.len(),
            3,
            "transport error + 503 retried, third send succeeds"
        );
        assert!(recorded.iter().all(|request| request.idempotent));
        for request in &recorded {
            for (name, value) in [("user-agent", UA_WEB), ("referer", REFERER)] {
                assert!(
                    request
                        .headers
                        .iter()
                        .any(|(key, sent)| key == name && sent == value),
                    "the {name} identity must ride every retried attempt"
                );
            }
        }
    }

    #[tokio::test]
    async fn get_json_fatal_status_keeps_the_labelled_error_text() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(404, "gone"))]));
        let provider = FivesingProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider
            .get_json(
                "http://mobileapi.5sing.kugou.com/song/newget?songid=0&songtype=yc",
                "song info",
            )
            .await
            .expect_err("404 must stay terminal");
        assert_eq!(
            error.to_string(),
            "provider fivesing failed: song info: HTTP 404",
            "the classify_http Fatal text keeps the labelled shape; today's \
             reason phrase (\"404 Not Found\") collapses to the numeric status"
        );
        assert_eq!(mock.recorded().len(), 1, "404 never retries");
    }

    #[tokio::test]
    async fn get_json_parse_failure_keeps_the_parse_failed_prefix() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(200, "not json"))]));
        let provider = FivesingProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider
            .get_json(
                "http://search.5sing.kugou.com/home/json?keyword=x",
                "search",
            )
            .await
            .expect_err("malformed body must stay terminal");
        assert!(
            error
                .to_string()
                .starts_with("provider fivesing failed: search: parse failed: "),
            "got: {error}"
        );
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn search_goes_through_the_migrated_get_json() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"list":[{"songId":15622111,"typeEname":"fc","songName":"<em>晴天</em>",
                         "singer":"Vk","songSize":10462607}]}"#,
        ))]));
        let provider = FivesingProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let page = provider
            .search(&SearchQuery::new("晴天"))
            .await
            .expect("search must parse the list envelope");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id.id, "15622111|fc");
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0]
                .url
                .starts_with("http://search.5sing.kugou.com/home/json?keyword="),
            "the search query must reach the migrated face: {}",
            recorded[0].url
        );
    }

    #[tokio::test]
    async fn resolve_routes_audio_links_through_the_migrated_face() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":1000,"data":{"squrl":"https://cdn/sq.mp3",
                "squrl_backup":"https://cdn/sq2.mp3","hqurl":"0","lqurl":""}}"#,
        ))]));
        let provider = FivesingProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let track = track_with("123|yc", &[]);
        let info = provider
            .resolve(&track, &ResolveOptions::default())
            .await
            .expect("code-1000 envelope must resolve");
        assert_eq!(info.url, "https://cdn/sq.mp3");
        assert_eq!(info.quality, StreamQuality::Lossless);
        assert_eq!(info.fallback_urls, vec!["https://cdn/sq2.mp3".to_owned()]);
        // Structured slot (MODEL_ALIGNMENT_DESIGN.md §2.4): the same backup
        // URL with the tier it serves; no headers/size/expiry data exists on
        // the 5sing response, so those stay empty and the legacy slot keeps
        // the bare string. Cross-slot dedup collapses the duplicate.
        assert_eq!(info.fallback_endpoints.len(), 1);
        assert_eq!(info.fallback_endpoints[0].url, "https://cdn/sq2.mp3");
        assert_eq!(
            info.fallback_endpoints[0].quality,
            Some(StreamQuality::Lossless)
        );
        assert!(info.fallback_endpoints[0].headers.is_empty());
        assert_eq!(info.fallback_endpoints[0].content_length, None);
        assert_eq!(info.fallback_endpoints[0].expires_at_unix, None);
        assert_eq!(
            info.ordered_fallback_urls(),
            vec!["https://cdn/sq2.mp3".to_owned()],
            "the dual fill must not duplicate the backup across slots"
        );
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].url,
            "http://mobileapi.5sing.kugou.com/song/getSongUrl?songid=123&songtype=yc",
            "the streaming resolve path must run through the retried get_json face"
        );
    }

    #[tokio::test]
    async fn resolve_stream_url_gate_refusal_is_categorized() {
        // End-to-end gate split (AUTH_REGISTRY_DESIGN.md §9.6): a 200 answer
        // carrying the live 1001 refusal shape surfaces as the categorized
        // AuthenticationRequired error, and the refusal never retries.
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"code":1001,"message":"缺少songid","success":false}"#,
        ))]));
        let provider = FivesingProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let track = track_with("123|yc", &[]);
        let error = provider
            .resolve(&track, &ResolveOptions::default())
            .await
            .expect_err("the declared refusal must surface");
        assert_eq!(error.category(), ErrorCategory::AuthenticationRequired);
        assert_eq!(error.provider_id(), Some("fivesing"));
        assert_eq!(error.detail(), Some("stream url: code 1001, 缺少songid"));
        assert!(!error.is_retryable());
        assert_eq!(
            error.to_string(),
            "login required: stream url: code 1001, 缺少songid"
        );
        assert_eq!(mock.recorded().len(), 1, "a refusal never retries");
    }
}
