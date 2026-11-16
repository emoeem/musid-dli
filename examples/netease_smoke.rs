//! NetEase provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-netease --example netease_smoke [MODULE...]
//!
//! Modules: search track stream lyrics playlist chart album artist link auth user
//! Default (no args): search track stream lyrics.
//!
//! `auth` generates a QR login key and polls the status once; completing the
//! login on a phone is out of scope for the smoke run. `user` requires an
//! existing session (see `set_cookies` in the library) and reports a hint
//! instead of failing when logged out.
//!
//! Fixed ids (SONG_ID / ARTIST_ID) are well-known NetEase resources and can be
//! replaced via `MUSID_SONG_ID` / `MUSID_ARTIST_ID` environment variables.

use musid_dli::{
    StreamQuality,
    models::Track,
    provider::{
        AlbumProvider, ArtistProvider, AuthenticationProvider, ChartProvider, CollectionProvider,
        LinkResolver, LyricsProvider, PlaylistProvider, QrAuthenticationProvider, ResolveOptions,
        ResolvedLink, SearchQuery, StreamResolver, TrackSearch,
    },
    source_netease::NeteaseProvider,
};

const SONG_ID: &str = "186016"; // 晴天 - 周杰伦 (replaceable)
const ARTIST_ID: &str = "6452"; // 周杰伦 (replaceable)
const SONG_URL_TEMPLATE: &str = "https://music.163.com/#/song?id={SONG_ID}";

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
    let song_id = std::env::var("MUSID_SONG_ID").unwrap_or_else(|_| SONG_ID.into());
    let artist_id = std::env::var("MUSID_ARTIST_ID").unwrap_or_else(|_| ARTIST_ID.into());
    let provider = NeteaseProvider::new();
    let mut failed = Vec::new();
    for module in &modules {
        print!("== {module}: ");
        match run_module(&provider, module, &song_id, &artist_id).await {
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

async fn run_module(
    provider: &NeteaseProvider,
    module: &str,
    song_id: &str,
    artist_id: &str,
) -> Result<(), String> {
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
            let song_url = SONG_URL_TEMPLATE.replace("{SONG_ID}", song_id);
            match provider
                .resolve_link(&song_url)
                .await
                .map_err(|e| e.to_string())?
            {
                ResolvedLink::Track(track) => print_track(&track),
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
                "quality={:?} format={:?} bytes={:?} expiry={:?} md5={:?}",
                stream.quality,
                stream.format,
                stream.content_length,
                stream.expires_at_unix,
                stream.checksum.as_deref().map(|m| &m[..8.min(m.len())])
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
                None => println!("no lyrics for this track (instrumental or missing)"),
            }
            Ok(())
        }
        "playlist" => {
            let cats = provider.categories().await.map_err(|e| e.to_string())?;
            println!(
                "categories: {} (first: {})",
                cats.len(),
                cats.first().map(|c| c.name.as_str()).unwrap_or("-")
            );
            let hot = provider
                .playlists("华语", 1, 5)
                .await
                .map_err(|e| e.to_string())?;
            println!("hot playlists(华语): {}", hot.len());
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
            let song_url = SONG_URL_TEMPLATE.replace("{SONG_ID}", song_id);
            let ResolvedLink::Track(track) = provider
                .resolve_link(&song_url)
                .await
                .map_err(|e| e.to_string())?
            else {
                return Err("song link did not resolve to a track".into());
            };
            let album_id = track.album_id.clone().ok_or("track has no album id")?;
            let album = provider.album(&album_id).await.map_err(|e| e.to_string())?;
            let tracks = provider
                .album_tracks(&album_id)
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
        "artist" => {
            let artist = provider
                .artist(artist_id)
                .await
                .map_err(|e| e.to_string())?;
            let top = provider
                .artist_top_tracks(artist_id)
                .await
                .map_err(|e| e.to_string())?;
            let albums = provider
                .artist_albums(artist_id, 1, 5)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "artist \"{}\": {} top tracks; {} albums (page 1)",
                artist.name,
                top.len(),
                albums.len()
            );
            Ok(())
        }
        "link" => {
            let song_url = SONG_URL_TEMPLATE.replace("{SONG_ID}", song_id);
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
            let playlist_id = found
                .first()
                .ok_or("playlist search returned nothing")?
                .id
                .id
                .clone();
            let playlist_url = format!("https://music.163.com/#/playlist?id={playlist_id}");
            if !matches!(
                provider
                    .resolve_link(&playlist_url)
                    .await
                    .map_err(|e| e.to_string())?,
                ResolvedLink::Playlist(_)
            ) {
                return Err("playlist link did not resolve to a playlist".into());
            }
            // Unrecognized hosts must be rejected, not fetched.
            match provider.resolve_link("https://example.com/song?id=1").await {
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
            println!("QR key generated; expires_in={}s", qr.expires_in);
            let poll = provider
                .poll_qr_login(&qr.key)
                .await
                .map_err(|e| e.to_string())?;
            println!("poll status: {:?} ({})", poll.status, poll.message);
            Ok(())
        }
        "user" => {
            let status = provider.login_status().await.map_err(|e| e.to_string())?;
            match &status {
                musid_dli::provider::LoginStatus::LoggedIn { display_name, .. } => {
                    let lists = provider
                        .user_playlists(1, 20)
                        .await
                        .map_err(|e| e.to_string())?;
                    println!(
                        "logged in as {:?}: {} user playlists",
                        display_name.clone().unwrap_or_default(),
                        lists.len()
                    );
                }
                _ => println!(
                    "not logged in ({status:?}); set a session cookie first, skipping user playlists"
                ),
            }
            Ok(())
        }
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search track stream lyrics playlist chart album artist link auth user"
        )),
    }
}

async fn first_search_hit(provider: &NeteaseProvider) -> Result<Track, String> {
    let page = provider
        .search(&SearchQuery::new("晴天 周杰伦"))
        .await
        .map_err(|e| e.to_string())?;
    page.items
        .into_iter()
        .next()
        .ok_or_else(|| "search returned no tracks".into())
}

fn print_track(track: &Track) {
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
