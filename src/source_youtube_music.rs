//! Optional YouTube Music provider powered by RustyPipe.
use crate::{
    Error, Result,
    models::{AudioFormat, ByteRange, StreamInfo, StreamQuality, Track, TrackId},
    provider::{
        MusicProvider, ProviderCapabilities, ResolveOptions, SearchPage, SearchQuery,
        StreamResolver, TrackSearch,
    },
};
use async_trait::async_trait;
use rustypipe::{
    client::RustyPipe,
    model::{AudioCodec as YtAudioCodec, AudioFormat as YtAudioFormat, TrackItem},
};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub struct RustyPipeProvider {
    client: Arc<RustyPipe>,
}
impl RustyPipeProvider {
    /// A dedicated storage directory is mandatory: RustyPipe caches client
    /// versions/deobfuscation data and may store authentication material there.
    /// If rustypipe-botguard is on PATH, RustyPipe detects it automatically.
    pub fn new(storage_dir: impl AsRef<Path>) -> Result<Self> {
        Self::build(storage_dir.as_ref().to_path_buf(), None)
    }

    /// Configure an explicit Botguard executable path for browser-client PO tokens.
    pub fn new_with_botguard(
        storage_dir: impl AsRef<Path>,
        botguard_bin: impl Into<OsString>,
    ) -> Result<Self> {
        Self::build(
            storage_dir.as_ref().to_path_buf(),
            Some(botguard_bin.into()),
        )
    }

    fn build(storage_dir: PathBuf, botguard_bin: Option<OsString>) -> Result<Self> {
        fs::create_dir_all(&storage_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&storage_dir, fs::Permissions::from_mode(0o700))?;
        }
        let builder = RustyPipe::builder()
            .storage_dir(storage_dir.clone())
            .botguard_snapshot_file(storage_dir.join("botguard.snapshot"));
        let result = match botguard_bin {
            Some(path) => builder.botguard_bin(path).build(),
            None => builder.build(),
        };
        let client = result.map_err(|error| Error::Provider {
            provider: "youtube-music".into(),
            message: error.to_string(),
        })?;
        Ok(Self {
            client: Arc::new(client),
        })
    }
    fn map_track(item: TrackItem) -> Track {
        let artists = item.artists.into_iter().map(|artist| artist.name).collect();
        let artwork_url = item
            .cover
            .into_iter()
            .max_by_key(|image| image.width)
            .map(|image| image.url);
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("track_type".into(), format!("{:?}", item.track_type));
        let album_id = item.album.as_ref().map(|album| album.id.clone());
        let album = item.album.map(|album| album.name);
        Track {
            id: TrackId::new("youtube-music", item.id),
            title: item.name,
            artists,
            album,
            album_id,
            duration: item
                .duration
                .map(|seconds| Duration::from_secs(seconds as u64)),
            artwork_url,
            isrc: None,
            available_qualities: Vec::new(),
            extra,
        }
    }
}
impl MusicProvider for RustyPipeProvider {
    fn id(&self) -> &str {
        "youtube-music"
    }
    fn name(&self) -> &str {
        "YouTube Music"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            ..Default::default()
        }
    }
}

#[async_trait]
impl TrackSearch for RustyPipeProvider {
    async fn search(&self, query: &SearchQuery) -> Result<SearchPage> {
        let result = self
            .client
            .query()
            .music_search_tracks(&query.keyword)
            .await
            .map_err(|error| Error::Provider {
                provider: self.id().into(),
                message: error.to_string(),
            })?;
        let mut page = result.items;
        for _ in 1..query.page.max(1) {
            let Some(token) = page.ctoken.clone() else {
                break;
            };
            page = self
                .client
                .query()
                .continuation(token, page.endpoint, page.visitor_data.as_deref())
                .await
                .map_err(|error| Error::Provider {
                    provider: self.id().into(),
                    message: error.to_string(),
                })?;
        }
        let total = page.count;
        let has_more = page.ctoken.is_some();
        let items = page
            .items
            .into_iter()
            .take(query.page_size as usize)
            .map(Self::map_track)
            .collect();
        Ok(SearchPage {
            items,
            total,
            has_more,
        })
    }
}

#[async_trait]
impl StreamResolver for RustyPipeProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != self.id() {
            return Err(Error::InvalidInput(
                "track does not belong to YouTube Music".into(),
            ));
        }
        let player = self
            .client
            .query()
            .player(&track.id.id)
            .await
            .map_err(|error| Error::Provider {
                provider: self.id().into(),
                message: error.to_string(),
            })?;
        if player.drm.is_some() {
            return Err(Error::Unsupported("YouTube returned a DRM-protected stream; refusing to expose it as a normal audio URL".into()));
        }
        let mut streams = player
            .audio_streams
            .into_iter()
            .filter(|stream| stream.drm_track_type.is_none() && stream.drm_systems.is_empty())
            .collect::<Vec<_>>();
        if streams.is_empty() {
            return Err(Error::NotFound);
        }
        let target_bitrate = match options.quality {
            StreamQuality::Low => 64_000,
            StreamQuality::Standard => 128_000,
            StreamQuality::High => 192_000,
            StreamQuality::Lossless | StreamQuality::HiRes | StreamQuality::Unknown => u32::MAX,
        };
        streams.sort_by_key(|stream| stream.bitrate.abs_diff(target_bitrate));
        let chosen = streams.remove(0);
        let quality = quality_from_bitrate(chosen.bitrate);
        let format = match (chosen.format, chosen.codec) {
            (YtAudioFormat::M4a, YtAudioCodec::Mp4a) => AudioFormat::Aac,
            (YtAudioFormat::M4a, _) => AudioFormat::M4a,
            (YtAudioFormat::Webm, YtAudioCodec::Opus) => AudioFormat::Opus,
            (YtAudioFormat::Webm, _) => AudioFormat::Unknown,
            _ => AudioFormat::Unknown,
        };
        let index_range = chosen.index_range.map(|range| ByteRange {
            start: range.start as u64,
            end: range.end as u64,
        });
        let init_range = chosen.init_range.map(|range| ByteRange {
            start: range.start as u64,
            end: range.end as u64,
        });
        let fallback_urls = streams.into_iter().map(|stream| stream.url).collect();
        Ok(StreamInfo {
            url: chosen.url,
            quality,
            format,
            duration: chosen
                .duration_ms
                .map(|ms| Duration::from_millis(ms as u64))
                .or_else(|| {
                    (player.details.duration > 0)
                        .then(|| Duration::from_secs(player.details.duration as u64))
                }),
            expires_at_unix: Some(player.valid_until.unix_timestamp().max(0) as u64),
            content_length: Some(chosen.size),
            checksum: None,
            headers: Vec::new(),
            fallback_urls,
            index_range,
            init_range,
            max_chunk_size: None,
        })
    }
}

fn quality_from_bitrate(bitrate: u32) -> StreamQuality {
    match bitrate {
        0..=63_999 => StreamQuality::Low,
        64_000..=127_999 => StreamQuality::Standard,
        _ => StreamQuality::High,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn youtube_music_never_claims_lossless_from_bitrate_alone() {
        assert_eq!(quality_from_bitrate(96_000), StreamQuality::Standard);
        assert_eq!(quality_from_bitrate(256_000), StreamQuality::High);
        assert_eq!(quality_from_bitrate(320_000), StreamQuality::High);
    }
}
