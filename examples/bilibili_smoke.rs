//! Bilibili provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-bilibili --example bilibili_smoke [MODULE...]
//!
//! Modules: search reference stream auth favorites
//! Default (no args): search stream.
//!
//! Anonymous keyword search is currently risk-controlled by Bilibili (the
//! response carries a v_voucher and no results); BV/av references and stream
//! resolution work anonymously, keyword search needs a session cookie.
//!
//! `search` runs a keyword query; `reference` resolves a direct BV link and
//! expands its pages. `stream` resolves a DASH audio URL (the CDN requires
//! the site Referer header, which the library supplies). `auth` creates a QR
//! session and polls once; `favorites` needs a completed scan login.

#[path = "../src/qr_terminal.rs"]
mod qr_terminal;

use musid_dli::{
    StreamQuality,
    provider::{
        AuthenticationProvider, PlaylistProvider, QrAuthenticationProvider, ResolveOptions,
        SearchQuery, StreamResolver, TrackSearch,
    },
    source_bilibili::BilibiliProvider,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let modules: Vec<String> = if args.is_empty() {
        vec!["search".into(), "stream".into()]
    } else {
        args
    };
    let provider = BilibiliProvider::new();
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

async fn run_module(provider: &BilibiliProvider, module: &str) -> Result<Outcome, String> {
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
        "reference" => {
            // Direct BV link: pages expand into per-part tracks.
            let page = provider
                .search(&SearchQuery::new(
                    "https://www.bilibili.com/video/BV1GJ411x7h7/",
                ))
                .await
                .map_err(|e| e.to_string())?;
            println!("reference resolved to {} track(s)", page.items.len());
            for track in page.items.iter().take(3) {
                print_track(track);
            }
            Ok(Outcome::Ok)
        }
        "stream" => {
            // Anonymous keyword search is risk-controlled by Bilibili (the
            // response carries a v_voucher and no results), so the stream
            // module resolves through a direct BV reference instead.
            let page = provider
                .search(&SearchQuery::new(
                    "https://www.bilibili.com/video/BV1GJ411x7h7/",
                ))
                .await
                .map_err(|e| e.to_string())?;
            let track = page
                .items
                .first()
                .ok_or("reference returned no tracks")?
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
                "quality={:?} format={:?} url_host={}",
                stream.quality,
                stream.format,
                stream
                    .url
                    .split_once('/')
                    .and_then(|(_, rest)| rest.split('/').next())
                    .unwrap_or("-")
            );
            Ok(Outcome::Ok)
        }
        "auth" => {
            let qr = provider
                .create_qr_login()
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "QR expires_in={}s; scan with the Bilibili app:",
                qr.expires_in
            );
            print!("{}", qr_terminal::render(&qr.url));
            match qr_terminal::render_png("bilibili_qr.png", &qr.url) {
                Ok(()) => println!(
                    "QR also saved to bilibili_qr.png (scan that if the terminal one won't read)"
                ),
                Err(e) => println!("PNG fallback failed: {e}"),
            }
            // Poll every 3 seconds until the phone scan completes (Success),
            // the QR expires, or the 180-second budget runs out.
            let started = std::time::Instant::now();
            let (status, user_id, display_name) = loop {
                let poll = provider
                    .poll_qr_login(&qr.key)
                    .await
                    .map_err(|e| e.to_string())?;
                println!("  poll: {:?} ({})", poll.status, poll.message);
                match poll.status {
                    musid_dli::provider::QrLoginStatus::Success
                    | musid_dli::provider::QrLoginStatus::Expired
                    | musid_dli::provider::QrLoginStatus::Failed => {
                        break (poll.status, poll.user_id, poll.display_name);
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
                    "logged in as {:?} ({:?}); run the search/stream/favorites modules now",
                    display_name, user_id
                );
            }
            Ok(Outcome::Ok)
        }
        "favorites" => {
            let status = provider.login_status().await.map_err(|e| e.to_string())?;
            match &status {
                musid_dli::provider::LoginStatus::LoggedIn { .. } => {
                    let folders = provider
                        .playlists("", 1, 20)
                        .await
                        .map_err(|e| e.to_string())?;
                    println!("logged in: {} favorite folders", folders.len());
                    Ok(Outcome::Ok)
                }
                _ => Ok(Outcome::Skipped(
                    "not logged in; complete a QR scan first".into(),
                )),
            }
        }
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search reference stream auth favorites"
        )),
    }
}

fn print_track(track: &musid_dli::models::Track) {
    println!(
        "{} - {} [bvid={}]",
        track.artists.join("、"),
        track.title,
        track.extra.get("bvid").map(String::as_str).unwrap_or("-")
    );
}
