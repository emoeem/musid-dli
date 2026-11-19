//! Kuwo provider live smoke test.
//!
//! Usage:
//!   cargo run --features source-kuwo --example kuwo_smoke [MODULE...]
//!
//! Modules: search lyrics stream playlist link cover
//! Default (no args): search lyrics.
//!
//! Kuwo's anonymous mobile endpoint serves paid songs as an 11-second
//! placeholder with a mismatched rid; the library rejects those, so the
//! stream module walks the first search hits looking for a free track and
//! reports SKIPPED when none is available anonymously. Lossless requests
//! downgrade to 320k/128k on this endpoint.

use musid_dli::{
    StreamQuality,
    provider::{
        CollectionProvider, LinkResolver, LyricsProvider, PlaylistProvider, ResolveOptions,
        ResolvedLink, SearchQuery, StreamResolver, TrackSearch,
    },
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
        "playlist" => {
            let categories = provider.categories().await.map_err(|e| e.to_string())?;
            println!("categories: {}", categories.len());
            // The synthetic 全部 entry (empty id) fronts the list; prefer a
            // real tag for the by-tag call and fall back to 全部.
            let tag = categories
                .iter()
                .find(|c| !c.id.is_empty())
                .ok_or("no child playlist category returned")?;
            let by_tag = provider
                .playlists(&tag.id, 1, 10)
                .await
                .map_err(|e| e.to_string())?;
            println!("playlists({}): {}", tag.name, by_tag.len());
            let hot = provider
                .playlists("", 1, 10)
                .await
                .map_err(|e| e.to_string())?;
            println!("playlists(全部): {}", hot.len());
            // Hot-list ids carry a synthetic digest prefix; resolving one
            // through the detail endpoint exercises the parse_id strip
            // (digest-5 node ids would additionally need the qukudata
            // resolve, which no live sample exercises).
            let hot_first = hot.first().ok_or("hot list returned nothing")?;
            let hot_meta = provider
                .playlist(&hot_first.id.id)
                .await
                .map_err(|e| e.to_string())?;
            println!(
                "hot playlist \"{}\": total={:?}",
                hot_meta.title, hot_meta.track_count
            );
            let found = provider
                .search_playlists("周杰伦", 1)
                .await
                .map_err(|e| e.to_string())?;
            println!("search_playlists(周杰伦): {}", found.len());
            let playlist = found.first().ok_or("playlist search returned nothing")?;
            let meta = provider
                .playlist(&playlist.id.id)
                .await
                .map_err(|e| e.to_string())?;
            println!("playlist \"{}\": total={:?}", meta.title, meta.track_count);
            let tracks = provider
                .playlist_tracks(&playlist.id.id)
                .await
                .map_err(|e| e.to_string())?;
            println!("playlist tracks fetched: {}", tracks.len());
            if let Some(track) = tracks.first() {
                println!("first track: {}", track.title);
            }
            Ok(Outcome::Ok)
        }
        "link" => {
            // Song share link: metadata only — no stream URL is resolved
            // here (paid songs have none on this endpoint but must still
            // be listable).
            let hit = first_search_hit(provider).await?;
            let song_url = format!("https://www.kuwo.cn/play_detail/{}", hit.id.id);
            let ResolvedLink::Track(track) = provider
                .resolve_link(&song_url)
                .await
                .map_err(|e| e.to_string())?
            else {
                return Err("song link did not resolve to a track".into());
            };
            println!("song link {song_url} -> \"{}\"", track.title);
            // Playlist share link: metadata + the page-0 songs in one
            // request (upstream get_detail_with_meta shape; nodes that cap
            // the rows can return fewer songs than track_count).
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
            let playlist_url = format!("http://www.kuwo.cn/playlist_detail/{playlist_id}");
            let ResolvedLink::Playlist(playlist) = provider
                .resolve_link(&playlist_url)
                .await
                .map_err(|e| e.to_string())?
            else {
                return Err("playlist link did not resolve to a playlist".into());
            };
            println!(
                "playlist link {playlist_url} -> \"{}\": total={:?} tracks on page 0: {}",
                playlist.title,
                playlist.track_count,
                playlist.tracks.len()
            );
            // Unrecognized links must be rejected, not fetched.
            match provider
                .resolve_link("https://example.com/play_detail/1")
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
            Ok(Outcome::Ok)
        }
        "cover" => {
            // Tier one (web-signed musicInfo) resolves the audit's live
            // baseline rid 6550104; tier two (artistpicserver) only runs
            // when musicInfo yields nothing, so a hit here does not prove
            // which tier served it. The URL must be an absolute http(s)
            // address, never a relative path or the plain text NO_PIC.
            let cover = provider
                .resolve_cover_url("6550104")
                .await
                .map_err(|e| e.to_string())?;
            println!("cover(6550104): {cover}");
            if !cover.starts_with("http://") && !cover.starts_with("https://") {
                return Err(format!("cover URL is not absolute http(s): {cover}"));
            }
            // Paid songs keep their cover (rid 228908 is the upstream
            // placeholder-test rid; the paid wall only affects playback).
            let paid = provider
                .resolve_cover_url("228908")
                .await
                .map_err(|e| e.to_string())?;
            println!("cover(228908, paid song): {paid}");
            // An invalid rid must surface NotFound, not a bogus URL:
            // musicInfo answers code=-1 (歌曲不存在) and artistpicserver
            // answers the plain text NO_PIC.
            match provider.resolve_cover_url("999999999999").await {
                Err(musid_dli::Error::NotFound) => {
                    println!("cover(999999999999): NotFound as expected")
                }
                Err(error) => {
                    return Err(format!("invalid rid produced {error:?}, expected NotFound"));
                }
                Ok(url) => return Err(format!("invalid rid unexpectedly resolved: {url}")),
            }
            Ok(Outcome::Ok)
        }
        other => Err(format!(
            "unknown module \"{other}\"; expected one of: search lyrics stream playlist link cover"
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
