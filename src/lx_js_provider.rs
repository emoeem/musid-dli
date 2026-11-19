//! LX Music JS bridge provider wrappers: translate [`JsBridgeEngine`]
//! request/response pairs into musid-dli's `TrackSearch`, `StreamResolver`
//! and `LyricsProvider` results.
//!
//! Extraction rules are ported from VoiceFox's `source/src/js/js_source.rs`
//! (search field fallbacks, URL validation, lyric fields, cover extraction);
//! the request/response envelope is the musid-dli contract in
//! docs/LX_JS_BRIDGE.md. The provider owns no JS runtime: it drives any
//! [`JsBridgeEngine`] the host plugs in, so every test below runs against a
//! fake engine. There is no `pic` command in the musid-dli contract, so
//! cover extraction only runs over search items.
//!
//! Not wired here (kept Unsupported by contract, docs/LX_JS_BRIDGE.md:41-49):
//! authentication (LX custom sources have no login flow) and
//! charts/albums/playlists.
use crate::lrc::parse_lrc;
use crate::lx_js::{JsBridgeEngine, JsRequest, lx_quality, quality_from_lx, response_value};
use crate::models::{AudioFormat, LyricDocument, LyricFormat, StreamInfo, Track, TrackId};
use crate::provider::{
    LyricsProvider, MusicProvider, ProviderCapabilities, ResolveOptions, SearchPage, SearchQuery,
    StreamResolver, TrackSearch,
};
use crate::{Error, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

pub const ID: &str = "lx-js";

/// Bridge provider over a host-supplied engine. The engine is shared
/// (`&self` methods) so one process can serve every trait call.
pub struct JsBridgeProvider {
    engine: Arc<dyn JsBridgeEngine>,
    next_id: AtomicU64,
}

impl JsBridgeProvider {
    pub fn new(engine: Arc<dyn JsBridgeEngine>) -> Self {
        Self {
            engine,
            next_id: AtomicU64::new(1),
        }
    }
    fn call(&self, cmd: &str, params: Value, label: &str) -> Result<Value> {
        let request = JsRequest {
            id: self.next_id.fetch_add(1, Ordering::SeqCst),
            cmd: cmd.to_owned(),
            params,
        };
        let response = self.engine.send_request(&request)?;
        response_value(response, label)
    }
}

impl MusicProvider for JsBridgeProvider {
    fn id(&self) -> &str {
        ID
    }
    fn name(&self) -> &str {
        "LX Music JS source"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            lyrics: true,
            // No login flow, and the contract exposes no container commands
            // (docs/LX_JS_BRIDGE.md trait mapping table).
            ..Default::default()
        }
    }
}

#[async_trait]
impl TrackSearch for JsBridgeProvider {
    async fn search(&self, query: &SearchQuery) -> Result<SearchPage> {
        let keyword = query.keyword.trim();
        if keyword.is_empty() {
            return Err(Error::InvalidInput("search keyword is empty".into()));
        }
        // Contract shape (docs/LX_JS_BRIDGE.md:27) includes "type": "song";
        // hosts whose wrapper ignores it are unaffected.
        let result = self.call(
            "search",
            json!({
                "keyword": keyword,
                "type": "song",
                "page": query.page,
                "limit": query.page_size,
            }),
            "search",
        )?;
        Ok(parse_search_page(&result, query.page_size))
    }
}

/// VoiceFox `parse_js_search_result` (js_source.rs:493-570): the list hides
/// under `list`|`data`|`items`; `total` defaults to the list length; `limit`
/// defaults to the requested page size (VoiceFox's 30 was its own request
/// limit).
fn parse_search_page(result: &Value, page_size: u32) -> SearchPage {
    let Some(list) = result
        .get("list")
        .or_else(|| result.get("data"))
        .or_else(|| result.get("items"))
        .and_then(Value::as_array)
    else {
        return SearchPage {
            items: vec![],
            total: Some(0),
            has_more: false,
        };
    };
    let total = result
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(list.len() as u64);
    let limit = result
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(page_size as u64);
    let items = list
        .iter()
        .filter_map(parse_search_track)
        .collect::<Vec<_>>();
    SearchPage {
        has_more: items.len() >= limit as usize,
        total: Some(total),
        items,
    }
}

/// First non-empty string among `keys` (VoiceFox `get_str`, js_source.rs:399-404).
fn get_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    })
}

/// One search hit. Field fallbacks mirror js_source.rs:517-537; every string
/// and number field is copied into `extra` so hosts keep the source's own
/// payload (VoiceFox stores the same data in `SongInfo.extra`).
fn parse_search_track(item: &Value) -> Option<Track> {
    let song_id = get_str(item, &["songmid", "id", "songId"]).unwrap_or_default();
    if song_id.is_empty() {
        return None;
    }
    let title = get_str(item, &["songname", "name", "title", "songName"]).unwrap_or_default();
    let singer = get_str(item, &["singer", "artist", "author", "singerName"]).unwrap_or_default();
    let album =
        get_str(item, &["albumname", "album", "albumName", "album_name"]).unwrap_or_default();
    let album_id = get_str(item, &["albumid", "albumId", "album_id"]).unwrap_or_default();

    let mut extra = BTreeMap::new();
    extra.insert(
        "source".into(),
        get_str(item, &["source", "platform"])
            .unwrap_or("kw")
            .to_owned(),
    );
    if let Some(object) = item.as_object() {
        for (key, value) in object {
            match value {
                Value::String(text) => {
                    extra.insert(key.clone(), text.clone());
                }
                Value::Number(number) => {
                    extra.insert(key.clone(), number.to_string());
                }
                _ => {}
            }
        }
    }
    Some(Track {
        id: TrackId::new(ID, song_id),
        title: title.to_owned(),
        artists: if singer.is_empty() {
            vec![]
        } else {
            vec![singer.to_owned()]
        },
        album: (!album.is_empty()).then(|| album.to_owned()),
        album_id: (!album_id.is_empty()).then(|| album_id.to_owned()),
        duration: lx_duration(
            item.get("interval")
                .or_else(|| item.get("duration"))
                .or_else(|| item.get("dt")),
        ),
        // LX search items carry no per-track quality list; the source's
        // declared qualities live in the engine's inited state, which the
        // JsBridgeEngine seam does not expose.
        isrc: None,
        artwork_url: extract_named_image_url(item),
        available_qualities: vec![],
        audio: None,
        extra,
    })
}

/// VoiceFox `get_duration` (js_source.rs:470-489): a seconds number or an
/// "m:ss" / "h:mm:ss" string, parsed from the seconds part backwards.
/// Unparseable values mean "unknown" here instead of VoiceFox's zero.
fn lx_duration(value: Option<&Value>) -> Option<Duration> {
    let value = value?;
    if let Some(seconds) = value.as_u64() {
        return Some(Duration::from_secs(seconds));
    }
    let text = value.as_str()?;
    let mut parts = text.split(':').rev();
    let seconds = parts.next().and_then(|part| part.parse::<u64>().ok())?;
    let minutes = parts
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .unwrap_or(0);
    let hours = parts
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .unwrap_or(0);
    Some(Duration::from_secs(hours * 3600 + minutes * 60 + seconds))
}

/// Protocol-relative `//host/...` URLs gain https:; only http(s) counts
/// (VoiceFox `normalize_remote_url`, js_source.rs:407-416).
fn normalize_remote_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.starts_with("//") {
        Some(format!("https:{value}"))
    } else if value.starts_with("http://") || value.starts_with("https://") {
        Some(value.to_string())
    } else {
        None
    }
}

const IMAGE_KEYS: &[&str] = &[
    "pic",
    "img",
    "cover",
    "image",
    "picUrl",
    "coverUrl",
    "imageUrl",
    "albumPic",
    "albumPicUrl",
];

/// VoiceFox `extract_named_image_url` (js_source.rs:418-447): named image
/// keys first, then a recursive scan of objects and arrays.
fn extract_named_image_url(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            for key in IMAGE_KEYS {
                if let Some(url) = object
                    .get(*key)
                    .and_then(Value::as_str)
                    .and_then(normalize_remote_url)
                {
                    return Some(url);
                }
            }
            object.values().find_map(extract_named_image_url)
        }
        Value::Array(values) => values.iter().find_map(extract_named_image_url),
        _ => None,
    }
}

#[async_trait]
impl StreamResolver for JsBridgeProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to the LX JS bridge".into(),
            ));
        }
        let quality = lx_quality(options.quality);
        let result = self.call(
            "musicUrl",
            json!({"songId": track.id.id, "quality": quality}),
            "musicUrl",
        )?;
        let url = extract_stream_url(&result)?;
        Ok(StreamInfo {
            url,
            // The tier we actually asked the engine for: HiRes maps to "flac"
            // (lx_quality), so the reported tier is Lossless, mirroring
            // VoiceFox's report-what-you-sent semantics.
            quality: quality_from_lx(quality),
            format: AudioFormat::Unknown,
            duration: track.duration,
            expires_at_unix: None,
            content_length: None,
            checksum: None,
            headers: response_headers(&result),
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

/// VoiceFox URL extraction (js_source.rs:292-303): `result` as a plain
/// string, then `result.url`, then `result.data.url`; empty or non-http(s)
/// values are not-found, never passed through.
fn extract_stream_url(result: &Value) -> Result<String> {
    let url = result
        .as_str()
        .or_else(|| result.get("url").and_then(Value::as_str))
        .or_else(|| {
            result
                .get("data")
                .and_then(|data| data.get("url"))
                .and_then(Value::as_str)
        })
        .filter(|url| !url.is_empty())
        .ok_or(Error::NotFound)?;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(Error::NotFound);
    }
    Ok(url.to_owned())
}

/// The contract documents `result.headers` as a string map
/// (docs/LX_JS_BRIDGE.md:23); non-string values are skipped.
fn response_headers(result: &Value) -> Vec<(String, String)> {
    result
        .get("headers")
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|text| (key.clone(), text.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait]
impl LyricsProvider for JsBridgeProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>> {
        if track.id.provider != ID {
            return Err(Error::InvalidInput(
                "track does not belong to the LX JS bridge".into(),
            ));
        }
        let result = self.call("lyric", json!({"songId": track.id.id}), "lyric")?;
        Ok(parse_lx_lyric(&result))
    }
}

/// VoiceFox lyric extraction (js_source.rs:335-369): `result.data` or the
/// result root carries `lyric`/`tlyric`/`rlyric`/`lxlyric`. `lxlyric` is LX's
/// enhanced format with no target field in musid-dli's LyricDocument, so it
/// is not invented into one.
fn parse_lx_lyric(result: &Value) -> Option<LyricDocument> {
    let data = result.get("data").unwrap_or(result);
    let original = data
        .get("lyric")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if original.trim().is_empty() {
        return None;
    }
    let translation = data
        .get("tlyric")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let romanization = data
        .get("rlyric")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    Some(LyricDocument {
        format: LyricFormat::Lrc,
        lines: parse_lrc(original),
        original: original.to_owned(),
        translation,
        romanization,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCategory;
    use crate::lx_js::JsResponse;
    use crate::models::StreamQuality;
    use std::sync::Mutex;

    /// Fake engine: records every request and answers through a handler.
    /// Vectors below come from the upstream audit (contract envelope in
    /// docs/LX_JS_BRIDGE.md and VoiceFox response shapes in js_source.rs);
    /// none of them are live captures.
    type Handler = Box<dyn Fn(&JsRequest) -> Result<JsResponse> + Send + Sync>;
    struct FakeEngine {
        handler: Handler,
        seen: Mutex<Vec<JsRequest>>,
    }
    impl FakeEngine {
        fn ok(result: Value) -> Self {
            Self::new(move |request| {
                Ok(JsResponse {
                    id: request.id,
                    ok: true,
                    result: Some(result.clone()),
                    error: None,
                })
            })
        }
        fn new(handler: impl Fn(&JsRequest) -> Result<JsResponse> + Send + Sync + 'static) -> Self {
            Self {
                handler: Box::new(handler),
                seen: Mutex::new(vec![]),
            }
        }
        fn last_request(&self) -> JsRequest {
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last()
                .cloned()
                .expect("engine received no request")
        }
    }
    impl JsBridgeEngine for FakeEngine {
        fn send_request(&self, request: &JsRequest) -> Result<JsResponse> {
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request.clone());
            (self.handler)(request)
        }
    }

    fn make(engine: FakeEngine) -> (JsBridgeProvider, Arc<FakeEngine>) {
        let engine = Arc::new(engine);
        (JsBridgeProvider::new(engine.clone()), engine)
    }

    fn track(id: &str) -> Track {
        Track {
            id: TrackId::new(ID, id),
            title: "title".into(),
            artists: vec![],
            album: None,
            album_id: None,
            duration: Some(Duration::from_secs(200)),
            isrc: None,
            artwork_url: None,
            available_qualities: vec![],
            audio: None,
            extra: BTreeMap::new(),
        }
    }

    const SONG_URL: &str = "https://example.com/kw/320k/song-1.mp3";

    #[tokio::test]
    async fn resolve_translates_the_contract_music_url_vector() {
        // Audit vector: {"id":2,"cmd":"musicUrl","params":{"songId":"song-1",
        // "quality":"320k"}} → {"id":2,"ok":true,"result":{"url": SONG_URL}}.
        let (provider, engine) = make(FakeEngine::ok(json!({ "url": SONG_URL })));
        let stream = provider
            .resolve(&track("song-1"), &ResolveOptions::default())
            .await
            .expect("resolve");
        assert_eq!(stream.url, SONG_URL);
        assert_eq!(stream.quality, StreamQuality::High);
        assert_eq!(stream.duration, Some(Duration::from_secs(200)));
        assert!(stream.headers.is_empty());
        assert_eq!(stream.fallback_urls, Vec::<String>::new());
        let request = engine.last_request();
        assert_eq!(request.cmd, "musicUrl");
        assert_eq!(request.params["songId"], "song-1");
        assert_eq!(request.params["quality"], "320k");
    }

    #[tokio::test]
    async fn resolve_accepts_plain_string_and_nested_data_urls() {
        // VoiceFox extraction order (js_source.rs:292-299): string → .url →
        // .data.url.
        let (provider, _) = make(FakeEngine::ok(json!("https://example.com/plain.mp3")));
        let stream = provider
            .resolve(&track("a"), &ResolveOptions::default())
            .await
            .expect("string result");
        assert_eq!(stream.url, "https://example.com/plain.mp3");

        let (provider, _) = make(FakeEngine::ok(json!({
            "data": {"url": "https://example.com/nested.mp3"}
        })));
        let stream = provider
            .resolve(&track("b"), &ResolveOptions::default())
            .await
            .expect("nested result");
        assert_eq!(stream.url, "https://example.com/nested.mp3");
    }

    #[tokio::test]
    async fn resolve_rejects_empty_and_non_http_urls_as_not_found() {
        for result in [
            json!({}),
            json!({"url": ""}),
            json!("ftp://example.com/a.mp3"),
        ] {
            let (provider, _) = make(FakeEngine::ok(result));
            let error = provider
                .resolve(&track("x"), &ResolveOptions::default())
                .await
                .expect_err("non-http url must be rejected");
            assert!(matches!(error, Error::NotFound), "got {error:?}");
            // Category layer (slice lx-js-error-mapping): the not-found face
            // of extract_stream_url classifies as ErrorCategory::NotFound.
            assert_eq!(error.category(), ErrorCategory::NotFound, "got {error:?}");
        }
    }

    #[tokio::test]
    async fn resolve_maps_quality_tiers_through_lx_strings() {
        for (requested, wire, reported) in [
            (StreamQuality::Lossless, "flac", StreamQuality::Lossless),
            (StreamQuality::HiRes, "flac", StreamQuality::Lossless),
            (StreamQuality::High, "320k", StreamQuality::High),
            (StreamQuality::Low, "128k", StreamQuality::Low),
        ] {
            let (provider, engine) = make(FakeEngine::ok(json!({ "url": SONG_URL })));
            let stream = provider
                .resolve(&track("q"), &ResolveOptions { quality: requested })
                .await
                .expect("resolve");
            assert_eq!(engine.last_request().params["quality"], wire);
            // Report what was actually requested over the wire, not the
            // caller's tier (HiRes rides the flac request).
            assert_eq!(stream.quality, reported);
        }
    }

    #[tokio::test]
    async fn resolve_forwards_contract_headers_from_the_result() {
        let (provider, _) = make(FakeEngine::ok(json!({
            "url": SONG_URL,
            "headers": {"User-Agent": "lx-source/1", "ignored": 5}
        })));
        let stream = provider
            .resolve(&track("h"), &ResolveOptions::default())
            .await
            .expect("resolve");
        assert_eq!(
            stream.headers,
            vec![("User-Agent".into(), "lx-source/1".into())]
        );
    }

    #[tokio::test]
    async fn engine_errors_surface_as_provider_errors() {
        let (provider, _) = make(FakeEngine::new(|request| {
            Ok(JsResponse {
                id: request.id,
                ok: false,
                result: None,
                error: Some("vip required".into()),
            })
        }));
        let error = provider
            .resolve(&track("v"), &ResolveOptions::default())
            .await
            .expect_err("engine error must propagate");
        assert!(error.to_string().contains("vip required"), "got {error:?}");

        // ok:true without a result is a broken envelope, not a success.
        let (provider, _) = make(FakeEngine::new(|request| {
            Ok(JsResponse {
                id: request.id,
                ok: true,
                result: None,
                error: None,
            })
        }));
        let error = provider
            .resolve(&track("v"), &ResolveOptions::default())
            .await
            .expect_err("null result must fail");
        assert!(error.to_string().contains("ok response without result"));
    }

    #[tokio::test]
    async fn foreign_tracks_are_rejected() {
        let (provider, _) = make(FakeEngine::ok(json!({ "url": SONG_URL })));
        let mut foreign = track("netease-1");
        foreign.id.provider = "netease".into();
        for error in [
            provider
                .resolve(&foreign, &ResolveOptions::default())
                .await
                .unwrap_err(),
            provider.lyrics(&foreign).await.unwrap_err(),
        ] {
            assert!(matches!(error, Error::InvalidInput(_)), "got {error:?}");
            // Category layer (slice lx-js-error-mapping): the foreign-track
            // guard classifies as ErrorCategory::InvalidInput.
            assert_eq!(error.category(), ErrorCategory::InvalidInput);
        }
    }

    #[tokio::test]
    async fn search_translates_the_contract_items_page() {
        // Audit vector: {"id":4,"cmd":"search",...} → {"items":[...],
        // "total":100}. Item 1 checks the field fallbacks and the string /
        // number extra copy; item 2 checks the recursive cover vector
        // ({"data":{"cover":{"picUrl":"//host/..."}}} → https://host/...).
        let (provider, engine) = make(FakeEngine::ok(json!({
            "items": [
                {
                    "songmid": "song-1",
                    "name": "晴天",
                    "singer": "歌手",
                    "albumname": "专辑",
                    "albumId": "al-9",
                    "interval": "3:25",
                    "size": 1024
                },
                {
                    "id": "song-2",
                    "title": "标题",
                    "data": {"cover": {"picUrl": "//img.example.com/cover.webp"}}
                }
            ],
            "total": 100
        })));
        let page = provider
            .search(&SearchQuery::new("晴天"))
            .await
            .expect("search");
        assert_eq!(page.total, Some(100));
        assert!(!page.has_more, "2 items < page size 30");
        assert_eq!(page.items.len(), 2);

        let first = &page.items[0];
        assert_eq!(first.id, TrackId::new(ID, "song-1"));
        assert_eq!(first.title, "晴天");
        assert_eq!(first.artists, vec!["歌手"]);
        assert_eq!(first.album.as_deref(), Some("专辑"));
        assert_eq!(first.album_id.as_deref(), Some("al-9"));
        assert_eq!(first.duration, Some(Duration::from_secs(205)));
        // Default source is "kw" (js_source.rs:527) and numbers are stringified.
        assert_eq!(first.extra["source"], "kw");
        assert_eq!(first.extra["size"], "1024");
        assert_eq!(first.extra["songmid"], "song-1");

        let second = &page.items[1];
        assert_eq!(second.id.id, "song-2");
        assert_eq!(second.artists, Vec::<String>::new());
        assert_eq!(
            second.artwork_url.as_deref(),
            Some("https://img.example.com/cover.webp")
        );

        let request = engine.last_request();
        assert_eq!(request.cmd, "search");
        assert_eq!(request.params["keyword"], "晴天");
        // Contract parameter (docs/LX_JS_BRIDGE.md:27).
        assert_eq!(request.params["type"], "song");
        assert_eq!(request.params["page"], 1);
        assert_eq!(request.params["limit"], 30);
    }

    #[tokio::test]
    async fn search_reads_the_voicefox_list_shape_and_fallback_keys() {
        let (provider, _) = make(FakeEngine::ok(json!({
            "list": [{
                "id": "w1",
                "songname": "名",
                "artist": "唱",
                "platform": "wy",
                "album_id": "a1",
                "duration": 245
            }],
            "limit": 1,
            "total": 7
        })));
        let page = provider
            .search(&SearchQuery::new("k"))
            .await
            .expect("search");
        assert_eq!(page.total, Some(7));
        assert!(page.has_more, "1 item >= echoed limit 1");
        let item = &page.items[0];
        assert_eq!(item.id.id, "w1");
        assert_eq!(item.artists, vec!["唱"]);
        assert_eq!(item.album_id.as_deref(), Some("a1"));
        assert_eq!(item.duration, Some(Duration::from_secs(245)));
        assert_eq!(item.extra["source"], "wy");
        assert_eq!(item.extra["platform"], "wy");
    }

    #[tokio::test]
    async fn search_without_a_list_yields_an_empty_page() {
        let (provider, _) = make(FakeEngine::ok(json!({"total": 5})));
        let page = provider
            .search(&SearchQuery::new("k"))
            .await
            .expect("search");
        assert!(page.items.is_empty());
        assert_eq!(page.total, Some(0));
        assert!(!page.has_more);
    }

    #[tokio::test]
    async fn empty_search_keywords_are_invalid_input() {
        let (provider, _) = make(FakeEngine::ok(json!({})));
        let error = provider
            .search(&SearchQuery::new("   "))
            .await
            .expect_err("blank keyword");
        assert!(matches!(error, Error::InvalidInput(_)), "got {error:?}");
        // Category layer (slice lx-js-error-mapping).
        assert_eq!(error.category(), ErrorCategory::InvalidInput);
    }

    /// The bridge's caller-input and not-found faces keep their legacy
    /// `Error::InvalidInput` / `Error::NotFound` variants (the M2 matrix in
    /// AUTH_REGISTRY_DESIGN.md §1.1 has no typed equivalents to migrate to)
    /// and classify through `Error::category()`; pinned here so the mapping
    /// cannot silently drift, together with the declared legacy facts:
    /// no provider slot (§1.3) and non-retryable.
    #[tokio::test]
    async fn bridge_input_and_not_found_faces_carry_stable_categories() {
        let (provider, _) = make(FakeEngine::ok(json!({})));

        let empty_keyword = provider.search(&SearchQuery::new("   ")).await.unwrap_err();
        assert_eq!(empty_keyword.category(), ErrorCategory::InvalidInput);

        let mut foreign = track("netease-1");
        foreign.id.provider = "netease".into();
        let foreign_track = provider
            .resolve(&foreign, &ResolveOptions::default())
            .await
            .unwrap_err();
        assert_eq!(foreign_track.category(), ErrorCategory::InvalidInput);

        let no_url = provider
            .resolve(&track("missing"), &ResolveOptions::default())
            .await
            .unwrap_err();
        assert_eq!(no_url.category(), ErrorCategory::NotFound);

        for error in [&empty_keyword, &foreign_track, &no_url] {
            assert_eq!(
                error.provider_id(),
                None,
                "legacy variants carry no provider slot: {error:?}"
            );
            assert!(!error.is_retryable(), "{error:?}");
            assert!(!error.user_hint().trim().is_empty(), "{error:?}");
        }
    }

    #[tokio::test]
    async fn lyrics_translate_the_contract_vector() {
        // Audit vector: {"id":3,"cmd":"lyric","params":{"songId":"..."}} →
        // {"ok":true,"result":{"lyric":"[00:...]...","tlyric":...}}.
        let (provider, engine) = make(FakeEngine::ok(json!({
            "lyric": "[ti:demo]\n[00:01.00]你好\n[01:02.50]世界",
            "tlyric": "[00:01.00]hello\n[01:02.50]world",
            "rlyric": null,
            "lxlyric": "enhanced payload"
        })));
        let document = provider
            .lyrics(&track("song-3"))
            .await
            .expect("lyrics")
            .expect("lyric text present");
        assert_eq!(document.format, LyricFormat::Lrc);
        assert_eq!(
            document.original,
            "[ti:demo]\n[00:01.00]你好\n[01:02.50]世界"
        );
        assert_eq!(
            document.translation.as_deref(),
            Some("[00:01.00]hello\n[01:02.50]world")
        );
        assert_eq!(document.romanization, None);
        assert_eq!(document.lines.len(), 2, "metadata tag skipped");
        assert_eq!(document.lines[0].text, "你好");
        assert_eq!(document.lines[0].start_ms, 1000);

        let request = engine.last_request();
        assert_eq!(request.cmd, "lyric");
        assert_eq!(request.params["songId"], "song-3");
    }

    #[tokio::test]
    async fn lyrics_read_nested_data_and_skip_empty_fields() {
        let (provider, _) = make(FakeEngine::ok(json!({
            "data": {"lyric": "[00:01.00]hi", "tlyric": "", "lxlyric": "x"}
        })));
        let document = provider
            .lyrics(&track("d"))
            .await
            .expect("lyrics")
            .expect("lyric text present");
        assert_eq!(document.original, "[00:01.00]hi");
        assert_eq!(document.translation, None);
        assert_eq!(document.romanization, None);
        assert_eq!(document.lines.len(), 1);
    }

    #[tokio::test]
    async fn lyrics_without_text_are_none() {
        for result in [json!({}), json!({"lyric": "  "}), json!({"tlyric": "only"})] {
            let (provider, _) = make(FakeEngine::ok(result));
            assert!(
                provider.lyrics(&track("n")).await.expect("ok").is_none(),
                "missing lyric text must be None"
            );
        }
    }

    #[test]
    fn capabilities_match_the_contract() {
        let (provider, _) = make(FakeEngine::ok(json!({})));
        let capabilities = provider.capabilities();
        assert!(capabilities.track_search);
        assert!(capabilities.stream_resolve);
        assert!(capabilities.lyrics);
        // No login flow; no container commands (docs/LX_JS_BRIDGE.md:41-49).
        assert!(!capabilities.authentication);
        assert!(!capabilities.playlists);
        assert!(!capabilities.albums);
        assert!(!capabilities.charts);
        assert!(!capabilities.link_resolve);
        assert_eq!(provider.id(), ID);
        assert_eq!(provider.name(), "LX Music JS source");
    }
}
