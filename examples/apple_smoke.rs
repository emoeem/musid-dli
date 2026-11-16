//! Apple Music provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-apple --example apple_smoke [MODULE...]
//!
//! Modules: search stream lyrics album playlist link
//! Default (no args): search stream.
//!
//! Every stream this provider hands out is a **30-second preview**: Apple
//! full tracks are DRM-protected. The achieved quality is always Low and the
//! track extras carry `preview_only=true`.

use musid_dli::{
    StreamQuality,
    provider::{
        AlbumProvider, CollectionProvider, LinkResolver, LyricsProvider, PlaylistProvider,
        ResolveOptions, SearchQuery, StreamResolver, TrackSearch,
    },
    source_apple::AppleMusicProvider,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let modules: Vec<String> = if args.is_empty() {
        vec!["search".into(), "stream".into()]
    } else {
        args
    };
    let provider = AppleMusicProvider::new();
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

async fn run_module(provider: &AppleMusicProvider, module: &str) -> Result<(), String> {
    match module {
        "search" => {
            let page = provider
                .search(&SearchQuery::new("周杰伦 晴天"))
                .await
                .map_err(|e| e.to_string())?;
            println!("{} tracks", page.items.len());
            if let Some(track) = page.items.first() {
                println!(
                    "{} - {} [album={:?} preview_only={}]",
                    track.artists.join("、"),
                    track.title,
                    track.album.as_deref(),
                    track
                        .extra
                        .get("preview_only")
                        .map(String::as_str)
                        .unwrap_or("-")
                );
            }
            Ok(())
        }
        "stream" => {
            let page = provider
                .search(&SearchQuery::new("周杰伦 晴天"))
                .await
                .map_err(|e| e.to_string())?;
            let track = page
                .items
                .first()
                .ok_or("search returned no tracks")?
                .clone();
            let stream = provider
                .resolve(
                    &track,
                    &ResolveOptions {
                        quality: StreamQuality::Low,
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "quality={:?} format={:?} duration={:?} (30-second preview by contract)",
                stream.quality,
                stream.format,
                stream.duration.map(|d| d.as_secs())
            );
            Ok(())
        }
        "lyrics" => {
            let page = provider
                .search(&SearchQuery::new("周杰伦 晴天"))
                .await
                .map_err(|e| e.to_string())?;
            let track = page
                .items
                .first()
                .ok_or("search returned no tracks")?
                .clone();
            let lyric = provider.lyrics(&track).await.map_err(|e| e.to_string())?;
            match lyric {
                Some(doc) => println!(
                    "{:?}: {} chars of plain text",
                    doc.format,
                    doc.original.len()
                ),
                None => println!("no lyrics available for this storefront"),
            }
            Ok(())
        }
        "album" => {
            // Song search results do not carry the album id; resolve the
            // song detail first (which includes the albums relationship).
            let page = provider
                .search(&SearchQuery::new("周杰伦 晴天"))
                .await
                .map_err(|e| e.to_string())?;
            let song_id = page
                .items
                .first()
                .ok_or("search returned no tracks")?
                .extra
                .get("song_id")
                .cloned()
                .ok_or("track has no song id")?;
            let song_url = format!("https://music.apple.com/us/song/{song_id}");
            let track = match provider
                .resolve_link(&song_url)
                .await
                .map_err(|e| e.to_string())?
            {
                musid_dli::provider::ResolvedLink::Track(track) => track,
                other => return Err(format!("expected track, got {other:?}")),
            };
            let album_id = track.album_id.ok_or("detail track has no album id")?;
            let album = provider.album(&album_id).await.map_err(|e| e.to_string())?;
            let tracks = provider
                .album_tracks(&album_id)
                .await
                .map_err(|e| e.to_string())?;
            println!("album \"{}\": {} tracks", album.title, tracks.len());
            Ok(())
        }
        "playlist" => {
            let found = provider
                .search_playlists("today hits", 1)
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
            println!("playlist \"{}\": {} tracks", detail.title, tracks.len());
            Ok(())
        }
        "link" => {
            let page = provider
                .search(&SearchQuery::new("周杰伦 晴天"))
                .await
                .map_err(|e| e.to_string())?;
            let track = page
                .items
                .first()
                .ok_or("search returned no tracks")?
                .clone();
            let song_id = track.extra.get("song_id").cloned().unwrap_or_default();
            let song_url = format!("https://music.apple.com/us/song/{song_id}");
            match provider
                .resolve_link(&song_url)
                .await
                .map_err(|e| e.to_string())?
            {
                musid_dli::provider::ResolvedLink::Track(resolved) => {
                    println!("link track: {}", resolved.title);
                }
                other => return Err(format!("expected track, got {other:?}")),
            }
            Ok(())
        }
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search stream lyrics album playlist link"
        )),
    }
}
