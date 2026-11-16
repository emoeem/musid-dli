//! 5sing provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-fivesing --example fivesing_smoke [MODULE...]
//!
//! Modules: search stream lyrics playlist link
//! Default (no args): search stream.

use musid_dli::{
    StreamQuality,
    provider::{
        CollectionProvider, LinkResolver, LyricsProvider, PlaylistProvider, ResolveOptions,
        SearchQuery, StreamResolver, TrackSearch,
    },
    source_fivesing::FivesingProvider,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let modules: Vec<String> = if args.is_empty() {
        vec!["search".into(), "stream".into()]
    } else {
        args
    };
    let provider = FivesingProvider::new();
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

async fn run_module(provider: &FivesingProvider, module: &str) -> Result<(), String> {
    match module {
        "search" => {
            let page = provider
                .search(&SearchQuery::new("晴天"))
                .await
                .map_err(|e| e.to_string())?;
            println!("{} tracks", page.items.len());
            if let Some(track) = page.items.first() {
                println!(
                    "{} - {} [type={}]",
                    track.artists.join("、"),
                    track.title,
                    track
                        .extra
                        .get("songtype")
                        .map(String::as_str)
                        .unwrap_or("-")
                );
            }
            Ok(())
        }
        "stream" => {
            let page = provider
                .search(&SearchQuery::new("晴天"))
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
                        quality: StreamQuality::High,
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "quality={:?} format={:?} fallbacks={}",
                stream.quality,
                stream.format,
                stream.fallback_urls.len()
            );
            Ok(())
        }
        "lyrics" => {
            let page = provider
                .search(&SearchQuery::new("晴天"))
                .await
                .map_err(|e| e.to_string())?;
            let track = page
                .items
                .first()
                .ok_or("search returned no tracks")?
                .clone();
            let lyric = provider.lyrics(&track).await.map_err(|e| e.to_string())?;
            match lyric {
                Some(doc) => println!("{:?}: {} lines", doc.format, doc.lines.len()),
                None => println!("no lyrics for this track"),
            }
            Ok(())
        }
        "playlist" => {
            // Metadata resolves anonymously; the track list is JS-rendered
            // server-side and stays Unsupported by contract.
            let found = provider
                .search_playlists("翻唱", 1)
                .await
                .map_err(|e| e.to_string())?;
            let playlist = found.first().ok_or("playlist search returned nothing")?;
            let detail = provider
                .playlist(&playlist.id.id)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "playlist \"{}\": track_count={:?}",
                detail.title, detail.track_count
            );
            match provider.playlist_tracks(&playlist.id.id).await {
                Err(musid_dli::Error::Unsupported(message)) => {
                    println!("track list correctly Unsupported: {message}");
                    Ok(())
                }
                other => Err(format!("track list produced {other:?}")),
            }
        }
        "link" => {
            let page = provider
                .search(&SearchQuery::new("晴天"))
                .await
                .map_err(|e| e.to_string())?;
            let track = page
                .items
                .first()
                .ok_or("search returned no tracks")?
                .clone();
            let (song_id, song_type) = (
                track.extra.get("songid").cloned().unwrap_or_default(),
                track.extra.get("songtype").cloned().unwrap_or_default(),
            );
            let song_url = format!("http://5sing.kugou.com/{song_type}/{song_id}.html");
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
            "unknown module \"{other}\"; expected one of: search stream lyrics playlist link"
        )),
    }
}
