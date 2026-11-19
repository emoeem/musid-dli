//! Optional YouTube Music provider powered by RustyPipe.
use crate::{
    Error, ErrorDetail, Result,
    models::{AudioFormat, ByteRange, FallbackEndpoint, StreamInfo, StreamQuality, Track, TrackId},
    provider::{
        MusicProvider, ProviderCapabilities, ResolveOptions, SearchPage, SearchQuery,
        StreamResolver, TrackSearch,
    },
};
use async_trait::async_trait;
use rustypipe::{
    client::RustyPipe,
    error::{ExtractionError, UnavailabilityReason},
    model::{AudioCodec as YtAudioCodec, AudioFormat as YtAudioFormat, TrackItem},
};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub struct RustyPipeProvider {
    client: Arc<RustyPipe>,
}
impl RustyPipeProvider {
    /// A dedicated storage directory is mandatory: RustyPipe caches client
    /// versions/deobfuscation data and may store authentication material there.
    /// If rustypipe-botguard is on PATH, RustyPipe detects it automatically.
    pub fn new(storage_dir: impl AsRef<Path>) -> Result<Self> {
        Self::build(storage_dir.as_ref().to_path_buf(), None)
    }

    /// Configure an explicit Botguard executable path for browser-client PO tokens.
    pub fn new_with_botguard(
        storage_dir: impl AsRef<Path>,
        botguard_bin: impl Into<OsString>,
    ) -> Result<Self> {
        Self::build(
            storage_dir.as_ref().to_path_buf(),
            Some(botguard_bin.into()),
        )
    }

    fn build(storage_dir: PathBuf, botguard_bin: Option<OsString>) -> Result<Self> {
        fs::create_dir_all(&storage_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&storage_dir, fs::Permissions::from_mode(0o700))?;
        }
        let builder = RustyPipe::builder()
            .storage_dir(storage_dir.clone())
            .botguard_snapshot_file(storage_dir.join("botguard.snapshot"));
        let result = match botguard_bin {
            Some(path) => builder.botguard_bin(path).build(),
            None => builder.build(),
        };
        let client = result.map_err(classify_rustypipe_error)?;
        Ok(Self {
            client: Arc::new(client),
        })
    }
}

/// Classifies a RustyPipe failure into the M2 error taxonomy
/// (AUTH_REGISTRY_DESIGN.md §1.1), replacing the `Error::Provider`
/// catch-alls this module shipped with at its four failure sites (client
/// build, music search, search continuation, player resolve).
///
/// Mapping (AUTH_REGISTRY_DESIGN.md §9.6 addendum shape, the jamendo/apple/
/// fivesing and m2-apple follow-up precedent):
///
/// - reqwest transport text (`Error::Http`) → `Network` (retryable);
/// - `Error::HttpStatus` 429/5xx → `Unavailable` (retryable): RustyPipe has
///   already exhausted its own internal retry ladder for exactly these
///   statuses (`rustypipe::error::Error::should_retry` retries 5xx + 429)
///   before the failure surfaces here, so a resend is the standing recovery;
/// - `Error::HttpStatus` everything else → `InvalidResponse` (unmapped
///   status outside its code table, migu/m2-apple precedent); the HTTP
///   status stays out of the `code` slot (HTTP statuses are not platform API
///   codes, joox/qianqian/apple precedent) and rides the detail text;
/// - `ExtractionError::NotFound` → the legacy `NotFound` semantics (the
///   platform itself answers 404/not-found for the requested content);
/// - `ExtractionError::Unavailable` with `UnavailabilityReason::Paid` → the
///   legacy DRM `Unsupported` semantics: RustyPipe documents `Paid` as "needs
///   to be purchased and is protected by digital restrictions management",
///   the same refusal class as the `player.drm` check in [`StreamResolver::
///   resolve`];
/// - `ExtractionError::Unavailable` with any other reason → `Unavailable`
///   (retryable); the ask bounds the target set to
///   Network/Unavailable/InvalidResponse, and the detail text preserves the
///   exact reason plus the YouTube message so hosts never lose the
///   deleted/private/premium distinction (permanent reasons ride a retryable
///   category only through this bucket);
/// - `ExtractionError::Botguard` → `Unavailable` (PO-token attestation is a
///   provider-side service/environment condition, not a response violation);
/// - `ExtractionError::InvalidData` / `WrongResult` → `InvalidResponse`
///   (unparseable/wrong data is an upstream contract violation; the upstream
///   bad-request shape follows the netease-881 precedent into the same
///   category via the residue arm);
/// - `Error::Auth` → `AuthenticationRequired`: the 401 arm of RustyPipe's
///   request path maps to `AuthError::NoLogin` ("you are not logged in"),
///   and mapping a declared auth refusal to a transport category would be
///   false (this provider exposes no login surface, so the category is
///   honest even though the recovery hint cannot be followed here);
/// - the residue arm (`Error::Other` and future RustyPipe variants) →
///   `InvalidResponse` with the preserved text.
///
/// The provider id rides the typed slot on every M2 variant. The
/// pre-migration text (`error.to_string()`, byte-identical to what the
/// `Error::Provider` message slot carried) is preserved in the `ErrorDetail`
/// slot, except that the reqwest transport text loses its full request-URL
/// suffix ([`strip_reqwest_url_suffix`]) because the ErrorDetail contract
/// (src/error.rs) forbids full request URLs in the slot. RustyPipe only
/// surfaces failure *messages* through these texts — no cookies, tokens,
/// signature parameters or session ids ride them (the innertube endpoints
/// are anonymous `POST https://music.youtube.com/youtubei/v1/<endpoint>`
/// calls).
fn classify_rustypipe_error(error: rustypipe::error::Error) -> Error {
    let provider = Some("youtube-music".to_owned());
    // Pre-migration catch-all text, byte-exact apart from the reqwest URL
    // suffix strip (see fn docs).
    let text = strip_reqwest_url_suffix(&error.to_string()).to_owned();
    let detail = || Some(ErrorDetail::new(text.clone()));
    match error {
        rustypipe::error::Error::Http(_) => Error::Network {
            provider,
            detail: detail(),
        },
        rustypipe::error::Error::HttpStatus(code, _) if code == 429 || code >= 500 => {
            Error::Unavailable {
                provider,
                detail: detail(),
            }
        }
        rustypipe::error::Error::HttpStatus(..) => Error::InvalidResponse {
            provider,
            code: None,
            detail: detail(),
        },
        rustypipe::error::Error::Extraction(ExtractionError::NotFound { .. }) => Error::NotFound,
        rustypipe::error::Error::Extraction(ExtractionError::Unavailable {
            reason: UnavailabilityReason::Paid,
            ..
        }) => Error::Unsupported(
            "YouTube refused extraction: the content is DRM-protected (paid)".into(),
        ),
        rustypipe::error::Error::Extraction(ExtractionError::Unavailable { .. }) => {
            Error::Unavailable {
                provider,
                detail: detail(),
            }
        }
        rustypipe::error::Error::Extraction(ExtractionError::Botguard(_)) => Error::Unavailable {
            provider,
            detail: detail(),
        },
        rustypipe::error::Error::Extraction(
            ExtractionError::InvalidData(_) | ExtractionError::WrongResult(_),
        ) => Error::InvalidResponse {
            provider,
            code: None,
            detail: detail(),
        },
        rustypipe::error::Error::Auth(_) => Error::AuthenticationRequired {
            provider,
            detail: detail(),
        },
        _ => Error::InvalidResponse {
            provider,
            code: None,
            detail: detail(),
        },
    }
}

/// Strips the trailing full-request-URL suffix reqwest appends to its
/// transport error Display ("... for url (...)"). The ErrorDetail
/// sanitization contract (src/error.rs) forbids full request URLs in the
/// detail slot, even though the anonymous innertube URLs carry no
/// credentials.
fn strip_reqwest_url_suffix(text: &str) -> &str {
    match text.find(" for url (") {
        Some(index) => &text[..index],
        None => text,
    }
}

impl RustyPipeProvider {
    fn map_track(item: TrackItem) -> Track {
        let artists = item.artists.into_iter().map(|artist| artist.name).collect();
        let artwork_url = item
            .cover
            .into_iter()
            .max_by_key(|image| image.width)
            .map(|image| image.url);
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("track_type".into(), format!("{:?}", item.track_type));
        let album_id = item.album.as_ref().map(|album| album.id.clone());
        let album = item.album.map(|album| album.name);
        Track {
            id: TrackId::new("youtube-music", item.id),
            title: item.name,
            artists,
            album,
            album_id,
            duration: item
                .duration
                .map(|seconds| Duration::from_secs(seconds as u64)),
            artwork_url,
            isrc: None,
            available_qualities: Vec::new(),
            audio: None,
            extra,
        }
    }
}
impl MusicProvider for RustyPipeProvider {
    fn id(&self) -> &str {
        "youtube-music"
    }
    fn name(&self) -> &str {
        "YouTube Music"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            track_search: true,
            stream_resolve: true,
            ..Default::default()
        }
    }
}

#[async_trait]
impl TrackSearch for RustyPipeProvider {
    async fn search(&self, query: &SearchQuery) -> Result<SearchPage> {
        let result = self
            .client
            .query()
            .music_search_tracks(&query.keyword)
            .await
            .map_err(classify_rustypipe_error)?;
        let mut page = result.items;
        for _ in 1..query.page.max(1) {
            let Some(token) = page.ctoken.clone() else {
                break;
            };
            page = self
                .client
                .query()
                .continuation(token, page.endpoint, page.visitor_data.as_deref())
                .await
                .map_err(classify_rustypipe_error)?;
        }
        let total = page.count;
        let has_more = page.ctoken.is_some();
        let items = page
            .items
            .into_iter()
            .take(query.page_size as usize)
            .map(Self::map_track)
            .collect();
        Ok(SearchPage {
            items,
            total,
            has_more,
        })
    }
}

#[async_trait]
impl StreamResolver for RustyPipeProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo> {
        if track.id.provider != self.id() {
            return Err(Error::InvalidInput(
                "track does not belong to YouTube Music".into(),
            ));
        }
        let player = self
            .client
            .query()
            .player(&track.id.id)
            .await
            .map_err(classify_rustypipe_error)?;
        if player.drm.is_some() {
            return Err(Error::Unsupported("YouTube returned a DRM-protected stream; refusing to expose it as a normal audio URL".into()));
        }
        let mut streams = player
            .audio_streams
            .into_iter()
            .filter(|stream| stream.drm_track_type.is_none() && stream.drm_systems.is_empty())
            .collect::<Vec<_>>();
        if streams.is_empty() {
            return Err(Error::NotFound);
        }
        let target_bitrate = match options.quality {
            StreamQuality::Low => 64_000,
            StreamQuality::Standard => 128_000,
            StreamQuality::High => 192_000,
            StreamQuality::Lossless | StreamQuality::HiRes | StreamQuality::Unknown => u32::MAX,
        };
        streams.sort_by_key(|stream| stream.bitrate.abs_diff(target_bitrate));
        let chosen = streams.remove(0);
        let quality = quality_from_bitrate(chosen.bitrate);
        let format = match (chosen.format, chosen.codec) {
            (YtAudioFormat::M4a, YtAudioCodec::Mp4a) => AudioFormat::Aac,
            (YtAudioFormat::M4a, _) => AudioFormat::M4a,
            (YtAudioFormat::Webm, YtAudioCodec::Opus) => AudioFormat::Opus,
            (YtAudioFormat::Webm, _) => AudioFormat::Unknown,
            _ => AudioFormat::Unknown,
        };
        let index_range = chosen.index_range.map(|range| ByteRange {
            start: range.start as u64,
            end: range.end as u64,
        });
        let init_range = chosen.init_range.map(|range| ByteRange {
            start: range.start as u64,
            end: range.end as u64,
        });
        // The non-chosen streams are different bitrates (they switch streams,
        // MODEL_ALIGNMENT_DESIGN.md §2.4): the structured slot carries each
        // one's own tier and declared size plus the shared player expiry,
        // while the legacy string slot keeps the bare URLs in the same order.
        let expires_at_unix = player.valid_until.unix_timestamp().max(0) as u64;
        let fallback_candidates: Vec<(String, u32, u64)> = streams
            .into_iter()
            .map(|stream| (stream.url, stream.bitrate, stream.size))
            .collect();
        let fallback_endpoints =
            fallback_endpoints_from_streams(&fallback_candidates, expires_at_unix);
        let fallback_urls = fallback_candidates
            .iter()
            .map(|(url, _, _)| url.clone())
            .collect();
        Ok(StreamInfo {
            url: chosen.url,
            quality,
            format,
            duration: chosen
                .duration_ms
                .map(|ms| Duration::from_millis(ms as u64))
                .or_else(|| {
                    (player.details.duration > 0)
                        .then(|| Duration::from_secs(player.details.duration as u64))
                }),
            expires_at_unix: Some(expires_at_unix),
            content_length: Some(chosen.size),
            checksum: None,
            headers: Vec::new(),
            fallback_urls,
            index_range,
            init_range,
            max_chunk_size: None,
            size_is_advisory: false,
            checksum_algorithm: None,
            fallback_endpoints,
        })
    }
}

/// Structured fallback endpoints for the non-chosen audio streams
/// (MODEL_ALIGNMENT_DESIGN.md §2.4). The per-URL tier comes from the bitrate
/// map (googlevideo fallbacks are different bitrates, so `None`'s
/// same-tier-as-primary assumption would be wrong), the declared size rides
/// along, and every URL shares the player's expiry because googlevideo
/// grants expire together. The urls/bitrates/sizes tuples exist because
/// `rustypipe::model::AudioStream` is `#[non_exhaustive]` and cannot be
/// constructed in this crate's tests.
fn fallback_endpoints_from_streams(
    candidates: &[(String, u32, u64)],
    expires_at_unix: u64,
) -> Vec<FallbackEndpoint> {
    candidates
        .iter()
        .map(|(url, bitrate, size)| FallbackEndpoint {
            url: url.clone(),
            headers: Vec::new(),
            expires_at_unix: Some(expires_at_unix),
            content_length: Some(*size),
            quality: Some(quality_from_bitrate(*bitrate)),
        })
        .collect()
}

fn quality_from_bitrate(bitrate: u32) -> StreamQuality {
    match bitrate {
        0..=63_999 => StreamQuality::Low,
        64_000..=127_999 => StreamQuality::Standard,
        _ => StreamQuality::High,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustypipe::error::AuthError;

    #[test]
    fn youtube_music_never_claims_lossless_from_bitrate_alone() {
        assert_eq!(quality_from_bitrate(96_000), StreamQuality::Standard);
        assert_eq!(quality_from_bitrate(256_000), StreamQuality::High);
        assert_eq!(quality_from_bitrate(320_000), StreamQuality::High);
    }

    #[test]
    fn fallback_endpoints_carry_the_per_stream_tier_size_and_expiry() {
        // MODEL_ALIGNMENT_DESIGN.md §2.4: the leftover streams are different
        // bitrates, so each structured endpoint keeps its own tier/size and
        // the shared googlevideo expiry; the order follows the stream list.
        let candidates = vec![
            ("https://gvid/32".to_owned(), 32_000u32, 1_000_000u64),
            ("https://gvid/192".to_owned(), 192_000, 3_000_000),
        ];
        let endpoints = fallback_endpoints_from_streams(&candidates, 1_700_000_000);
        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0].url, "https://gvid/32");
        assert_eq!(endpoints[0].quality, Some(StreamQuality::Low));
        assert_eq!(endpoints[0].content_length, Some(1_000_000));
        assert_eq!(endpoints[0].expires_at_unix, Some(1_700_000_000));
        assert!(endpoints[0].headers.is_empty());
        assert_eq!(endpoints[1].url, "https://gvid/192");
        assert_eq!(endpoints[1].quality, Some(StreamQuality::High));
        assert_eq!(endpoints[1].content_length, Some(3_000_000));
        // Empty candidate list stays empty (no fabricated slots).
        assert!(fallback_endpoints_from_streams(&[], 1).is_empty());
    }

    // ---- M2 error-category wiring (slice m2-youtube-error-categories-unwired) ----

    #[test]
    fn http_transport_failure_lands_in_network_with_the_url_stripped() {
        // reqwest appends the full request URL to its transport Display
        // (reqwest-0.12.28/src/error.rs:268); the ErrorDetail contract forbids
        // full request URLs in the detail slot, so the suffix must be gone
        // while the underlying text is preserved.
        let error = classify_rustypipe_error(rustypipe::error::Error::Http(
            "error sending request for url (https://music.youtube.com/youtubei/v1/search?prettyPrint=false)"
                .into(),
        ));
        assert_eq!(error.category(), crate::ErrorCategory::Network);
        assert!(error.is_retryable());
        assert_eq!(error.provider_id(), Some("youtube-music"));
        assert_eq!(error.detail(), Some("http error: error sending request"));
        let display = error.to_string();
        assert!(display.starts_with("network error: "));
        assert!(!display.contains("music.youtube.com"), "{display}");
        // Texts without the suffix pass through byte-exact.
        assert_eq!(
            strip_reqwest_url_suffix("http error: operation timed out"),
            "http error: operation timed out"
        );
    }

    #[test]
    fn rustypipe_ladder_exhaustion_splits_unavailable_from_invalid_response() {
        // RustyPipe retries 5xx + 429 internally (its should_retry) before the
        // failure surfaces, so both land in the ladder-exhaustion bucket
        // (m2-apple terminal_status_error precedent); everything else is an
        // unmapped status → InvalidResponse, code slot stays None (HTTP
        // statuses are not platform API codes).
        for code in [429u16, 500, 503] {
            let error = classify_rustypipe_error(rustypipe::error::Error::HttpStatus(
                code,
                "backend error".into(),
            ));
            assert_eq!(
                error.category(),
                crate::ErrorCategory::Unavailable,
                "{code}"
            );
            assert!(error.is_retryable(), "{code}");
            assert_eq!(error.provider_id(), Some("youtube-music"));
            let expected = format!("http status code: {code} message: backend error");
            assert_eq!(error.detail(), Some(expected.as_str()), "{code}");
        }
        for code in [400u16, 401, 403, 404] {
            let error = classify_rustypipe_error(rustypipe::error::Error::HttpStatus(
                code,
                "quota exceeded".into(),
            ));
            assert_eq!(
                error.category(),
                crate::ErrorCategory::InvalidResponse,
                "{code}"
            );
            assert!(!error.is_retryable(), "{code}");
            assert_eq!(error.provider_id(), Some("youtube-music"));
            assert!(
                error
                    .detail()
                    .unwrap()
                    .starts_with(&format!("http status code: {code} message: "))
            );
        }
    }

    #[test]
    fn extraction_not_found_keeps_the_legacy_not_found_semantics() {
        // The platform itself answers 404 for the requested content
        // (rustypipe maps NOT_FOUND to ExtractionError::NotFound,
        // client/mod.rs:2383); the legacy NotFound semantics survive.
        let error = classify_rustypipe_error(rustypipe::error::Error::Extraction(
            ExtractionError::NotFound {
                id: "aQvGIIdgFDM".to_owned(),
                msg: "404".into(),
            },
        ));
        assert_eq!(error.category(), crate::ErrorCategory::NotFound);
        assert!(!error.is_retryable());
        assert_eq!(error.provider_id(), None);
    }

    #[test]
    fn extraction_paid_refusal_keeps_the_drm_unsupported_semantics() {
        // UnavailabilityReason::Paid is RustyPipe's DRM refusal ("needs to be
        // purchased and is protected by digital restrictions management"),
        // the same refusal class as the player.drm check in resolve().
        let error = classify_rustypipe_error(rustypipe::error::Error::Extraction(
            ExtractionError::Unavailable {
                reason: UnavailabilityReason::Paid,
                msg: "rental".into(),
            },
        ));
        assert_eq!(error.category(), crate::ErrorCategory::Unsupported);
        assert!(!error.is_retryable());
        assert_eq!(error.provider_id(), None);
        assert!(
            error
                .to_string()
                .contains("the content is DRM-protected (paid)")
        );
    }

    #[test]
    fn extraction_unavailability_lands_in_unavailable_with_the_reason_preserved() {
        // The remaining content-level reasons share the ask-bounded
        // Unavailable bucket; the detail keeps the exact reason and the
        // YouTube message so hosts never lose the distinction.
        for (reason, expected) in [
            (UnavailabilityReason::Deleted, "deleted"),
            (UnavailabilityReason::Private, "private"),
            (UnavailabilityReason::TryAgain, "try again"),
        ] {
            let error = classify_rustypipe_error(rustypipe::error::Error::Extraction(
                ExtractionError::Unavailable {
                    reason,
                    msg: "as returned by YT".into(),
                },
            ));
            assert_eq!(
                error.category(),
                crate::ErrorCategory::Unavailable,
                "{expected}"
            );
            assert!(error.is_retryable(), "{expected}");
            assert_eq!(error.provider_id(), Some("youtube-music"));
            let expected_detail = format!(
                "extraction error: content unavailable ({expected}). Reason (from YT): as returned by YT"
            );
            assert_eq!(error.detail(), Some(expected_detail.as_str()), "{expected}");
        }
    }

    #[test]
    fn botguard_and_mapping_failures_split_unavailable_from_invalid_response() {
        let botguard = classify_rustypipe_error(rustypipe::error::Error::Extraction(
            ExtractionError::Botguard("no botguard executable found".into()),
        ));
        // PO-token attestation is a provider-side service/environment
        // condition, not a response violation.
        assert_eq!(botguard.category(), crate::ErrorCategory::Unavailable);
        assert!(botguard.is_retryable());
        assert_eq!(botguard.provider_id(), Some("youtube-music"));
        assert_eq!(
            botguard.detail(),
            Some("extraction error: botguard error: no botguard executable found")
        );
        for variant in [
            rustypipe::error::Error::Extraction(ExtractionError::InvalidData(
                "missing contents".into(),
            )),
            rustypipe::error::Error::Extraction(ExtractionError::WrongResult(
                "returned the 5-minute error message video".to_owned(),
            )),
        ] {
            let error = classify_rustypipe_error(variant);
            assert_eq!(error.category(), crate::ErrorCategory::InvalidResponse);
            assert!(!error.is_retryable());
            assert_eq!(error.provider_id(), Some("youtube-music"));
        }
    }

    #[test]
    fn auth_refusal_lands_in_authentication_required() {
        // RustyPipe's request path maps a 401 to AuthError::NoLogin ("you are
        // not logged in", client/mod.rs:2390); the declared auth refusal
        // keeps its category instead of a transport one.
        let error = classify_rustypipe_error(rustypipe::error::Error::Auth(AuthError::NoLogin));
        assert_eq!(
            error.category(),
            crate::ErrorCategory::AuthenticationRequired
        );
        assert!(!error.is_retryable());
        assert_eq!(error.provider_id(), Some("youtube-music"));
        assert_eq!(error.detail(), Some("auth error: you are not logged in"));
    }

    #[test]
    fn residue_arm_lands_in_invalid_response_with_the_text_preserved() {
        // Error::Other (unspecified local/parse shapes) and future RustyPipe
        // variants are off-contract from this provider's point of view; the
        // pre-migration catch-all text survives in the detail slot.
        let error = classify_rustypipe_error(rustypipe::error::Error::Other(
            "could not get local timezone".into(),
        ));
        assert_eq!(error.category(), crate::ErrorCategory::InvalidResponse);
        assert!(!error.is_retryable());
        assert_eq!(error.provider_id(), Some("youtube-music"));
        assert_eq!(error.detail(), Some("error: could not get local timezone"));
    }
}
