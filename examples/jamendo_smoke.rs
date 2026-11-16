//! Jamendo provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-jamendo --example jamendo_smoke [MODULE...]
//!
//! Modules: search stream playlist album link
//! Default (no args): search stream.
//!
//! Jamendo is a Creative Commons catalog: no login, no lyrics, and the best
//! available tier (usually FLAC) is served regardless of the requested
//! quality because the files are freely downloadable.

use musid_dli::{
    StreamQuality,
    provider::{
        AlbumProvider, CollectionProvider, LinkResolver, PlaylistProvider, ResolveOptions,
        SearchQuery, StreamResolver, TrackSearch,
    },
    source_jamendo::JamendoProvider,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let modules: Vec<String> = if args.is_empty() {
        vec!["search".into(), "stream".into()]
    } else {
        args
    };
    let provider = JamendoProvider::new();
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

async fn run_module(provider: &JamendoProvider, module: &str) -> Result<(), String> {
    match module {
        "search" => {
            let page = provider
                .search(&SearchQuery::new("piano"))
                .await
                .map_err(|e| e.to_string())?;
            println!("{} tracks", page.items.len());
            if let Some(track) = page.items.first() {
                println!(
                    "{} - {} [album={:?} qualities={:?}]",
                    track.artists.join("、"),
                    track.title,
                    track.album.as_deref(),
                    track
                        .available_qualities
                        .iter()
                        .map(|q| format!("{q:?}"))
                        .collect::<Vec<_>>()
                        .join("/")
                );
            }
            Ok(())
        }
        "stream" => {
            let page = provider
                .search(&SearchQuery::new("piano"))
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
                        quality: StreamQuality::Lossless,
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            println!("quality={:?} format={:?}", stream.quality, stream.format);
            Ok(())
        }
        "playlist" => {
            let found = provider
                .search_playlists("chill", 1)
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
                "playlist \"{}\": {} tracks fetched",
                detail.title,
                tracks.len()
            );
            Ok(())
        }
        "album" => {
            let page = provider
                .search(&SearchQuery::new("piano"))
                .await
                .map_err(|e| e.to_string())?;
            let track = page
                .items
                .first()
                .ok_or("search returned no tracks")?
                .clone();
            let album_id = track.album_id.ok_or("track has no album id")?;
            let album = provider.album(&album_id).await.map_err(|e| e.to_string())?;
            let tracks = provider
                .album_tracks(&album_id)
                .await
                .map_err(|e| e.to_string())?;
            println!("album \"{}\": {} tracks", album.title, tracks.len());
            Ok(())
        }
        "link" => {
            let track = match provider
                .resolve_link("https://www.jamendo.com/track/1836317")
                .await
                .map_err(|e| e.to_string())?
            {
                musid_dli::provider::ResolvedLink::Track(track) => track,
                other => return Err(format!("expected track, got {other:?}")),
            };
            println!("link track: {}", track.title);
            match provider
                .resolve_link("https://www.jamendo.com/playlist/1")
                .await
            {
                Err(musid_dli::Error::NotFound) => {}
                other => return Err(format!("missing playlist unexpectedly {other:?}")),
            }
            match provider.resolve_link("https://example.com/track/1").await {
                Err(musid_dli::Error::InvalidInput(_)) => {
                    println!("invalid link rejected as expected")
                }
                other => return Err(format!("invalid link produced {other:?}")),
            }
            Ok(())
        }
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search stream playlist album link"
        )),
    }
}
