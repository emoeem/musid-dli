//! Apple Music provider migrated from VoiceFox's `lx-source::apple`.
//!
//! Contract limits stated up front: Apple Music full tracks are DRM-protected
//! and only **30-second preview URLs** are obtainable (`attributes.previews`).
//! Every StreamInfo this provider hands out is a preview; the capability set
//! reports Low quality only and the docs say so. Search, song/album/playlist
//! metadata, previews and plain-text lyrics come from the unsigned AMP API
//! with a web-embedded JWT token.
use crate::{
    Error, ErrorDetail, Result,
    models::{
        AudioFormat, LyricDocument, LyricFormat, Playlist, PlaylistCategory, StreamInfo,
        StreamQuality, Track, TrackId,
    },
    provider::{
        AlbumProvider, AuthState, AuthenticationProvider, CollectionProvider, LoginStatus,
        LyricsProvider, MusicProvider, PlaylistProvider, ResolveOptions, ResolvedLink, SearchPage,
        SearchQuery, StreamResolver, TrackSearch,
    },
    session::SessionStore,
    transport::{ClientProfile, Http, RetryPolicy, TransportError, TransportRequest},
};
use async_trait::async_trait;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::OnceLock,
    time::{Duration, Instant},
};

const ID: &str = "applemusic";
const UA_WEB: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/143.0 Safari/537.36";
const HOME: &str = "https://music.apple.com";
const AMP: &str = "https://amp-api.music.apple.com";
const STOREFRONT: &str = "us";

#[derive(Clone)]
pub struct AppleMusicProvider {
    http: Http,
    sessions: SessionStore,
    /// Web-embedded JWT, fetched once and probed against a minimal search.
    token: std::sync::Arc<OnceLock<String>>,
    token_fetched_at: std::sync::Arc<std::sync::Mutex<Option<Instant>>>,
}
impl std::fmt::Debug for AppleMusicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppleMusicProvider")
            .field("session_store", &"redacted")
            .finish()
    }
}
impl Default for AppleMusicProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl AppleMusicProvider {
    pub fn new() -> Self {
        // Client construction moved onto the unified transport layer
        // (TRANSPORT_DESIGN.md §4.3): the profile carries exactly today's
        // knobs — 15s total timeout, follow redirects. The build fallback
        // shape lives in `transport::build_client`.
        let http = Http::from_profile(&ClientProfile::provider_default());
        Self {
            http,
            sessions: SessionStore::default(),
            token: std::sync::Arc::new(OnceLock::new()),
            token_fetched_at: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
    /// Test seam: inject a transport handle (a mock in unit tests). The
    /// production constructor is [`AppleMusicProvider::new`]. Test-only so
    /// non-test builds see no dead constructor.
    #[cfg(test)]
    pub(crate) fn with_transport(http: Http) -> Self {
        Self {
            http,
            sessions: SessionStore::default(),
            token: std::sync::Arc::new(OnceLock::new()),
            token_fetched_at: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
    pub fn with_session_store(mut self, sessions: SessionStore) -> Self {
        self.sessions = sessions;
        self
    }
    pub fn session_store(&self) -> SessionStore {
        self.sessions.clone()
    }
    /// Register all currently implemented Apple Music capabilities.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_lyrics(provider.clone());
        registry.register_album(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_link_resolver(provider);
    }
    /// Adds the web identity header to a [`TransportRequest`]. Platform
    /// specific and kept in the provider (TRANSPORT_DESIGN.md §1).
    fn request(&self, request: TransportRequest) -> TransportRequest {
        request.header("user-agent", UA_WEB)
    }
    /// GET one text resource through the unified transport layer.
    ///
    /// Declared behavior upgrade (TRANSPORT_DESIGN.md §5.4/§5.5 row 3,
    /// slice 7): this face was single-shot — no retry loop, so any transport
    /// error, 429/5xx or mid-body reset was terminal. Through `send_raw` it
    /// joins the [`RetryPolicy::standard()`] ladder: transport errors, 429
    /// and 5xx retry up to 4 extra attempts with exponential backoff
    /// (0.5/1/2/4s, 8s cap) + ≤250ms jitter + a server `Retry-After` clamped
    /// to 10s — worst case ≈ 83.5s per call, ≈116s when a `Retry-After`
    /// rides every retry (review P-5 arithmetic). Error texts keep today's
    /// shapes in the M2 detail slot: "{label}: HTTP {status}" for terminal
    /// statuses (the engine's `classify_http` Fatal text is exactly
    /// "HTTP {status}") and "{label}: {error}" for send errors after the
    /// ladder; the landings are typed (`terminal_status_error` /
    /// `network_error`, AUTH_REGISTRY_DESIGN.md §9.6). Body reads are
    /// buffered, so a mid-body connection reset becomes retryable instead of
    /// a terminal `.text()` error (§5.5 row 4).
    async fn raw_text(&self, url: &str, label: &str) -> Result<String> {
        let request = self.request(TransportRequest::get(url));
        let response = match self.http.send_raw(&request, &RetryPolicy::standard()).await {
            Ok(response) => response,
            // classify_http Fatal text is "HTTP {status}", so this keeps
            // today's "{label}: HTTP {status}" shape byte-exact in the
            // detail slot (M2 error migration, AUTH_REGISTRY_DESIGN.md
            // §9.6 apple error sites).
            Err(TransportError::Status(message)) => {
                return Err(terminal_status_error(label, &message));
            }
            Err(error) => return Err(network_error(format!("{label}: {error}"))),
        };
        Ok(response.text().into_owned())
    }
    /// Homepage → script path without regex. The bundle name moved over
    /// time (`index-legacy~…`, `index-legacy-…`, plain `index~…`), so all
    /// known marker shapes are tried before giving up.
    fn script_path(homepage: &str) -> Option<String> {
        const MARKERS: [&str; 3] = ["/assets/index-legacy", "/assets/index~", "/assets/index-"];
        for marker in MARKERS {
            let Some(start) = homepage.find(marker) else {
                continue;
            };
            let tail = &homepage[start..];
            let end = tail.find(['"', '\'']).unwrap_or(tail.len());
            let path = tail[..end].trim_end().to_owned();
            if path.ends_with(".js") {
                return Some(path);
            }
        }
        None
    }
    /// JWT candidates: `eyJ…` tokens of at least 40 JWT-ish characters.
    fn extract_tokens(script: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut from = 0;
        while let Some(pos) = script[from..].find("eyJ") {
            let start = from + pos;
            let token: String = script[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                .collect();
            if token.len() >= 40 {
                tokens.push(token);
            }
            from = start + 3;
        }
        tokens.dedup();
        tokens
    }
    /// Web-embedded JWT crawl: homepage → script → candidate probing.
    ///
    /// Kept in the provider per the slice contract (TRANSPORT_DESIGN.md
    /// §7.7): the crawl chain, the candidate-probing loop and the per-minute
    /// rotation guard are authentication logic, not transport retry. Only
    /// the two text fetches moved — they ride the migrated `raw_text` and
    /// inherit its `standard()` ladder. Each candidate probe stays a
    /// `send_once` single shot outside the retry engine: today every
    /// candidate gets exactly one attempt and any failure just means "not
    /// this candidate"; putting probes on the ladder would turn a bad token
    /// day into a minutes-long crawl.
    async fn token(&self) -> Result<String> {
        if let Some(token) = self.token.get() {
            return Ok(token.clone());
        }
        // Token rotation guard: re-fetch at most once per minute.
        if let Some(fetched) = *self
            .token_fetched_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            && fetched.elapsed() < Duration::from_secs(60)
        {
            return Err(unavailable("Apple Music token unavailable; retry shortly"));
        }
        *self
            .token_fetched_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let homepage = self.raw_text(HOME, "homepage").await?;
        let path = Self::script_path(&homepage)
            .ok_or_else(|| invalid_response("homepage script not found (page layout changed)"))?;
        let script = self.raw_text(&format!("{HOME}/{path}"), "script").await?;
        let candidates = Self::extract_tokens(&script);
        if candidates.is_empty() {
            return Err(invalid_response("script contains no token candidates"));
        }
        // Probe each candidate with a minimal search; a specific song id would
        // 404 on storefront grounds, which says nothing about the token.
        // send_once, not send_raw: one attempt per candidate, whatever the
        // status (auth probing, not transport retry — see the doc comment).
        for candidate in candidates {
            let probe = self
                .http
                .send_once(
                    &self
                        .request(TransportRequest::get(format!(
                            "{AMP}/v1/catalog/{STOREFRONT}/search?term=music&types=songs&limit=1"
                        )))
                        .header("authorization", format!("Bearer {candidate}"))
                        .header("origin", HOME),
                )
                .await;
            if let Ok(response) = probe
                && response.is_success()
            {
                let _ = self.token.set(candidate.clone());
                return Ok(candidate);
            }
        }
        Err(unavailable("no usable Apple Music token found"))
    }
    /// GET one AMP API endpoint through the unified transport layer.
    ///
    /// Same declared upgrade as [`Self::raw_text`]: the single-shot face
    /// joins the [`RetryPolicy::standard()`] ladder (transport errors,
    /// 429/5xx, mid-body resets). Error texts keep today's shapes in the M2
    /// detail slot: "AMP request: HTTP {status}" for terminal statuses and
    /// "AMP request failed: {error}" for send errors after the ladder
    /// (landings typed per §9.6). One declared text drift: parse failures
    /// now carry the serde_json message (buffered-body decode) instead of
    /// reqwest's "error decoding response body: ..." wrapper (§5.5 row 7
    /// class) — they land in `Error::InvalidResponse`. The Bearer/Origin
    /// header group stays provider-side; the query encoding is unchanged.
    async fn amp_get(&self, uri: &str, params: &[(&str, &str)]) -> Result<Value> {
        let token = self.token().await?;
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
            format!("{AMP}{uri}")
        } else {
            format!("{AMP}{uri}?{query}")
        };
        let request = self
            .request(TransportRequest::get(url))
            .header("authorization", format!("Bearer {token}"))
            .header("origin", HOME);
        let response = match self.http.send_raw(&request, &RetryPolicy::standard()).await {
            Ok(response) => response,
            // classify_http Fatal text is "HTTP {status}", so this keeps
            // today's "AMP request: HTTP {status}" shape byte-exact in the
            // detail slot (M2 error migration, AUTH_REGISTRY_DESIGN.md
            // §9.6 apple error sites).
            Err(TransportError::Status(message)) => {
                return Err(terminal_status_error("AMP request", &message));
            }
            Err(error) => return Err(network_error(format!("AMP request failed: {error}"))),
        };
        response
            .json()
            .map_err(|error| invalid_response(format!("AMP response parse failed: {error}")))
    }
    async fn container_tracks(&self, kind: &str, id: &str) -> Result<(Playlist, Vec<Track>)> {
        let json = self
            .amp_get(
                &format!("/v1/catalog/{STOREFRONT}/{kind}s/{id}"),
                &[("include", "tracks"), ("limit[tracks]", "100")],
            )
            .await?;
        let item = json["data"]
            .as_array()
            .and_then(|items| items.first())
            .ok_or(Error::NotFound)?;
        let playlist = apple_parse_playlist(item).ok_or(Error::NotFound)?;
        let tracks = item["relationships"]["tracks"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(apple_parse_song)
            .collect::<Vec<_>>();
        Ok((playlist, tracks))
    }
    async fn fetch_song_detail(&self, id: &str) -> Result<(Track, Option<String>)> {
        // `include=albums` mirrors upstream fetch_song, which asks for
        // `extend=extendedAssetUrls&include=lyrics,albums` (voicefox
        // source/src/apple/api.rs:291-299); lyrics are fetched separately
        // here, so only the albums relationship is needed. Without the
        // include the album id is best-effort, not guaranteed.
        let json = self
            .amp_get(
                &format!("/v1/catalog/{STOREFRONT}/songs/{id}"),
                &[("extend", "extendedAssetUrls"), ("include", "albums")],
            )
            .await?;
        let item = json["data"]
            .as_array()
            .and_then(|items| items.first())
            .ok_or(Error::NotFound)?;
        let mut track = apple_parse_song(item).ok_or(Error::NotFound)?;
        // Song details carry the album id in relationships (search results
        // do not), so album resolution goes through the detail call.
        if let Some(album_id) = apple_detail_album_id(item) {
            track.album_id = Some(album_id);
        }
        let preview = apple_preview_url(item);
        Ok((track, preview))
    }
}
/// The typed `Unavailable` landing (AUTH_REGISTRY_DESIGN.md §9.6 apple
/// error-site migration, jamendo §9.6 shape): the provider id rides the
/// typed slot and the pre-migration refusal text the detail slot, where the
/// legacy `Error::Provider` payload answered `provider_id()` but carried no
/// category. Sites: the per-minute token-rotation guard (the guard's own
/// text says "retry shortly") and the exhausted JWT crawl (every candidate
/// probe failed — a temporary upstream condition, recovered by the next
/// crawl after the guard window, so retryable).
fn unavailable(detail: impl Into<String>) -> Error {
    Error::Unavailable {
        provider: Some(ID.into()),
        detail: Some(ErrorDetail::new(detail)),
    }
}
/// The typed `InvalidResponse` landing: the upstream answered outside its
/// contract. Sites: the homepage no longer contains a known script marker
/// ("page layout changed"), the script carries no JWT candidates, the AMP
/// body is unparseable JSON, and a song detail ships no `previews` node
/// (the module contract makes previews the only obtainable stream — a
/// missing node is a missing field, not an outage). The exact pre-migration
/// text rides the detail slot; the `code` slot stays `None` (HTTP statuses
/// live in the detail text, never in the platform-API-code namespace —
/// joox/qianqian precedent).
fn invalid_response(detail: impl Into<String>) -> Error {
    Error::InvalidResponse {
        provider: Some(ID.into()),
        code: None,
        detail: Some(ErrorDetail::new(detail)),
    }
}
/// The typed `Network` landing for send failures after the standard ladder
/// (connection/DNS/TLS, §1.1). The exact pre-migration text rides the
/// detail slot. Defensive note: the arm also catches `TransportError::Build`
/// (request never went out, transport.rs:203-205), whose text then rides the
/// same slot — apple builds its URLs from constants + urlencoding, so the
/// realistic arm body is post-ladder send failure.
fn network_error(detail: impl Into<String>) -> Error {
    Error::Network {
        provider: Some(ID.into()),
        detail: Some(ErrorDetail::new(detail)),
    }
}
/// The M2 landing for a terminal `classify_http` Fatal status on the two
/// transport faces (the exact shape [`TransportError::Status`] carries:
/// transport.rs:511/520, retry.rs:84/88). Split (migu §9.5 `terminal_status_error`
/// precedent): 429/5xx mean the standard ladder already retried to
/// exhaustion (retry.rs:80-85) — the pipeline stayed down through the whole
/// ladder, so the honest category is [`Error::Unavailable`]. Every other
/// non-success is an unmapped upstream status code (`Error::InvalidResponse`,
/// §1.1 码表外): apple inherits no status table from upstream (its pre-M1
/// folding was the `Error::Provider` catch-all itself, so no NotFound
/// carve-out exists to preserve — the kugou §9.2 no-invented-tables rule),
/// and a stale-token 401 would repeat on resend, so the arm is not
/// retryable. The detail text is always "{label}: HTTP {code}" — byte-exact
/// with the pre-migration message.
fn terminal_status_error(label: &str, text: &str) -> Error {
    let detail = format!("{label}: {text}");
    match text
        .trim()
        .strip_prefix("HTTP ")
        .and_then(|code| code.trim().parse::<u16>().ok())
    {
        Some(code) if code == 429 || (500..600).contains(&code) => unavailable(detail),
        _ => invalid_response(detail),
    }
}
/// Cover templates carry `{w}`/`{h}` placeholders.
fn artwork_url(template: &str, size: u32) -> Option<String> {
    let template = template.trim();
    if template.is_empty() {
        return None;
    }
    Some(
        template
            .replace("{w}", &size.to_string())
            .replace("{h}", &size.to_string()),
    )
}
fn apple_parse_song(item: &Value) -> Option<Track> {
    let id = item["id"].as_str()?.trim().to_owned();
    let attributes = item.get("attributes")?;
    let title = attributes["name"].as_str()?.trim().to_owned();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    let mut extra = BTreeMap::new();
    extra.insert("song_id".into(), id.clone());
    // Every stream handed out is a 30-second preview.
    extra.insert("preview_only".into(), "true".into());
    Some(Track {
        id: TrackId::new(ID, id),
        title,
        artists: vec![
            attributes["artistName"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        ],
        album: attributes["albumName"]
            .as_str()
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        album_id: None,
        duration: {
            let millis = attributes["durationInMillis"].as_u64().unwrap_or_default();
            (millis > 0).then(|| Duration::from_millis(millis))
        },
        artwork_url: attributes["artwork"]["url"]
            .as_str()
            .and_then(|template| artwork_url(template, 600)),
        isrc: None,
        available_qualities: vec![StreamQuality::Low],
        audio: None,
        extra,
    })
}
fn apple_parse_playlist(item: &Value) -> Option<Playlist> {
    let id = item["id"].as_str()?.trim().to_owned();
    let attributes = item.get("attributes")?;
    let title = attributes["name"].as_str()?.trim().to_owned();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(Playlist {
        id: TrackId::new(ID, id),
        title,
        description: attributes["description"]["standard"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        artwork_url: attributes["artwork"]["url"]
            .as_str()
            .and_then(|template| artwork_url(template, 600)),
        track_count: attributes["trackCount"].as_u64().map(|n| n as u32),
        tracks: vec![],
        play_count: None,
        creator: None,
        link: None,
        extra: Default::default(),
    })
}
/// The album id a song-detail resource carries in `relationships.albums`
/// (`data[0].id`, non-empty). Search-result songs do not include it, so the
/// album resolution path has to go through the detail call.
fn apple_detail_album_id(item: &Value) -> Option<String> {
    item["relationships"]["albums"]["data"]
        .as_array()
        .and_then(|albums| albums.first())
        .and_then(|album| album["id"].as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}
/// The 30-second preview URL of a song resource: the first entry of
/// `attributes.previews` with a non-empty `url` (upstream basis: voicefox
/// apple/api.rs:306-312). Absent previews mean "no preview available", never
/// a fallback.
fn apple_preview_url(item: &Value) -> Option<String> {
    item["attributes"]["previews"]
        .as_array()
        .and_then(|previews| previews.first())
        .and_then(|preview| preview["url"].as_str())
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
}
/// Plain-text lyric extraction: `data[0].relationships.lyrics.data[0]
/// attributes.text` (upstream basis: voicefox source/src/apple/api.rs:316-331).
/// The catalog lyrics relationship carries no timing, so the result is never
/// an LRC/QRC document.
fn apple_lyric_text(json: &Value) -> Option<String> {
    json["data"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["relationships"]["lyrics"]["data"].as_array())
        .and_then(|lyrics| lyrics.first())
        .and_then(|lyric| lyric["attributes"]["text"].as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}
/// Link kinds the resolver understands (mirrors upstream `api::AppleLink`,
/// voicefox source/src/apple/api.rs:435-440).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AppleLink {
    Song(String),
    Album(String),
    Playlist(String),
}
/// Pure, offline-testable link recognition. Upstream basis:
/// `voicefox/source/src/apple/api.rs::link_target` (api.rs:442-479).
///
/// - an `?i=` query wins first: it points at the song inside an album link
///   (upstream api.rs:447-450);
/// - the kind is matched as a `/song|album|playlist/` segment, and the id is
///   the LAST non-empty path segment up to `?`/`#` — slug shapes like
///   `/album/ye-hui-mei/1440833097` must yield the numeric id
///   (upstream api.rs:459-468);
/// - a bare numeric string is accepted as a song id (upstream api.rs:475-478).
///
/// musid-dli extension: matching is case-insensitive (`to_ascii_lowercase`
/// preserves byte length, so slices stay aligned), while the id keeps the
/// original casing. Before this fixture batch musid-dli took the FIRST path
/// segment after the kind and ignored `?i=`; both shapes are fixed here.
fn apple_link_target(link: &str) -> Option<AppleLink> {
    let trimmed = link.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(song_id) = query_value(trimmed, "i") {
        return Some(AppleLink::Song(song_id));
    }
    let lower = trimmed.to_ascii_lowercase();
    let kinds = [
        ("song", AppleLink::Song as fn(String) -> AppleLink),
        ("album", AppleLink::Album),
        ("playlist", AppleLink::Playlist),
    ];
    for (segment, build) in kinds {
        let marker = format!("/{segment}/");
        if let Some(index) = lower.find(&marker) {
            let rest = &trimmed[index + marker.len()..];
            let id = rest
                .split(['?', '#'])
                .next()
                .unwrap_or(rest)
                .split('/')
                .rfind(|part| !part.is_empty())
                .unwrap_or_default()
                .trim()
                .to_string();
            if !id.is_empty() {
                return Some(build(id));
            }
        }
    }
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Some(AppleLink::Song(trimmed.to_string()));
    }
    None
}
/// Single query parameter reader (`?` then stop at `#`; empty values ignored),
/// same semantics as upstream `query_value` (voicefox apple/api.rs:481-488).
fn query_value(link: &str, key: &str) -> Option<String> {
    let (_, query) = link.split_once('?')?;
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key && !value.trim().is_empty()).then(|| value.trim().to_string())
    })
}
impl MusicProvider for AppleMusicProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        "Apple Music"
    }
    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            track_search: true,
            // Stream resolution exists, but every URL is a 30-second preview
            // (full tracks are DRM-protected; see the module docs). This is
            // the documented option for the preview-only semantics
            // (AUTH_REGISTRY_DESIGN.md §9.6): resolve() ignores the requested
            // quality tier and always serves the preview at
            // `StreamQuality::Low`, reporting the achieved tier on
            // `StreamInfo.quality`; every parsed track carries
            // `available_qualities == [Low]` and `supported_qualities` below
            // declares the same single tier for the registry descriptor.
            // `UnsupportedQuality` was rejected: the upstream port serves the
            // preview for any requested tier (the pinned resolve test runs
            // with `ResolveOptions::default()`, i.e. High), so failing
            // above-Low requests would break the pinned migration contract,
            // while the achieved tier is already visible to hosts.
            stream_resolve: true,
            lyrics: true,
            playlists: true,
            albums: true,
            link_resolve: true,
            ..Default::default()
        }
    }
    /// The registry descriptor facet carries the same preview-only truth:
    /// the only resolvable tier is Low (the 30-second preview). Empty
    /// ("not declared") would understate a contract the module docs state
    /// up front (AUTH_REGISTRY_DESIGN.md §5.4).
    fn supported_qualities(&self) -> Vec<crate::models::StreamQuality> {
        vec![StreamQuality::Low]
    }
}
#[async_trait]
impl TrackSearch for AppleMusicProvider {
    async fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        if q.keyword.trim().is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        let json = self
            .amp_get(
                &format!("/v1/catalog/{STOREFRONT}/search"),
                &[
                    ("term", q.keyword.trim()),
                    ("types", "songs"),
                    ("limit", "30"),
                ],
            )
            .await?;
        let items = json["results"]["songs"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(apple_parse_song)
            .collect::<Vec<_>>();
        let total = items.len() as u64;
        Ok(SearchPage {
            items,
            total: Some(total),
            has_more: false,
        })
    }
}
/// Streams are the 30-second previews; quality stays Low by contract.
///
/// Resolve-timeout reconciliation (TRANSPORT_DESIGN.md §5.4/§5.5 row 3,
/// slice 7): `resolve` is network-bound — it always runs `fetch_song_detail`
/// through `amp_get` on the `standard()` ladder (worst case ≈ 83.5s per
/// call, ≈116s when a server `Retry-After` rides every retry at the 10s
/// cap). On a cold token cache the JWT crawl runs first and adds two more
/// ≈116s ladders (homepage + script text fetches) plus one 15s single-shot
/// probe per JWT candidate; the probes stay outside the engine. Hosts tuned
/// for today's single-shot apple (≈15s per call, ≈15s + candidates × 15s
/// cold) must raise the resolve-level `per_provider_timeout` (resolve.rs)
/// above that — or pass a reduced `transport_retries` — or the ladder is
/// cancelled mid-flight and masked as "provider resolution timed out" plus
/// a circuit-breaker failure.
#[async_trait]
impl StreamResolver for AppleMusicProvider {
    async fn resolve(&self, track: &Track, _options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to Apple Music".into(),
            ));
        }
        let song_id = track
            .extra
            .get("song_id")
            .cloned()
            .unwrap_or_else(|| track.id.id.clone());
        let (_, preview) = self.fetch_song_detail(&song_id).await?;
        let url = preview.ok_or_else(|| invalid_response("no preview available for this track"))?;
        Ok(StreamInfo {
            url,
            quality: StreamQuality::Low,
            format: AudioFormat::M4a,
            duration: Some(Duration::from_secs(30)),
            expires_at_unix: None,
            content_length: None,
            checksum: None,
            headers: vec![],
            fallback_urls: vec![],
            index_range: None,
            init_range: None,
            max_chunk_size: None,
            size_is_advisory: true,
            checksum_algorithm: None,
            fallback_endpoints: vec![],
        })
    }
}
/// Plain-text lyrics from `relationships.lyrics`; the catalog has no timing.
#[async_trait]
impl LyricsProvider for AppleMusicProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to Apple Music".into(),
            ));
        }
        let song_id = track
            .extra
            .get("song_id")
            .cloned()
            .unwrap_or_else(|| track.id.id.clone());
        let json = self
            .amp_get(
                &format!("/v1/catalog/{STOREFRONT}/songs/{song_id}"),
                &[("include", "lyrics")],
            )
            .await?;
        Ok(apple_lyric_text(&json).map(|original| LyricDocument {
            format: LyricFormat::PlainText,
            lines: vec![],
            original,
            translation: None,
            romanization: None,
        }))
    }
}
#[async_trait]
impl AlbumProvider for AppleMusicProvider {
    async fn album(&self, id: &str) -> Result<crate::models::Album> {
        let (playlist, tracks) = self.container_tracks("album", id).await?;
        Ok(crate::models::Album {
            id: playlist.id,
            title: playlist.title,
            artists: vec![],
            artwork_url: playlist.artwork_url,
            track_count: Some(tracks.len() as u32),
        })
    }
    async fn album_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let (_, tracks) = self.container_tracks("album", id).await?;
        Ok(tracks)
    }
}
#[async_trait]
impl CollectionProvider for AppleMusicProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist> {
        let (playlist, _) = self.container_tracks("playlist", id).await?;
        Ok(playlist)
    }
    async fn playlist_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let (_, tracks) = self.container_tracks("playlist", id).await?;
        Ok(tracks)
    }
}
#[async_trait]
impl crate::provider::LinkResolver for AppleMusicProvider {
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink> {
        // Apple links embed the kind and id; recognition is pure and covered
        // offline by tests/fixtures/apple/link_targets.json.
        match apple_link_target(url).ok_or_else(|| {
            Error::InvalidInput("unrecognized Apple Music song/album/playlist link".into())
        })? {
            AppleLink::Song(id) => {
                let (track, _) = self.fetch_song_detail(&id).await?;
                Ok(ResolvedLink::Track(track))
            }
            AppleLink::Album(id) => {
                let (playlist, tracks) = self.container_tracks("album", &id).await?;
                Ok(ResolvedLink::Album(crate::models::Album {
                    id: playlist.id,
                    title: playlist.title,
                    artists: vec![],
                    artwork_url: playlist.artwork_url,
                    track_count: Some(tracks.len() as u32),
                }))
            }
            AppleLink::Playlist(id) => {
                let (playlist, _) = self.container_tracks("playlist", &id).await?;
                Ok(ResolvedLink::Playlist(playlist))
            }
        }
    }
}
/// Apple exposes no login surface to the catalog API: the auth-state view is
/// the constant `AuthState::LoggedOut` (no session is ever created), and the
/// legacy `LoginStatus` is its explicit fold through `to_login_status`
/// (AUTH_REGISTRY_DESIGN.md §2.3) rather than a hand-written literal.
/// The impl stays unregistered (Apple's `register` adds no auth slot, the
/// `authentication` capability bool stays false): hosts holding the concrete
/// provider get the honest constant-LoggedOut view.
#[async_trait]
impl AuthenticationProvider for AppleMusicProvider {
    async fn login_status(&self) -> Result<LoginStatus> {
        Ok(AuthState::LoggedOut.to_login_status(None))
    }
    async fn logout(&self) -> Result<()> {
        Ok(())
    }
}
/// Apple has no public playlist-search-type endpoint in this shape; search
/// covers songs and the curated categories would need a hardcoded table.
#[async_trait]
impl PlaylistProvider for AppleMusicProvider {
    async fn categories(&self) -> Result<Vec<PlaylistCategory>> {
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
        let json = self
            .amp_get(
                &format!("/v1/catalog/{STOREFRONT}/search"),
                &[
                    ("term", keyword.trim()),
                    ("types", "playlists"),
                    ("limit", "20"),
                ],
            )
            .await?;
        Ok(json["results"]["playlists"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(apple_parse_playlist)
            .collect())
    }
    async fn user_playlists(&self, _page: u32, _page_size: u32) -> Result<Vec<Playlist>> {
        Err(Error::Unsupported(ID.into()))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, MockTransport, TransportResponse};
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn extracts_script_path_without_regex() {
        let homepage = r#"<script src="/assets/index-legacy-abc123.js" crossorigin>"#;
        assert_eq!(
            AppleMusicProvider::script_path(homepage).as_deref(),
            Some("/assets/index-legacy-abc123.js")
        );
        assert!(AppleMusicProvider::script_path("<html></html>").is_none());
    }
    #[test]
    fn extracts_jwt_candidates() {
        let script =
            r#"var a="eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9.extra-token-abc-123-456";var b="short""#;
        let tokens = AppleMusicProvider::extract_tokens(script);
        assert_eq!(tokens.len(), 1);
        assert!(tokens[0].starts_with("eyJ"));
    }
    #[test]
    fn artwork_templates_are_sized() {
        assert_eq!(
            artwork_url("https://is1-ssl.mzstatic.com/x/{w}x{h}.jpg", 600).as_deref(),
            Some("https://is1-ssl.mzstatic.com/x/600x600.jpg")
        );
        assert!(artwork_url("", 600).is_none());
    }
    #[test]
    fn parses_song_attributes() {
        let item = json!({
            "id": "1440833098",
            "attributes": {
                "name": "晴天",
                "artistName": "周杰伦",
                "albumName": "叶惠美",
                "durationInMillis": 269000,
                "artwork": {"url": "https://is1-ssl.mzstatic.com/{w}x{h}.jpg"},
                "previews": [{"url": "https://audio-ssl.itunes.cn/a.m4a"}]
            }
        });
        let track = apple_parse_song(&item).unwrap();
        assert_eq!(track.id.id, "1440833098");
        assert_eq!(track.artists, vec!["周杰伦"]);
        assert_eq!(track.duration.map(|d| d.as_millis()), Some(269000));
        assert_eq!(track.available_qualities, vec![StreamQuality::Low]);
        assert_eq!(
            track.extra.get("preview_only").map(String::as_str),
            Some("true")
        );
        assert!(apple_parse_song(&json!({"id": "1"})).is_none());
    }
    #[test]
    fn parses_playlist_attributes() {
        let item = json!({
            "id": "pl.abc",
            "attributes": {
                "name": "今日热榜",
                "trackCount": 50,
                "artwork": {"url": "https://is1-ssl.mzstatic.com/{w}x{h}.jpg"}
            }
        });
        let playlist = apple_parse_playlist(&item).unwrap();
        assert_eq!(playlist.id.id, "pl.abc");
        assert_eq!(playlist.track_count, Some(50));
    }
    #[tokio::test]
    async fn registration_exposes_implemented_capabilities() {
        let provider = AppleMusicProvider::new();
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        let descriptor = registry.descriptor(ID).unwrap();
        assert!(descriptor.capabilities.track_search);
        // Streams exist but are previews only; the quality contract stays Low.
        assert!(descriptor.capabilities.stream_resolve);
        assert!(descriptor.capabilities.lyrics);
        assert!(descriptor.capabilities.playlists);
        assert!(descriptor.capabilities.albums);
        assert!(!descriptor.capabilities.authentication);
        assert!(registry.stream_provider(ID).is_some());
    }

    /// The preview-only contract is declared on both capability facets
    /// (AUTH_REGISTRY_DESIGN.md §9.6): the descriptor's quality tier list is
    /// the single Low preview tier, and the descriptor aggregate still
    /// carries the unchanged capability bools (the registration test above
    /// pins those).
    #[test]
    fn preview_only_contract_is_declared_on_the_quality_facet() {
        let provider = AppleMusicProvider::new();
        assert_eq!(provider.supported_qualities(), vec![StreamQuality::Low]);
        let mut registry = crate::ProviderRegistry::new();
        provider.register(&mut registry);
        assert_eq!(
            registry.descriptor(ID).unwrap().supported_qualities,
            vec![StreamQuality::Low]
        );
    }

    /// login_status is the explicit `AuthState::LoggedOut` fold (no session
    /// is ever created), and the default `validate_session` derivation stays
    /// inside the state machine.
    #[tokio::test]
    async fn login_status_is_the_explicit_logged_out_state_fold() {
        let provider = AppleMusicProvider::new();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
        assert_eq!(
            provider.validate_session().await.unwrap(),
            AuthState::LoggedOut
        );
        provider.logout().await.unwrap();
        assert_eq!(
            provider.login_status().await.unwrap(),
            LoginStatus::LoggedOut
        );
    }

    // ── fixture-driven tests (see tests/fixtures/apple/README.md) ────────

    /// Loads a reconstructed vector/response fixture from tests/fixtures/apple
    /// (see the README there for the derivation of these shapes and vectors).
    macro_rules! fixture {
        ($name:expr) => {
            serde_json::from_str::<Value>(include_str!(concat!("../tests/fixtures/apple/", $name)))
                .expect("apple fixture must be valid JSON")
        };
    }

    #[test]
    fn fixture_constants_pin_the_amp_endpoints_and_storefront() {
        let fixture = fixture!("token_bootstrap.json");
        let constants = &fixture["source_constants"];
        assert_eq!(HOME, constants["home"].as_str().unwrap());
        assert_eq!(AMP, constants["amp_api"].as_str().unwrap());
        assert_eq!(STOREFRONT, constants["storefront"].as_str().unwrap());
        assert_eq!(UA_WEB, constants["user_agent"].as_str().unwrap());
        // The token probe hits a generic search, never a specific song id
        // (a specific id 404s on storefront grounds; voicefox apple/api.rs:120-137).
        assert!(
            constants["probe_url"]
                .as_str()
                .unwrap()
                .ends_with("/v1/catalog/us/search?term=music&types=songs&limit=1")
        );
    }

    #[test]
    fn token_bootstrap_fixture_covers_script_path_and_jwt_extraction() {
        let fixture = fixture!("token_bootstrap.json");
        for case in fixture["script_path_cases"].as_array().unwrap() {
            let expected = case["expected"].as_str();
            assert_eq!(
                AppleMusicProvider::script_path(case["homepage"].as_str().unwrap()).as_deref(),
                expected,
                "{}",
                case["note"].as_str().unwrap_or("")
            );
        }
        for case in fixture["extract_token_cases"].as_array().unwrap() {
            let expected: Vec<&str> = case["expected"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let tokens = AppleMusicProvider::extract_tokens(case["script"].as_str().unwrap());
            assert_eq!(tokens, expected, "{}", case["note"].as_str().unwrap_or(""));
        }
    }

    #[test]
    fn song_fixture_pins_parse_shape_detail_album_and_the_preview_trap() {
        let fixture = fixture!("song_parse.json");
        let track = apple_parse_song(&fixture["search_item"]).expect("search item must parse");
        let expected = &fixture["expected"];
        assert_eq!(track.id.provider, expected["provider"].as_str().unwrap());
        assert_eq!(track.id.id, expected["id"].as_str().unwrap());
        assert_eq!(
            track.extra.get("song_id").map(String::as_str),
            Some(expected["song_id"].as_str().unwrap())
        );
        assert_eq!(track.title, expected["title"].as_str().unwrap());
        assert_eq!(
            track.artists,
            expected["artists"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            track.album.as_deref(),
            Some(expected["album"].as_str().unwrap())
        );
        assert_eq!(
            track.duration.map(|d| d.as_millis()),
            Some(expected["duration_millis"].as_u64().unwrap() as u128)
        );
        assert_eq!(
            track.artwork_url.as_deref(),
            Some(expected["artwork_600"].as_str().unwrap())
        );
        // Search results carry no album id; only the detail call does.
        assert!(track.album_id.is_none());
        let qualities: Vec<String> = track
            .available_qualities
            .iter()
            .map(|quality| format!("{quality:?}"))
            .collect();
        assert_eq!(
            qualities,
            expected["available_qualities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            track.extra.get("preview_only").map(String::as_str),
            Some(expected["preview_only"].as_str().unwrap())
        );
        // Detail item: relationships.albums + the previews node.
        let detail = &fixture["detail_item"];
        let track = apple_parse_song(detail).unwrap();
        assert_eq!(
            apple_detail_album_id(detail).as_deref(),
            fixture["detail_expected"]["album_id"].as_str()
        );
        assert!(track.album_id.is_none(), "parse does not set album_id");
        assert_eq!(
            apple_preview_url(detail).as_deref(),
            fixture["detail_expected"]["preview_url"].as_str()
        );
        // No previews / empty previews: None, never a fallback.
        assert_eq!(apple_preview_url(&fixture["detail_no_album"]), None);
        assert_eq!(apple_detail_album_id(&fixture["detail_no_album"]), None);
        // Artwork template vectors.
        for case in fixture["artwork_cases"].as_array().unwrap() {
            assert_eq!(
                artwork_url(
                    case["template"].as_str().unwrap(),
                    case["size"].as_u64().unwrap() as u32
                )
                .as_deref(),
                case["expected"].as_str(),
                "{}",
                case["note"].as_str().unwrap_or("")
            );
        }
        for case in fixture["filtered"].as_array().unwrap() {
            assert!(
                apple_parse_song(&case["item"]).is_none(),
                "{}",
                case["note"].as_str().unwrap_or("")
            );
        }
    }

    #[test]
    fn search_fixture_pins_the_results_songs_data_envelope() {
        let fixture = fixture!("search_response.json");
        let response = &fixture["response"];
        let items = response["results"]["songs"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(apple_parse_song)
            .collect::<Vec<_>>();
        let expected_ids: Vec<&str> = fixture["expected"]["parsed_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            items.iter().map(|t| t.id.id.as_str()).collect::<Vec<_>>(),
            expected_ids
        );
        // total is the parsed count and has_more stays false.
        let total = items.len() as u64;
        assert_eq!(
            total,
            fixture["expected"]["total_is_parsed_count"]
                .as_u64()
                .unwrap()
        );
        assert!(!fixture["expected"]["has_more"].as_bool().unwrap());
        // Empty / wrong-shape envelope: empty page, no error.
        let empty = fixture["empty_case"]["response"].clone();
        let items = empty["results"]["songs"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(apple_parse_song)
            .count();
        assert_eq!(items, 0);
    }

    #[test]
    fn playlist_fixture_pins_curator_shape_and_the_search_envelope() {
        let fixture = fixture!("playlist_parse.json");
        let playlist = apple_parse_playlist(&fixture["item"]).expect("playlist item must parse");
        let expected = &fixture["expected"];
        assert_eq!(playlist.id.provider, expected["provider"].as_str().unwrap());
        assert_eq!(playlist.id.id, expected["id"].as_str().unwrap());
        assert_eq!(playlist.title, expected["title"].as_str().unwrap());
        assert_eq!(
            playlist.description.as_deref(),
            expected["description"].as_str()
        );
        assert_eq!(
            playlist.artwork_url.as_deref(),
            expected["artwork_600"].as_str()
        );
        assert_eq!(
            playlist.track_count,
            expected["track_count"].as_u64().map(|n| n as u32)
        );
        // Recorded deviation: a bare-string description parses upstream but
        // is None here (musid-dli reads description.standard only).
        assert!(
            apple_parse_playlist(&fixture["description_fallback_gap"]["item"])
                .unwrap()
                .description
                .is_none()
        );
        // Playlist-search envelope + filtered rows.
        let response = &fixture["playlist_search_response"];
        let items = response["results"]["playlists"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(apple_parse_playlist)
            .collect::<Vec<_>>();
        let expected_ids: Vec<&str> = fixture["playlist_search_expected"]["parsed_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            items.iter().map(|p| p.id.id.as_str()).collect::<Vec<_>>(),
            expected_ids
        );
        // Empty data array is valid.
        let empty = fixture["empty_case"]["response"]["results"]["playlists"]["data"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(empty, 0);
    }

    #[test]
    fn container_fixture_pins_album_and_playlist_tracks() {
        let fixture = fixture!("container_tracks.json");
        let expected = &fixture["expected"];
        // Album: data[0] parses as a playlist-shaped resource; tracks come
        // from relationships.tracks.data with the same song parser.
        let album = &fixture["album_response"];
        let item = album["data"]
            .as_array()
            .and_then(|items| items.first())
            .expect("album container must have data[0]");
        let playlist = apple_parse_playlist(item).unwrap();
        assert_eq!(playlist.title, expected["album_title"].as_str().unwrap());
        assert_eq!(
            playlist.artwork_url.as_deref(),
            expected["album_artwork_600"].as_str()
        );
        let tracks = item["relationships"]["tracks"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(apple_parse_song)
            .collect::<Vec<_>>();
        let expected_ids: Vec<&str> = expected["album_parsed_track_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            tracks.iter().map(|t| t.id.id.as_str()).collect::<Vec<_>>(),
            expected_ids
        );
        // Playlist container: same envelope shape.
        let playlist_item = fixture["playlist_response"]["data"]
            .as_array()
            .and_then(|items| items.first())
            .unwrap();
        let playlist = apple_parse_playlist(playlist_item).unwrap();
        assert_eq!(playlist.title, expected["playlist_title"].as_str().unwrap());
        let tracks = playlist_item["relationships"]["tracks"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(apple_parse_song)
            .collect::<Vec<_>>();
        assert_eq!(
            tracks.len(),
            expected["playlist_parsed_track_ids"]
                .as_array()
                .unwrap()
                .len()
        );
        // Empty / absent data: the container path maps data.first() == None
        // to Error::NotFound (network-bound method; the None is pinned here).
        for case in fixture["empty_cases"].as_array().unwrap() {
            assert!(
                case["response"]["data"]
                    .as_array()
                    .and_then(|items| items.first())
                    .is_none(),
                "{}",
                case["note"].as_str().unwrap_or("")
            );
        }
    }

    #[test]
    fn lyrics_fixture_pins_plain_text_and_every_absent_shape() {
        let fixture = fixture!("lyrics.json");
        let text = apple_lyric_text(&fixture["response"]).expect("golden lyric must parse");
        let expected = &fixture["expected"];
        assert_eq!(text, expected["text"].as_str().unwrap());
        assert_eq!(
            format!("{:?}", LyricFormat::PlainText),
            expected["format"].as_str().unwrap()
        );
        // The document carries no timing: lines stays empty.
        assert_eq!(expected["lines_count"].as_u64().unwrap(), 0);
        assert_eq!(expected["translation"].as_str(), None);
        // Every absent shape yields None (no error).
        for case in fixture["none_cases"].as_array().unwrap() {
            assert!(
                apple_lyric_text(&case["response"]).is_none(),
                "{}",
                case["note"].as_str().unwrap_or("")
            );
        }
    }

    #[test]
    fn link_fixture_pins_song_album_playlist_recognition() {
        let fixture = fixture!("link_targets.json");
        for case in fixture["cases"].as_array().unwrap() {
            let link = case["link"].as_str().unwrap();
            let expected_kind = case["kind"].as_str();
            let expected_id = case["id"].as_str();
            let target = apple_link_target(link);
            match (expected_kind, expected_id) {
                (None, _) => assert!(
                    target.is_none(),
                    "expected rejection for {link:?}: {target:?} ({})",
                    case["note"].as_str().unwrap_or("")
                ),
                (Some(kind), Some(id)) => {
                    let (actual_kind, actual_id) = match target {
                        Some(AppleLink::Song(id)) => ("song", id),
                        Some(AppleLink::Album(id)) => ("album", id),
                        Some(AppleLink::Playlist(id)) => ("playlist", id),
                        None => panic!(
                            "expected {kind}/{id}, got None ({})",
                            case["note"].as_str().unwrap_or("")
                        ),
                    };
                    assert_eq!(actual_kind, kind, "{}", case["note"].as_str().unwrap_or(""));
                    assert_eq!(actual_id, id, "{}", case["note"].as_str().unwrap_or(""));
                }
                _ => unreachable!("fixture cases always pair kind and id"),
            }
        }
    }

    // ── unified transport migration tests (mock transport, no network):
    // the provider runs against Http::from_shared_mock and the recorded
    // request log pins the migrated request face (TRANSPORT_DESIGN.md §7.7).

    fn mock_json(status: u16, body: &str) -> TransportResponse {
        TransportResponse {
            status,
            headers: Vec::new(),
            url: None,
            body: body.as_bytes().to_vec(),
        }
    }

    /// Mock serving the JWT crawl: homepage HTML with a script marker, a
    /// script with two JWT candidates, and a probe endpoint answering
    /// `probe_status`/`probe_body`. Anything else fails loudly as `Build`
    /// (never retried, so a wrong URL cannot burn the ladder).
    fn crawl_mock(probe_status: u16, probe_body: &'static str) -> Arc<MockTransport> {
        Arc::new(MockTransport::new(move |request| {
            if request.url == HOME {
                Ok(mock_json(
                    200,
                    r#"<script src="/assets/index-legacy-abc123.js" crossorigin>"#,
                ))
            } else if request.url == format!("{HOME}//assets/index-legacy-abc123.js") {
                Ok(mock_json(
                    200,
                    r#"var a="eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9.candidate-one-abcdefghij";var b="eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9.candidate-two-klmnopqrstuv""#,
                ))
            } else if request.url.starts_with(AMP) {
                Ok(mock_json(probe_status, probe_body))
            } else {
                Err(TransportError::Build(format!(
                    "unexpected crawl url: {}",
                    request.url
                )))
            }
        }))
    }

    #[tokio::test]
    async fn raw_text_runs_through_send_raw_with_web_identity() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert_eq!(request.method, HttpMethod::Get);
            assert!(request.idempotent, "GET defaults to idempotent");
            assert_eq!(request.url, "https://example.test/page");
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(key, sent)| key == "user-agent" && sent == UA_WEB),
                "the web UA must ride the request"
            );
            Ok(mock_json(200, "<html>ok</html>"))
        }));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let text = provider
            .raw_text("https://example.test/page", "homepage")
            .await
            .unwrap();
        assert_eq!(text, "<html>ok</html>");
        assert_eq!(mock.recorded().len(), 1, "success path is a single send");
    }

    #[tokio::test]
    async fn raw_text_retries_transport_errors_and_5xx_then_succeeds() {
        // Declared upgrade (TRANSPORT_DESIGN.md §5.5 row 3): the single-shot
        // face retries a transport error and a 503 under the unified ladder
        // and lands the third send.
        let mock = Arc::new(MockTransport::script([
            Err(TransportError::Transport("conn reset".to_owned())),
            Ok(mock_json(503, "")),
            Ok(mock_json(200, "recovered")),
        ]));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let text = provider
            .raw_text("https://example.test/x", "script")
            .await
            .unwrap();
        assert_eq!(text, "recovered");
        let recorded = mock.recorded();
        assert_eq!(
            recorded.len(),
            3,
            "transport error + 503 retried, third send succeeds"
        );
        assert!(recorded.iter().all(|request| request.idempotent));
    }

    #[tokio::test]
    async fn raw_text_fatal_status_keeps_today_error_text() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(404, "gone"))]));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider
            .raw_text("https://example.test/x", "homepage")
            .await
            .expect_err("404 must stay terminal");
        // §1.2 refresh (apple error-site migration): the landing moved from
        // the legacy catch-all to `Error::InvalidResponse` (unmapped
        // upstream status, §1.1); the transport-migration text rides the
        // detail slot byte-exact and the Display carries it after the
        // category body.
        assert_eq!(
            error.to_string(),
            "invalid response: homepage: HTTP 404",
            "the classify_http Fatal text keeps the pre-migration shape in the detail slot"
        );
        assert_eq!(error.category(), crate::ErrorCategory::InvalidResponse);
        assert_eq!(error.provider_id(), Some(ID));
        assert_eq!(error.detail(), Some("homepage: HTTP 404"));
        assert!(!error.is_retryable(), "a 404 repeats verbatim on resend");
        assert_eq!(mock.recorded().len(), 1, "404 never retries");
    }

    #[tokio::test]
    async fn amp_get_runs_through_send_raw_with_bearer_and_origin() {
        let mock = Arc::new(MockTransport::new(|request| {
            assert_eq!(request.method, HttpMethod::Get);
            assert!(request.idempotent, "GET defaults to idempotent");
            assert_eq!(
                request.url,
                "https://amp-api.music.apple.com/v1/catalog/us/songs/1440833098?extend=extendedAssetUrls&include=albums"
            );
            for (name, value) in [
                ("user-agent", UA_WEB),
                ("authorization", "Bearer test-jwt"),
                ("origin", HOME),
            ] {
                assert!(
                    request
                        .headers
                        .iter()
                        .any(|(key, sent)| key == name && sent == value),
                    "header {name} must ride the request"
                );
            }
            Ok(mock_json(200, r#"{"data":[{"id":"1440833098"}]}"#))
        }));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        provider
            .token
            .set("test-jwt".to_owned())
            .expect("fresh provider");
        let value = provider
            .amp_get(
                "/v1/catalog/us/songs/1440833098",
                &[("extend", "extendedAssetUrls"), ("include", "albums")],
            )
            .await
            .unwrap();
        assert_eq!(value["data"][0]["id"], "1440833098");
        assert_eq!(
            mock.recorded().len(),
            1,
            "a seeded token skips the crawl entirely"
        );
    }

    #[tokio::test]
    async fn amp_get_retries_5xx_then_succeeds() {
        let mock = Arc::new(MockTransport::script([
            Ok(mock_json(500, "")),
            Ok(mock_json(200, r#"{"data":[{"id":"1"}]}"#)),
        ]));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        provider
            .token
            .set("test-jwt".to_owned())
            .expect("fresh provider");
        let value = provider
            .amp_get("/v1/catalog/us/songs/1", &[])
            .await
            .unwrap();
        assert_eq!(value["data"][0]["id"], "1");
        assert_eq!(
            mock.recorded().len(),
            2,
            "the 500 is retried once under standard()"
        );
    }

    #[tokio::test]
    async fn amp_get_fatal_status_keeps_today_error_text() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(401, "forbidden"))]));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        provider
            .token
            .set("test-jwt".to_owned())
            .expect("fresh provider");
        let error = provider
            .amp_get("/v1/catalog/us/songs/1", &[])
            .await
            .expect_err("401 must stay terminal");
        // §1.2 refresh: unmapped status → `InvalidResponse`, transport text
        // byte-exact in the detail slot (see raw_text_fatal_status test).
        assert_eq!(
            error.to_string(),
            "invalid response: AMP request: HTTP 401",
            "the classify_http Fatal text keeps the pre-migration shape in the detail slot"
        );
        assert_eq!(error.category(), crate::ErrorCategory::InvalidResponse);
        assert_eq!(error.provider_id(), Some(ID));
        assert_eq!(error.detail(), Some("AMP request: HTTP 401"));
        assert!(
            !error.is_retryable(),
            "a stale-token 401 repeats on resend; recovery is the next crawl"
        );
        assert_eq!(mock.recorded().len(), 1, "401 never retries");
    }

    #[tokio::test]
    async fn amp_get_parse_failure_keeps_label_text() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(200, "not json"))]));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        provider
            .token
            .set("test-jwt".to_owned())
            .expect("fresh provider");
        let error = provider
            .amp_get("/v1/catalog/us/songs/1", &[])
            .await
            .expect_err("malformed body must stay terminal");
        // §1.2 refresh: unparseable body → `InvalidResponse` (§1.1), the
        // label text byte-exact in the detail slot.
        assert!(
            error
                .to_string()
                .starts_with("invalid response: AMP response parse failed: "),
            "got: {error}"
        );
        assert_eq!(error.category(), crate::ErrorCategory::InvalidResponse);
        assert_eq!(error.provider_id(), Some(ID));
        assert!(
            error
                .detail()
                .unwrap_or_default()
                .starts_with("AMP response parse failed: "),
            "the label text rides the detail slot: {:?}",
            error.detail()
        );
        assert!(!error.is_retryable());
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn token_crawl_probes_candidates_via_send_once_and_caches() {
        let mock = crawl_mock(200, r#"{"results":{}}"#);
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let token = provider
            .token()
            .await
            .expect("first candidate must probe OK");
        assert!(token.starts_with("eyJ"));
        // One homepage + one script send and exactly ONE probe send (the
        // first candidate succeeded).
        assert_eq!(mock.recorded().len(), 3);
        // The Bearer/Origin/UA group rides the probe (kept provider-side).
        let probe = &mock.recorded()[2];
        assert!(probe.url.starts_with(AMP), "probe hits the AMP search");
        assert!(
            probe
                .headers
                .iter()
                .any(|(key, sent)| key == "authorization" && *sent == format!("Bearer {token}")),
        );
        assert!(
            probe
                .headers
                .iter()
                .any(|(key, sent)| key == "origin" && sent == HOME)
        );
        assert!(
            probe
                .headers
                .iter()
                .any(|(key, sent)| key == "user-agent" && sent == UA_WEB)
        );
        // The token is cached: a second call does not touch the network.
        let again = provider.token().await.expect("cached token");
        assert_eq!(again, token);
        assert_eq!(mock.recorded().len(), 3, "cached token must skip the crawl");
    }

    #[tokio::test]
    async fn token_probe_stays_single_shot_when_every_candidate_fails() {
        // Auth probing is not transport retry (TRANSPORT_DESIGN.md §7.7): a
        // 500 probe must NOT enter the standard() ladder — each candidate
        // gets exactly one send, then the crawl gives up.
        let mock = crawl_mock(500, "");
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider.token().await.expect_err("no candidate probes OK");
        // §1.2 refresh: the exhausted crawl is a temporary upstream
        // condition (the rotation guard re-crawls after its window), so the
        // landing is `Unavailable` (retryable) with the pre-migration text
        // byte-exact in the detail slot.
        assert_eq!(
            error.to_string(),
            "provider unavailable: no usable Apple Music token found"
        );
        assert_eq!(error.category(), crate::ErrorCategory::Unavailable);
        assert_eq!(error.provider_id(), Some(ID));
        assert_eq!(error.detail(), Some("no usable Apple Music token found"));
        assert!(
            error.is_retryable(),
            "the next crawl after the guard window is the recovery"
        );
        let recorded = mock.recorded();
        // homepage + script + exactly one single-shot send per candidate
        // (2), zero retries despite the 500s.
        assert_eq!(recorded.len(), 4, "probes stay outside the retry engine");
        assert!(recorded[2].url.starts_with(AMP) && recorded[3].url.starts_with(AMP));
    }

    #[tokio::test]
    async fn token_rotation_guard_blocks_immediate_recrawl() {
        // The kept per-minute guard: after a failed crawl an immediate retry
        // short-circuits without touching the network.
        let mock = Arc::new(MockTransport::script([Ok(mock_json(404, "gone"))]));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let first = provider.token().await.expect_err("crawl must fail");
        // §1.2 refresh: the 404 crawl error lands `InvalidResponse` (see the
        // raw_text_fatal_status test) with the transport text byte-exact.
        assert_eq!(first.to_string(), "invalid response: homepage: HTTP 404");
        assert_eq!(first.category(), crate::ErrorCategory::InvalidResponse);
        assert_eq!(mock.recorded().len(), 1);
        let second = provider.token().await.expect_err("guard must trip");
        // §1.2 refresh: the guard lands `Unavailable` (retryable) with its
        // pre-migration text byte-exact in the detail slot.
        assert_eq!(
            second.to_string(),
            "provider unavailable: Apple Music token unavailable; retry shortly"
        );
        assert_eq!(second.category(), crate::ErrorCategory::Unavailable);
        assert_eq!(second.provider_id(), Some(ID));
        assert_eq!(
            second.detail(),
            Some("Apple Music token unavailable; retry shortly")
        );
        assert!(second.is_retryable(), "retrying shortly is the recovery");
        assert_eq!(mock.recorded().len(), 1, "the guard must not send anything");
    }

    /// Pure-seam pin for the terminal-status split (migu §9.5 shape: the
    /// seam is pinned without five scripted sends with real backoff sleeps).
    /// 429/5xx exhausted the standard ladder → `Unavailable` (retryable);
    /// every other non-success is an unmapped upstream status →
    /// `InvalidResponse`; a text outside the engine's "HTTP {code}" contract
    /// keeps the same landing with the raw text preserved (defensive —
    /// transport.rs:511/520 only ever produce "HTTP {code}").
    #[test]
    fn terminal_status_error_splits_ladder_exhaustion_from_unmapped_status() {
        for text in ["HTTP 429", "HTTP 500", "HTTP 503"] {
            let error = terminal_status_error("AMP request", text);
            assert_eq!(
                error.category(),
                crate::ErrorCategory::Unavailable,
                "{text}"
            );
            assert_eq!(error.provider_id(), Some(ID));
            assert_eq!(
                error.detail(),
                Some(format!("AMP request: {text}")).as_deref()
            );
            assert!(
                error.is_retryable(),
                "ladder exhaustion recovers by resend, {text}"
            );
            assert_eq!(
                error.to_string(),
                format!("provider unavailable: AMP request: {text}")
            );
        }
        for text in ["HTTP 401", "HTTP 403", "HTTP 404"] {
            let error = terminal_status_error("homepage", text);
            assert_eq!(
                error.category(),
                crate::ErrorCategory::InvalidResponse,
                "{text}"
            );
            assert_eq!(error.provider_id(), Some(ID));
            assert_eq!(error.detail(), Some(format!("homepage: {text}")).as_deref());
            assert!(!error.is_retryable(), "an unmapped status repeats, {text}");
            assert_eq!(
                error.to_string(),
                format!("invalid response: homepage: {text}")
            );
        }
        // Defensive arm: off-contract text keeps the landing and preserves
        // the underlying text (§1.2 "不得静默丢弃").
        let error = terminal_status_error("AMP request", "HTTP 302");
        assert_eq!(error.category(), crate::ErrorCategory::InvalidResponse);
        assert_eq!(error.detail(), Some("AMP request: HTTP 302"));
    }

    /// The send-failure arm lands `Error::Network` (§1.1 connection/DNS/TLS)
    /// with the "{label}: {error}" text byte-exact in the detail slot.
    /// Scripted with a `Build` error because the engine never retries it
    /// (transport.rs:203-205), so the arm is reachable in one send without
    /// burning real backoff sleeps; the realistic post-ladder body is a
    /// `Transport` error, which rides the same arm.
    #[tokio::test]
    async fn raw_text_send_failure_lands_in_network_with_today_text() {
        let mock = Arc::new(MockTransport::script([Err(TransportError::Build(
            "conn refused".to_owned(),
        ))]));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        let error = provider
            .raw_text("https://example.test/x", "script")
            .await
            .expect_err("the send failure must stay terminal");
        assert_eq!(error.to_string(), "network error: script: conn refused");
        assert_eq!(error.category(), crate::ErrorCategory::Network);
        assert_eq!(error.provider_id(), Some(ID));
        assert_eq!(error.detail(), Some("script: conn refused"));
        assert!(error.is_retryable(), "Network is the retryable category");
        assert_eq!(mock.recorded().len(), 1);
    }

    /// A song detail without a `previews` node violates the module contract
    /// ("only 30-second preview URLs are obtainable"): the resolve landing
    /// is `InvalidResponse` with the pre-migration text byte-exact in the
    /// detail slot (the missing-field class, §1.1).
    #[tokio::test]
    async fn resolve_without_previews_lands_in_invalid_response() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"data":[{"id":"1440833098","attributes":{"name":"晴天"}}]}"#,
        ))]));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        provider
            .token
            .set("test-jwt".to_owned())
            .expect("fresh provider");
        let item = json!({"id":"1440833098","attributes":{"name":"晴天"}});
        let track = apple_parse_song(&item).expect("track must parse");
        let error = provider
            .resolve(&track, &ResolveOptions::default())
            .await
            .expect_err("a preview-less detail must fail resolve");
        assert_eq!(
            error.to_string(),
            "invalid response: no preview available for this track"
        );
        assert_eq!(error.category(), crate::ErrorCategory::InvalidResponse);
        assert_eq!(error.provider_id(), Some(ID));
        assert_eq!(error.detail(), Some("no preview available for this track"));
        assert!(!error.is_retryable());
        assert_eq!(mock.recorded().len(), 1);
    }

    #[tokio::test]
    async fn search_goes_through_the_migrated_amp_get() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"results":{"songs":{"data":[{"id":"1440833098","attributes":{"name":"晴天","artistName":"周杰伦"}}]}}}"#,
        ))]));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        provider
            .token
            .set("test-jwt".to_owned())
            .expect("fresh provider");
        let page = provider
            .search(&SearchQuery::new("周杰伦"))
            .await
            .expect("search must parse the AMP envelope");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id.id, "1440833098");
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0]
                .url
                .starts_with("https://amp-api.music.apple.com/v1/catalog/us/search?term="),
            "the search query must reach the migrated face: {}",
            recorded[0].url
        );
    }

    #[tokio::test]
    async fn resolve_serves_the_preview_url_through_the_migrated_face() {
        let mock = Arc::new(MockTransport::script([Ok(mock_json(
            200,
            r#"{"data":[{"id":"1440833098","attributes":{"name":"晴天","previews":[{"url":"https://audio-ssl.itunes.cn/a.m4a"}],"artwork":{"url":"https://is1-ssl.mzstatic.com/{w}x{h}.jpg"}}}]}"#,
        ))]));
        let provider = AppleMusicProvider::with_transport(Http::from_shared_mock(mock.clone()));
        provider
            .token
            .set("test-jwt".to_owned())
            .expect("fresh provider");
        let item = json!({"id":"1440833098","attributes":{"name":"晴天"}});
        let track = apple_parse_song(&item).expect("track must parse");
        let info = provider
            .resolve(&track, &ResolveOptions::default())
            .await
            .expect("resolve must serve the preview");
        assert_eq!(info.url, "https://audio-ssl.itunes.cn/a.m4a");
        assert_eq!(info.quality, StreamQuality::Low);
        assert_eq!(info.format, AudioFormat::M4a);
        assert_eq!(info.duration, Some(Duration::from_secs(30)));
        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1, "one amp_get song-detail call");
        assert!(recorded[0].url.contains("/v1/catalog/us/songs/1440833098"));
    }
}
