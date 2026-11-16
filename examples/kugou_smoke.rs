//! Kugou provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-kugou --example kugou_smoke [MODULE...]
//!
//! Modules: search lyrics playlist chart artist stream auth user
//! Default (no args): search lyrics.
//!
//! Kugou refuses stream URLs to anonymous sessions (verified server-side
//! behaviour): the stream module reports SKIPPED with that reason instead of
//! failing. QR login flows are pending; `auth` reports the session status.

#[path = "../src/qr_terminal.rs"]
mod qr_terminal;

use musid_dli::{
    StreamQuality,
    provider::{
        ArtistProvider, AuthenticationProvider, ChartProvider, CollectionProvider, LyricsProvider,
        PlaylistProvider, QrAuthenticationProvider, ResolveOptions, SearchQuery, StreamResolver,
        TrackSearch,
    },
    source_kugou::KugouProvider,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let modules: Vec<String> = if args.is_empty() {
        vec!["search".into(), "lyrics".into()]
    } else {
        args
    };
    let provider = KugouProvider::new();
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

async fn run_module(provider: &KugouProvider, module: &str) -> Result<Outcome, String> {
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
                    println!(
                        "{:?}: {} lines; translation={}; romanization={}; words={}",
                        doc.format,
                        doc.lines.len(),
                        doc.translation.is_some(),
                        doc.romanization.is_some(),
                        doc.lines.iter().any(|l| !l.words.is_empty())
                    );
                    if let Some(line) = doc.lines.iter().find(|l| !l.text.trim().is_empty()) {
                        println!("first line: {}", line.text);
                    }
                }
                None => println!("no lyrics for this track"),
            }
            Ok(Outcome::Ok)
        }
        "playlist" => {
            let cats = provider.categories().await.map_err(|e| e.to_string())?;
            let category = cats
                .iter()
                .find(|c| !c.id.is_empty() && c.id.contains(':'))
                .ok_or("no child playlist category returned")?;
            println!(
                "categories: {} (first real: {} / {})",
                cats.len(),
                category.name,
                category.group.clone().unwrap_or_default()
            );
            let by_tag = provider
                .playlists(&category.id, 1, 5)
                .await
                .map_err(|e| e.to_string())?;
            println!("playlists({}): {}", category.name, by_tag.len());
            let found = provider
                .search_playlists("周杰伦", 1)
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
            Ok(Outcome::Ok)
        }
        "chart" => {
            let charts = provider.charts().await.map_err(|e| e.to_string())?;
            let chart = charts.first().ok_or("chart catalogue is empty")?;
            println!("charts: {} (first: {})", charts.len(), chart.title);
            let entries = provider
                .chart_tracks(&chart.id.id)
                .await
                .map_err(|e| e.to_string())?;
            println!("chart \"{}\": {} entries", chart.title, entries.len());
            for entry in entries.iter().take(3) {
                println!(
                    "  #{} {} - {}",
                    entry.rank,
                    entry.track.artists.join("、"),
                    entry.track.title
                );
            }
            Ok(Outcome::Ok)
        }
        "artist" => {
            // No dedicated Kugou artist endpoint: the name-based search
            // fallback filters tracks to the artist (mirrors upstream).
            let top = provider
                .artist_top_tracks("周杰伦")
                .await
                .map_err(|e| e.to_string())?;
            println!("artist 周杰伦: {} top tracks", top.len());
            for track in top.iter().take(3) {
                println!("  {} - {}", track.artists.join("、"), track.title);
            }
            Ok(Outcome::Ok)
        }
        "stream" => {
            let track = first_search_hit(provider).await?;
            match provider
                .resolve(
                    &track,
                    &ResolveOptions {
                        quality: StreamQuality::High,
                    },
                )
                .await
            {
                Ok(stream) => {
                    println!(
                        "quality={:?} format={:?} bytes={:?}",
                        stream.quality, stream.format, stream.content_length
                    );
                    Ok(Outcome::Ok)
                }
                Err(error) if error.to_string().contains("login") => {
                    // Server-side gate: Kugou hands out no URL to anonymous sessions.
                    Ok(Outcome::Skipped(format!("{error}")))
                }
                Err(error) => Err(error.to_string()),
            }
        }
        "auth" => {
            let qr = provider
                .create_qr_login()
                .await
                .map_err(|e| e.to_string())?;
            println!("QR expires_in={}s; scan with the Kugou app:", qr.expires_in);
            print!("{}", qr_terminal::render(&qr.url));
            match qr_terminal::render_png("kugou_qr.png", &qr.url) {
                Ok(()) => println!(
                    "QR also saved to kugou_qr.png (scan that if the terminal one won't read)"
                ),
                Err(e) => println!("PNG fallback failed: {e}"),
            }
            // Poll every 3 seconds until the phone scan completes.
            let started = std::time::Instant::now();
            let (status, user_id) = loop {
                let poll = provider
                    .poll_qr_login(&qr.key)
                    .await
                    .map_err(|e| e.to_string())?;
                println!("  poll: {:?} ({})", poll.status, poll.message);
                match poll.status {
                    musid_dli::provider::QrLoginStatus::Success
                    | musid_dli::provider::QrLoginStatus::Expired
                    | musid_dli::provider::QrLoginStatus::Failed => {
                        break (poll.status, poll.user_id);
                    }
                    _ => {}
                }
                if started.elapsed() > std::time::Duration::from_secs(180) {
                    return Err("QR poll budget exhausted (180s); scan faster next time".into());
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            };
            if status == musid_dli::provider::QrLoginStatus::Success {
                println!(
                    "logged in as {:?}; run the stream/user modules now",
                    user_id
                );
            }
            Ok(Outcome::Ok)
        }
        "user" => {
            let status = provider.login_status().await.map_err(|e| e.to_string())?;
            match &status {
                musid_dli::provider::LoginStatus::LoggedIn { .. } => {
                    println!("logged in; user playlist APIs are pending");
                    Ok(Outcome::Ok)
                }
                _ => Ok(Outcome::Skipped(
                    "not logged in; set a session cookie first".into(),
                )),
            }
        }
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search lyrics playlist chart artist stream auth user"
        )),
    }
}

async fn first_search_hit(provider: &KugouProvider) -> Result<musid_dli::models::Track, String> {
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
        "{} - {} [album={:?} qualities={:?} hash={}]",
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
            .get("FileHash")
            .map(String::as_str)
            .unwrap_or("-")
    );
}
