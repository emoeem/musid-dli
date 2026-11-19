//! Migu provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-migu --example migu_smoke [MODULE...]
//!
//! Modules: search lyrics stream playlist chart album link
//! Default (no args): search lyrics.
//!
//! Migu anonymously serves only PQ (128k) audio even when a higher toneFlag
//! is requested: the resolved quality is corrected against the actually
//! served file size, so the stream module prints the achieved quality rather
//! than the requested one.

use musid_dli::{
    StreamQuality,
    provider::{
        AlbumProvider, ChartProvider, CollectionProvider, LinkResolver, LyricsProvider,
        PlaylistProvider, ResolveOptions, ResolvedLink, SearchQuery, StreamResolver, TrackSearch,
    },
    source_migu::MiguProvider,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let modules: Vec<String> = if args.is_empty() {
        vec!["search".into(), "lyrics".into()]
    } else {
        args
    };
    let provider = MiguProvider::new();
    let mut failed = Vec::new();
    for module in &modules {
        print!("== {module}: ");
        match run_module(&provider, module).await {
            Ok(()) => println!("OK"),
            Err(error) => {
                println!("FAILED: {error}");
                failed.push(module.clone());
            }
        }
    }
    if !failed.is_empty() {
        println!("failed modules: {}", failed.join(", "));
        std::process::exit(1);
    }
    Ok(())
}

async fn run_module(provider: &MiguProvider, module: &str) -> Result<(), String> {
    match module {
        "search" => {
            let page = provider
                .search(&SearchQuery::new("晴天 周杰伦"))
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "{} tracks; total={:?}; has_more={}",
                page.items.len(),
                page.total,
                page.has_more
            );
            if let Some(track) = page.items.first() {
                print_track(track);
            }
            Ok(())
        }
        "lyrics" => {
            let track = first_search_hit(provider).await?;
            let lyric = provider.lyrics(&track).await.map_err(|e| e.to_string())?;
            match lyric {
                Some(doc) => {
                    println!(
                        "{:?}: {} lines; words={}; translation={}",
                        doc.format,
                        doc.lines.len(),
                        doc.lines.iter().any(|l| !l.words.is_empty()),
                        doc.translation.is_some()
                    );
                    if let Some(line) = doc.lines.iter().find(|l| !l.text.trim().is_empty()) {
                        println!("first line: {}", line.text);
                    }
                }
                None => println!("no lyrics for this track"),
            }
            Ok(())
        }
        "stream" => {
            let track = first_search_hit(provider).await?;
            let stream = provider
                .resolve(
                    &track,
                    &ResolveOptions {
                        quality: StreamQuality::High,
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "requested=High achieved={:?} format={:?} bytes={:?} candidates={}",
                stream.quality,
                stream.format,
                stream.content_length,
                stream.fallback_urls.len()
            );
            Ok(())
        }
        "playlist" => {
            let found = provider
                .search_playlists("华语", 1)
                .await
                .map_err(|e| e.to_string())?;
            let playlist = found.first().ok_or("playlist search returned nothing")?;
            let detail = provider
                .playlist(&playlist.id.id)
                .await
                .map_err(|e| e.to_string())?;
            let tracks = provider
                .playlist_tracks(&playlist.id.id)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "playlist \"{}\": track_count={:?}; fetched {} tracks",
                detail.title,
                detail.track_count,
                tracks.len()
            );
            Ok(())
        }
        "chart" => {
            let charts = provider.charts().await.map_err(|e| e.to_string())?;
            let first = charts.first().ok_or("chart catalogue returned nothing")?;
            println!(
                "{} boards; first: {} ({}{})",
                charts.len(),
                first.title,
                first.id.id,
                first
                    .description
                    .as_deref()
                    .map(|desc| format!(", {desc}"))
                    .unwrap_or_default()
            );
            let entries = provider
                .chart_tracks(&first.id.id)
                .await
                .map_err(|e| e.to_string())?;
            let top = entries.first().ok_or("chart returned no entries")?;
            println!(
                "board \"{}\": {} entries; rank 1: {} - {}",
                first.title,
                entries.len(),
                top.track.artists.join("、"),
                top.track.title
            );
            Ok(())
        }
        "album" => {
            // Migu search rows carry no albumId (the search endpoint returns
            // album names only), so the album module pins one live-verified
            // anonymous album: 10927, 孙燕姿《逆光》, 12 tracks over
            // queryAlbumSong (pageNo > 1 answers an empty data object).
            let album_id = "10927";
            let album = provider.album(album_id).await.map_err(|e| e.to_string())?;
            let tracks = provider
                .album_tracks(album_id)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "album \"{}\" ({}): artists={}; track_count={:?}; fetched {} tracks",
                album.title,
                album.id.id,
                album.artists.join("、"),
                album.track_count,
                tracks.len()
            );
            if let Some(track) = tracks.first() {
                print_track(track);
            }
            Ok(())
        }
        "link" => {
            // Song share link: metadata only — the play URL needs Migu's
            // extra key negotiation and upstream keeps it out of link
            // parsing (mg/parse.rs:106), so no stream is resolved here.
            // The resourceinfo.do route takes the contentId, which search
            // rows carry in extra.
            let hit = first_search_hit(provider).await?;
            let content_id = hit
                .extra
                .get("contentId")
                .ok_or("search hit carries no contentId for the song link")?
                .clone();
            let song_url = format!("https://music.migu.cn/v3/music/song/{content_id}");
            let ResolvedLink::Track(track) = provider
                .resolve_link(&song_url)
                .await
                .map_err(|e| e.to_string())?
            else {
                return Err("song link did not resolve to a track".into());
            };
            println!("song link {song_url} -> \"{}\"", track.title);
            // Playlist share link: metadata + the first 50-row page in one
            // request (upstream get_detail_with_meta shape, never paginated).
            let found = provider
                .search_playlists("华语", 1)
                .await
                .map_err(|e| e.to_string())?;
            let playlist_id = found
                .first()
                .ok_or("playlist search returned nothing")?
                .id
                .id
                .clone();
            let playlist_url = format!("https://music.migu.cn/v3/music/playlist/{playlist_id}");
            let ResolvedLink::Playlist(playlist) = provider
                .resolve_link(&playlist_url)
                .await
                .map_err(|e| e.to_string())?
            else {
                return Err("playlist link did not resolve to a playlist".into());
            };
            println!(
                "playlist link {playlist_url} -> \"{}\": total={:?} tracks on page 1: {}",
                playlist.title,
                playlist.track_count,
                playlist.tracks.len()
            );
            // Album share link: metadata only in this port — the Album model
            // has no tracks slot, the songs stay reachable via album_tracks.
            let album_id = "10927";
            let album_url = format!("https://music.migu.cn/v3/music/album/{album_id}");
            let ResolvedLink::Album(album) = provider
                .resolve_link(&album_url)
                .await
                .map_err(|e| e.to_string())?
            else {
                return Err("album link did not resolve to an album".into());
            };
            println!(
                "album link {album_url} -> \"{}\": artists={}; track_count={:?}",
                album.title,
                album.artists.join("、"),
                album.track_count
            );
            // Unrecognized links must be rejected, not fetched.
            match provider
                .resolve_link("https://example.com/v3/music/song/1")
                .await
            {
                Err(musid_dli::Error::InvalidInput(_)) => {
                    println!("invalid link rejected as expected")
                }
                Err(e) => {
                    return Err(format!(
                        "invalid link produced {e:?}, expected InvalidInput"
                    ));
                }
                Ok(resolved) => {
                    return Err(format!("invalid link unexpectedly resolved: {resolved:?}"));
                }
            }
            Ok(())
        }
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search lyrics stream playlist chart album link"
        )),
    }
}

async fn first_search_hit(provider: &MiguProvider) -> Result<musid_dli::models::Track, String> {
    let page = provider
        .search(&SearchQuery::new("晴天 周杰伦"))
        .await
        .map_err(|e| e.to_string())?;
    page.items
        .into_iter()
        .next()
        .ok_or_else(|| "search returned no tracks".into())
}

fn print_track(track: &musid_dli::models::Track) {
    println!(
        "{} - {} [album={:?} qualities={:?} contentId={}]",
        track.artists.join("、"),
        track.title,
        track.album.as_deref(),
        track
            .available_qualities
            .iter()
            .map(|q| format!("{q:?}"))
            .collect::<Vec<_>>()
            .join("/"),
        track
            .extra
            .get("contentId")
            .map(String::as_str)
            .unwrap_or("-")
    );
}
