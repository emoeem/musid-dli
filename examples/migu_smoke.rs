//! Migu provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-migu --example migu_smoke [MODULE...]
//!
//! Modules: search lyrics stream playlist
//! Default (no args): search lyrics.
//!
//! Migu anonymously serves only PQ (128k) audio even when a higher toneFlag
//! is requested: the resolved quality is corrected against the actually
//! served file size, so the stream module prints the achieved quality rather
//! than the requested one.

use musid_dli::{
    StreamQuality,
    provider::{
        CollectionProvider, LyricsProvider, PlaylistProvider, ResolveOptions, SearchQuery,
        StreamResolver, TrackSearch,
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
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search lyrics stream playlist"
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
