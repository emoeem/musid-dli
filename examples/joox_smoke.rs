//! JOOX provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-joox --example joox_smoke [MODULE...]
//!
//! Modules: search lyrics playlist_search stream playlist album link
//! Default (no args): search lyrics (the two anonymous-live faces).
//!
//! Anonymous live status (2026-11-19 re-probe): `search` and `lyrics` answer
//! anonymously (the lyric field is base64-encoded live); `playlist_search`
//! parses to an empty result because the live search body carries no
//! `section_list`; `stream` (web_get_songinfo) and the container faces
//! `playlist`/`album`/link-into-container answer 404 anonymously, so those
//! modules honestly FAIL with the surfaced refusal and are opt-in.

use musid_dli::{
    StreamQuality,
    provider::{
        AlbumProvider, CollectionProvider, LinkResolver, LyricsProvider, PlaylistProvider,
        ResolveOptions, ResolvedLink, SearchQuery, StreamResolver, TrackSearch,
    },
    source_joox::JooxProvider,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let modules: Vec<String> = if args.is_empty() {
        vec!["search".into(), "lyrics".into()]
    } else {
        args
    };
    let provider = JooxProvider::new();
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

async fn run_module(provider: &JooxProvider, module: &str) -> Result<(), String> {
    match module {
        "search" => {
            let page = provider
                .search(&SearchQuery::new("晴天"))
                .await
                .map_err(|e| e.to_string())?;
            println!("{} tracks (total {:?})", page.items.len(), page.total);
            let track = page.items.first().ok_or("search returned no tracks")?;
            println!(
                "{} - {} [{}]",
                track.artists.join(" / "),
                track.title,
                track.id.id
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
        "playlist_search" => {
            // Live status 2026-11-19: the search body carries no section_list,
            // so the face parses to an empty result (upstream parity, never an
            // error). A surprise non-empty result is reported so the record
            // stays visible either way.
            let found = provider
                .search_playlists("华语", 1)
                .await
                .map_err(|e| e.to_string())?;
            if found.is_empty() {
                Err("playlist search returned 0 results (live search body carries no section_list per the 2026-11-19 probe)".into())
            } else {
                println!("{} playlists", found.len());
                Ok(())
            }
        }
        "stream" => {
            // Live status 2026-11-19: web_get_songinfo answers HTTP 404 with
            // an empty body anonymously, so this module is expected to fail
            // with the surfaced refusal; it passes only if the endpoint
            // revived or a session cookie (SessionStore) unblocked it.
            let page = provider
                .search(&SearchQuery::new("晴天"))
                .await
                .map_err(|e| e.to_string())?;
            let mut last = String::new();
            for track in &page.items {
                match provider
                    .resolve(
                        track,
                        &ResolveOptions {
                            quality: StreamQuality::High,
                        },
                    )
                    .await
                {
                    Ok(stream) => {
                        println!(
                            "{} - {}: quality={:?} format={:?} advisory={}",
                            track.artists.join(" / "),
                            track.title,
                            stream.quality,
                            stream.format,
                            stream.size_is_advisory
                        );
                        return Ok(());
                    }
                    Err(error) => {
                        println!(
                            "  skip ({} - {}): {error}",
                            track.artists.join(" / "),
                            track.title
                        );
                        last = error.to_string();
                    }
                }
            }
            Err(format!(
                "no search result resolved anonymously; last refusal: {last}"
            ))
        }
        "playlist" => {
            // Containers are dead anonymously (v3 playlist nginx 404); this
            // module passes only if the endpoint revived.
            let found = provider
                .search_playlists("华语", 1)
                .await
                .map_err(|e| e.to_string())?;
            let target = found
                .first()
                .map(|p| p.id.id.clone())
                .ok_or("playlist search returned no id; using the documented dead shape")?;
            let detail = provider
                .playlist(&target)
                .await
                .map_err(|e| e.to_string())?;
            let tracks = provider
                .playlist_tracks(&target)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "playlist \"{}\" ({}): {:?} tracks, fetched {}",
                detail.title,
                target,
                detail.track_count,
                tracks.len()
            );
            Ok(())
        }
        "album" => {
            // Albums ride the same v3 playlist endpoint (dead anonymously).
            let page = provider
                .search(&SearchQuery::new("晴天"))
                .await
                .map_err(|e| e.to_string())?;
            let first = page.items.first().ok_or("search returned no tracks")?;
            let album_id = first
                .album_id
                .clone()
                .filter(|id| !id.is_empty())
                .ok_or("search found no album id")?;
            let album = provider.album(&album_id).await.map_err(|e| e.to_string())?;
            let tracks = provider
                .album_tracks(&album_id)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "album \"{}\" ({}): {:?} artists, {} tracks fetched",
                album.title,
                album_id,
                album.artists,
                tracks.len()
            );
            Ok(())
        }
        "link" => {
            // Link routing is fixture-pinned (links.json); the song link
            // resolution rides the dead songinfo endpoint anonymously, so the
            // live resolution half is expected to fail and only the
            // unrecognized-link rejection half passes live.
            let page = provider
                .search(&SearchQuery::new("晴天"))
                .await
                .map_err(|e| e.to_string())?;
            let track = page
                .items
                .first()
                .ok_or("search returned no tracks")?
                .clone();
            let song_url = format!("https://www.joox.com/hk/single/{}", track.id.id);
            match provider.resolve_link(&song_url).await {
                Ok(ResolvedLink::Track(resolved)) => {
                    println!("song link: {}", resolved.title);
                }
                Ok(other) => return Err(format!("expected track, got {other:?}")),
                Err(error) => {
                    println!("  song link resolution refused live (documented 2026-11-19): {error}")
                }
            }
            match provider.resolve_link("https://www.joox.com/").await {
                Err(musid_dli::Error::InvalidInput(_)) => {
                    println!("unrecognized link correctly rejected");
                    Ok(())
                }
                other => Err(format!("unrecognized link produced {other:?}")),
            }
        }
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search lyrics playlist_search stream playlist album link"
        )),
    }
}
