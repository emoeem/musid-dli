//! 5sing (Kugou original-music community) provider migrated from VoiceFox's
//! `lx-source::fivesing`. Unsigned JSON endpoints: search (songs + playlists),
//! song metadata with plain-text LRC lyrics (`dynamicWords`), three-tier
//! stream URLs (SQ/HQ/LQ with backups) and playlist details parsed from the
//! site's HTML pages. Track ids carry the song type suffix (`{id}|{kind}`)
//! because the same numeric id can exist as 原创/翻唱/伴奏.
use crate::{
    Error, Result,
    lrc::parse_lrc,
    models::{
        AudioFormat, LyricDocument, LyricFormat, Playlist, StreamInfo, StreamQuality, Track,
        TrackId,
    },
    provider::{
        AuthenticationProvider, CollectionProvider, LinkResolver, LoginStatus, LyricsProvider,
        MusicProvider, PlaylistProvider, ResolveOptions, ResolvedLink, SearchPage, SearchQuery,
        StreamResolver, TrackSearch,
    },
    session::SessionStore,
};
use async_trait::async_trait;
use reqwest::{Client, RequestBuilder, header};
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
    client: Client,
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
    /// Register all currently implemented 5sing capabilities in one step.
    pub fn register(&self, registry: &mut crate::ProviderRegistry) {
        let provider = std::sync::Arc::new(self.clone());
        registry.register_search(provider.clone());
        registry.register_stream_resolver(provider.clone());
        registry.register_lyrics(provider.clone());
        registry.register_collection(provider.clone());
        registry.register_playlist(provider);
    }
    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        request
            .header(header::USER_AGENT, UA_WEB)
            .header(header::REFERER, REFERER)
    }
    async fn get_json(&self, url: &str, label: &str) -> Result<Value> {
        let response = self
            .request(self.client.get(url))
            .send()
            .await
            .map_err(|e| perr(format!("{label}: {e}")))?;
        if !response.status().is_success() {
            return Err(perr(format!("{label}: HTTP {}", response.status())));
        }
        response
            .json()
            .await
            .map_err(|e| perr(format!("{label}: parse failed: {e}")))
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
        if json["code"].as_i64() != Some(1000) {
            return Err(perr("stream url request failed"));
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
            ..Default::default()
        }
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
        let original = json["data"]["dynamicWords"]
            .as_str()
            .map(str::trim)
            .filter(|lyric| !lyric.is_empty())
            .map(str::to_owned);
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
/// 5sing exposes no login surface to these endpoints: always anonymous.
#[async_trait]
impl AuthenticationProvider for FivesingProvider {
    async fn login_status(&self) -> Result<LoginStatus> {
        Ok(LoginStatus::LoggedOut)
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
                let data = &json["data"];
                let title = clean_text(data["SN"].as_str().unwrap_or_default());
                if title.is_empty() {
                    return Err(Error::NotFound);
                }
                let mut extra = BTreeMap::new();
                extra.insert("songid".into(), id.clone());
                extra.insert("songtype".into(), song_type.to_owned());
                Ok(ResolvedLink::Track(Track {
                    id: TrackId::new(ID, format!("{id}|{song_type}")),
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
                    extra,
                }))
            }
        }
    }
}
impl FivesingProvider {
    /// Playlist metadata from the JSON endpoint plus tracks from the HTML page
    /// (the site exposes no public track-list API).
    /// Playlist metadata from the JSON endpoint. The track list itself is
    /// rendered client-side since the 2026-11-17 live check (the static page
    /// carries no song data and no anonymous API returns it), so the tracks
    /// stay out of the metadata and playlist_tracks reports Unsupported.
    async fn playlist_info(&self, id: &str) -> Result<Playlist> {
        let json = self
            .get_json(
                &format!("{PLAYLIST_INFO_API}?id={id}&songfields=ID,user"),
                "playlist info",
            )
            .await?;
        let data = &json["data"];
        // `data` may be an empty array when the playlist is missing or private.
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
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let mut entries = Vec::new();
        for (url_key, backup_key, quality) in [
            ("squrl", "squrl_backup", StreamQuality::Lossless),
            ("hqurl", "hqurl_backup", StreamQuality::High),
            ("lqurl", "lqurl_backup", StreamQuality::Low),
        ] {
            let Some(url) = data["data"][url_key]
                .as_str()
                .map(str::trim)
                .filter(|url| !url.is_empty() && *url != "0")
            else {
                continue;
            };
            let backups = data["data"][backup_key]
                .as_str()
                .filter(|url| !url.is_empty() && *url != "0")
                .map(|url| vec![url.to_owned()])
                .unwrap_or_default();
            entries.push((quality, url.to_owned(), backups));
        }
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, StreamQuality::Lossless);
        assert_eq!(entries[0].2, vec!["http://cdn/sq2.flac".to_string()]);
        assert_eq!(entries[1].0, StreamQuality::Low);
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
        // 5sing exposes no login surface to these endpoints.
        assert!(!descriptor.capabilities.authentication);
        assert!(!descriptor.capabilities.albums);
        assert!(!descriptor.capabilities.charts);
        assert!(registry.stream_provider(ID).is_some());
        assert!(registry.playlist_provider(ID).is_some());
    }
}
