//! Qianqian (千千音乐) provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-qianqian --example qianqian_smoke [MODULE...]
//!
//! Modules: search stream lyrics playlist album link
//! Optional (records current live refusals): playlist_search
//! Default (no args): search stream.
//!
//! Anonymous scope: all modules run without any account; VIP tracks answer
//! path-less tracklink metadata, so `stream` picks a free track from search.

use musid_dli::{
    StreamQuality,
    provider::{
        AlbumProvider, CollectionProvider, LinkResolver, LyricsProvider, PlaylistProvider,
        ResolveOptions, ResolvedLink, SearchQuery, StreamResolver, TrackSearch,
    },
    source_qianqian::QianqianProvider,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let modules: Vec<String> = if args.is_empty() {
        vec!["search".into(), "stream".into()]
    } else {
        args
    };
    let provider = QianqianProvider::new();
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

async fn run_module(provider: &QianqianProvider, module: &str) -> Result<(), String> {
    match module {
        "search" => {
            let page = provider
                .search(&SearchQuery::new("晴天"))
                .await
                .map_err(|e| e.to_string())?;
            println!("{} tracks (total {:?})", page.items.len(), page.total);
            let track = page.items.first().ok_or("search returned no tracks")?;
            println!(
                "{} - {} [quality tiers {:?}]",
                track.artists.join("、"),
                track.title,
                track.available_qualities
            );
            Ok(())
        }
        "stream" => {
            // VIP tracks answer path-less tracklink metadata anonymously;
            // walk the search result for one whose resolution succeeds.
            let page = provider
                .search(&SearchQuery::new("晴天"))
                .await
                .map_err(|e| e.to_string())?;
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
                            "{} - {}: quality={:?} format={:?} size={:?} advisory={} md5={}",
                            track.artists.join("、"),
                            track.title,
                            stream.quality,
                            stream.format,
                            stream.content_length,
                            stream.size_is_advisory,
                            stream.checksum.as_deref().unwrap_or("-")
                        );
                        return Ok(());
                    }
                    Err(error) => {
                        println!(
                            "  skip ({} - {}): {error}",
                            track.artists.join("、"),
                            track.title
                        );
                    }
                }
            }
            Err("no search result resolved to a stream anonymously (all VIP?)".into())
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
            let categories = provider.categories().await.map_err(|e| e.to_string())?;
            println!(
                "{} categories (first: {:?})",
                categories.len(),
                categories[0].name
            );
            let tag = categories
                .iter()
                .find(|category| !category.id.is_empty())
                .ok_or("category directory has no concrete category")?;
            let lists = provider
                .playlists(&tag.id, 1, 30)
                .await
                .map_err(|e| e.to_string())?;
            let found = lists.first().ok_or("category list returned nothing")?;
            let detail = provider
                .playlist(&found.id.id)
                .await
                .map_err(|e| e.to_string())?;
            let tracks = provider
                .playlist_tracks(&found.id.id)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "playlist \"{}\" ({}): {:?} tracks, fetched {}",
                detail.title,
                found.id.id,
                detail.track_count,
                tracks.len()
            );
            if detail.link.is_none() {
                return Err("detail playlist lost its M3 link slot".into());
            }
            Ok(())
        }
        "album" => {
            let page = provider
                .search(&SearchQuery::new("逆流而上"))
                .await
                .map_err(|e| e.to_string())?;
            let first = page.items.first().ok_or("search returned no tracks")?;
            let from_extra = first.extra.get("album_asset_code").cloned();
            let code = from_extra
                .or_else(|| first.album_id.clone())
                .filter(|code| !code.is_empty() && code.chars().any(char::is_alphabetic))
                .ok_or("search found no album asset code")?;
            let album = provider.album(&code).await.map_err(|e| e.to_string())?;
            let tracks = provider
                .album_tracks(&code)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "album \"{}\" ({}): artists {:?}, {} tracks fetched",
                album.title,
                code,
                album.artists,
                tracks.len()
            );
            Ok(())
        }
        "link" => {
            // Song link from a search hit's TSID.
            let page = provider
                .search(&SearchQuery::new("晴天"))
                .await
                .map_err(|e| e.to_string())?;
            let track = page
                .items
                .first()
                .ok_or("search returned no tracks")?
                .clone();
            let song_url = format!("https://music.91q.com/song/{}", track.id.id);
            match provider
                .resolve_link(&song_url)
                .await
                .map_err(|e| e.to_string())?
            {
                ResolvedLink::Track(resolved) => {
                    println!("song link: {}", resolved.title);
                }
                other => return Err(format!("expected track, got {other:?}")),
            }
            // Playlist link (the fixture-pinned id shape from the probe).
            match provider
                .resolve_link("https://music.91q.com/songlist/309558")
                .await
                .map_err(|e| e.to_string())?
            {
                ResolvedLink::Playlist(playlist) => {
                    println!("playlist link: {}", playlist.title);
                }
                other => return Err(format!("expected playlist, got {other:?}")),
            }
            // Unrecognized links must be rejected without a request.
            match provider.resolve_link("https://music.91q.com/player").await {
                Err(musid_dli::Error::InvalidInput(_)) => {
                    println!("unrecognized link correctly rejected");
                    Ok(())
                }
                other => Err(format!("unrecognized link produced {other:?}")),
            }
        }
        "playlist_search" => {
            // Optional probe: the 2026-11-19 anonymous re-probe found
            // /v1/search?type=6 refused live (errno 23001
            // 不被支持的搜索类型(10000)); upstream parity parses the refusal
            // to an empty result. This module surfaces the live shape
            // honestly: a refusal shape is reported as failure so the record
            // stays visible, a surprise success is also reported.
            let found = provider
                .search_playlists("华语", 1)
                .await
                .map_err(|e| e.to_string())?;
            if found.is_empty() {
                Err("playlist search returned 0 results (live refusal errno 23001 per the 2026-11-19 probe)".into())
            } else {
                println!("{} playlists", found.len());
                Ok(())
            }
        }
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search stream lyrics playlist album link playlist_search"
        )),
    }
}
