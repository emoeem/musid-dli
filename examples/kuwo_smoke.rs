//! Kuwo provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-kuwo --example kuwo_smoke [MODULE...]
//!
//! Modules: search lyrics stream
//! Default (no args): search lyrics.
//!
//! Kuwo's anonymous mobile endpoint serves paid songs as an 11-second
//! placeholder with a mismatched rid; the library rejects those, so the
//! stream module walks the first search hits looking for a free track and
//! reports SKIPPED when none is available anonymously. Lossless requests
//! downgrade to 320k/128k on this endpoint.

use musid_dli::{
    StreamQuality,
    provider::{LyricsProvider, ResolveOptions, SearchQuery, StreamResolver, TrackSearch},
    source_kuwo::KuwoProvider,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let modules: Vec<String> = if args.is_empty() {
        vec!["search".into(), "lyrics".into()]
    } else {
        args
    };
    let provider = KuwoProvider::new();
    let mut failed = Vec::new();
    for module in &modules {
        print!("== {module}: ");
        match run_module(&provider, module).await {
            Ok(Outcome::Ok) => println!("OK"),
            Ok(Outcome::Skipped(reason)) => println!("SKIPPED: {reason}"),
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

enum Outcome {
    Ok,
    Skipped(String),
}

async fn run_module(provider: &KuwoProvider, module: &str) -> Result<Outcome, String> {
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
            Ok(Outcome::Ok)
        }
        "lyrics" => {
            let track = first_search_hit(provider).await?;
            let lyric = provider.lyrics(&track).await.map_err(|e| e.to_string())?;
            match lyric {
                Some(doc) => {
                    println!("{:?}: {} lines", doc.format, doc.lines.len());
                    if let Some(line) = doc.lines.iter().find(|l| !l.text.trim().is_empty()) {
                        println!("first line: {}", line.text);
                    }
                }
                None => println!("no lyrics for this track"),
            }
            Ok(Outcome::Ok)
        }
        "stream" => {
            let page = provider
                .search(&SearchQuery::new("晴天 周杰伦"))
                .await
                .map_err(|e| e.to_string())?;
            let mut placeholders = 0;
            for track in page.items.iter().take(5) {
                match provider
                    .resolve(
                        track,
                        &ResolveOptions {
                            // Lossless goes through the web-signed playUrl
                            // endpoint where the anonymous mobile endpoint
                            // only returns placeholders.
                            quality: StreamQuality::Lossless,
                        },
                    )
                    .await
                {
                    Ok(stream) => {
                        println!(
                            "track {} quality={:?} format={:?} (lossless tier via web signing)",
                            track.id.id, stream.quality, stream.format
                        );
                        return Ok(Outcome::Ok);
                    }
                    Err(error) if error.to_string().contains("not found") => {
                        placeholders += 1;
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            Ok(Outcome::Skipped(format!(
                "all {placeholders} sampled tracks resolved to placeholder previews (paid content); anonymous free-tier streams would succeed"
            )))
        }
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search lyrics stream"
        )),
    }
}

async fn first_search_hit(provider: &KuwoProvider) -> Result<musid_dli::models::Track, String> {
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
