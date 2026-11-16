//! QQ Music provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-qqmusic --example qqmusic_smoke [MODULE...]
//!
//! Modules: search track stream lyrics playlist chart album link auth wechat user
//! Default (no args): search track stream lyrics.
//!
//! Anonymous runs get no playable stream URL (QQ module code 10006) and no
//! user playlists: both need a session cookie from a completed scan login.
//! `auth` creates a QR session, renders the PNG as a data URL and polls the
//! status once (expect Waiting); completing the scan on a phone is manual.

#[path = "../src/qr_terminal.rs"]
mod qr_terminal;

use musid_dli::{
    StreamQuality,
    provider::{
        AlbumProvider, AuthenticationProvider, ChartProvider, CollectionProvider, LinkResolver,
        LyricsProvider, PlaylistProvider, QrAuthenticationProvider, ResolveOptions, ResolvedLink,
        SearchQuery, StreamResolver, TrackSearch,
    },
    source_qqmusic::QqMusicProvider,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let modules: Vec<String> = if args.is_empty() {
        vec![
            "search".into(),
            "track".into(),
            "stream".into(),
            "lyrics".into(),
        ]
    } else {
        args
    };
    let provider = QqMusicProvider::new();
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

async fn run_module(provider: &QqMusicProvider, module: &str) -> Result<(), String> {
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
        "track" => {
            let track = first_search_hit(provider).await?;
            let url = format!("https://y.qq.com/n/ryqq/songDetail/{}", track.id.id);
            match provider
                .resolve_link(&url)
                .await
                .map_err(|e| e.to_string())?
            {
                ResolvedLink::Track(detail) => print_track(&detail),
                other => return Err(format!("expected track, got {other:?}")),
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
                "quality={:?} format={:?} bytes={:?}",
                stream.quality, stream.format, stream.content_length
            );
            Ok(())
        }
        "lyrics" => {
            let track = first_search_hit(provider).await?;
            let lyric = provider.lyrics(&track).await.map_err(|e| e.to_string())?;
            match lyric {
                Some(doc) => {
                    println!(
                        "{} lines; translation={}; romanization={}",
                        doc.lines.len(),
                        doc.translation.is_some(),
                        doc.romanization.is_some()
                    );
                    if let Some(line) = doc.lines.iter().find(|l| !l.text.trim().is_empty()) {
                        println!("first line: {}", line.text);
                    }
                }
                None => println!("no lyrics for this track"),
            }
            Ok(())
        }
        "playlist" => {
            let cats = provider.categories().await.map_err(|e| e.to_string())?;
            let category = cats
                .iter()
                .find(|c| c.id != "10000000")
                .ok_or("no real playlist category returned")?;
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
                "playlist \"{}\": track_count={:?}; fetched {} tracks",
                detail.title,
                detail.track_count,
                tracks.len()
            );
            Ok(())
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
            Ok(())
        }
        "album" => {
            let track = first_search_hit(provider).await?;
            let album_mid = track.album_id.clone().ok_or("track has no album mid")?;
            let album = provider
                .album(&album_mid)
                .await
                .map_err(|e| e.to_string())?;
            let tracks = provider
                .album_tracks(&album_mid)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "album \"{}\" ({}): {} artists; {} tracks",
                album.title,
                album.id.id,
                album.artists.join("、"),
                tracks.len()
            );
            Ok(())
        }
        "link" => {
            let track = first_search_hit(provider).await?;
            let song_url = format!("https://y.qq.com/n/ryqq/songDetail/{}", track.id.id);
            if !matches!(
                provider
                    .resolve_link(&song_url)
                    .await
                    .map_err(|e| e.to_string())?,
                ResolvedLink::Track(_)
            ) {
                return Err("song link did not resolve to a track".into());
            }
            let found = provider
                .search_playlists("周杰伦", 1)
                .await
                .map_err(|e| e.to_string())?;
            let disstid = found
                .first()
                .ok_or("playlist search returned nothing")?
                .id
                .id
                .clone();
            let playlist_url = format!("https://y.qq.com/n/ryqq/playlist/{disstid}");
            if !matches!(
                provider
                    .resolve_link(&playlist_url)
                    .await
                    .map_err(|e| e.to_string())?,
                ResolvedLink::Playlist(_)
            ) {
                return Err("playlist link did not resolve to a playlist".into());
            }
            let album_mid = track.album_id.clone().ok_or("track has no album mid")?;
            let album_url = format!("https://y.qq.com/n/ryqq/albumDetail/{album_mid}");
            if !matches!(
                provider
                    .resolve_link(&album_url)
                    .await
                    .map_err(|e| e.to_string())?,
                ResolvedLink::Album(_)
            ) {
                return Err("album link did not resolve to an album".into());
            }
            match provider
                .resolve_link("https://example.com/songDetail/abc")
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
        "auth" => {
            let qr = provider
                .create_qr_login()
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "QR expires_in={}s; scan with the QQ Music app:",
                qr.expires_in
            );
            // The ptqrshow QR content is opaque: decode the PNG via
            // image + rqrr, then re-render it as an ANSI QR for the terminal.
            let png_bytes = {
                use base64::Engine as _;
                let raw = qr
                    .url
                    .strip_prefix("data:image/png;base64,")
                    .unwrap_or(&qr.url);
                base64::engine::general_purpose::STANDARD
                    .decode(raw)
                    .map_err(|e| format!("QR base64 decode failed: {e}"))?
            };
            let decoded = image::load_from_memory(&png_bytes)
                .map_err(|e| format!("QR PNG decode failed: {e}"))?
                .to_luma8();
            let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(
                decoded.width() as usize,
                decoded.height() as usize,
                |x, y| decoded.get_pixel(x as u32, y as u32).0[0],
            );
            let grids = prepared.detect_grids();
            let grid = grids.first().ok_or("QR contains no grid")?;
            let (_, content) = grid
                .decode()
                .map_err(|e| format!("QR content decode failed: {e}"))?;
            print!("{}", qr_terminal::render(&content));
            match qr_terminal::render_png("qqmusic_qr.png", &content) {
                Ok(()) => println!(
                    "QR also saved to qqmusic_qr.png (scan that if the terminal one won't read)"
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
                    "logged in as {:?} ({:?}); run the user/stream modules now",
                    display_name, user_id
                );
            }
            Ok(())
        }
        "user" => {
            let status = provider.login_status().await.map_err(|e| e.to_string())?;
            match &status {
                musid_dli::provider::LoginStatus::LoggedIn { .. } => {
                    let lists = provider
                        .user_playlists(1, 20)
                        .await
                        .map_err(|e| e.to_string())?;
                    println!("logged in: {} user playlists", lists.len());
                }
                _ => println!(
                    "not logged in ({status:?}); complete a scan login first, skipping user playlists"
                ),
            }
            Ok(())
        }
        "wechat" => {
            let qr = provider
                .create_wechat_login()
                .await
                .map_err(|e| e.to_string())?;
            println!("WeChat QR expires_in={}s; scan with WeChat:", qr.expires_in);
            // The WeChat OAuth QR encodes the confirm URL built from the uuid
            // (documented WeChat qrconnect QR content).
            let uuid = qr.key.strip_prefix("wx:").unwrap_or(&qr.key);
            let qr_content = format!("https://open.weixin.qq.com/connect/confirm?uuid={uuid}");
            print!("{}", qr_terminal::render(&qr_content));
            match qr_terminal::render_png("qq_wechat_qr.png", &qr_content) {
                Ok(()) => println!(
                    "QR also saved to qq_wechat_qr.png (scan that if the terminal one won't read)"
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
                    "logged in as {:?} ({:?}); run the user/stream modules now",
                    display_name, user_id
                );
            }
            Ok(())
        }
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search track stream lyrics playlist chart album link auth user"
        )),
    }
}

async fn first_search_hit(provider: &QqMusicProvider) -> Result<musid_dli::models::Track, String> {
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
