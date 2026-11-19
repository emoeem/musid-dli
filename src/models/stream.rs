use serde::{Deserialize, Serialize};
/// Tiers are ordered Low < Standard < High < Lossless < HiRes; Unknown sorts
/// last so fallback selection never treats it as a real tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum StreamQuality {
    Low,
    Standard,
    High,
    Lossless,
    HiRes,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AudioFormat {
    Mp3,
    Flac,
    Aac,
    M4a,
    Opus,
    Ogg,
    Wav,
    #[default]
    Unknown,
}
/// Half-open byte range [start, end) required by segmented/DASH resources.
///
/// Descriptive metadata for playback hosts (MODEL_ALIGNMENT_DESIGN.md
/// §2.6): the download engine does not implement DASH segment fetch, so
/// these ranges are never download inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}
/// Algorithm a source declared for [`StreamInfo::checksum`].
///
/// Only reachable through the defaulted `checksum_algorithm` field, so legacy
/// JSON cannot name it (MODEL_ALIGNMENT_DESIGN.md R3). `None` on the field
/// means MD5: it is the only algorithm the download engine verifies today
/// (`verify_md5_sync`, download.rs) and the only one voicefox carries (the
/// upstream field is literally `md5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumAlgorithm {
    #[default]
    Md5,
    /// Reserved; not verified by the download engine yet. A stream carrying
    /// it drops its checksum at the `DownloadRequest` boundary instead of
    /// being reinterpreted as MD5 (§2.3).
    Sha1,
    /// Reserved; same treatment as `Sha1`.
    Sha256,
}
/// One structured backup endpoint for a resolved stream.
///
/// Model answer to the shared structural defect of both trees: the fallback
/// lists are bare strings while the engine applies the primary URL's
/// headers/size/checksum to every candidate (voicefox engine.rs:294-377,
/// musid-dli download.rs:441-520), which breaks the size gate as soon as a
/// candidate is a different stream. Producers that can describe a backup
/// beyond its URL fill this instead of (or in addition to, see
/// [`StreamInfo::ordered_fallback_urls`]) the legacy `fallback_urls` slot.
/// Consuming the per-endpoint metadata inside the engine loop is the
/// follow-up download-engine slice; the model carries the data now so that
/// migration needs no second serde break (§2.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackEndpoint {
    pub url: String,
    /// Request headers this endpoint requires. May differ from the primary
    /// URL's headers (per-host Referer/UA).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// When this URL stops working, unix seconds. The stream-level
    /// `expires_at_unix` stays the cache-freshness carrier.
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
    /// Declared size in bytes, if the source states it for this endpoint.
    /// Named after `StreamInfo::content_length`, not voicefox `size`.
    #[serde(default)]
    pub content_length: Option<u64>,
    /// Quality tier of the stream this endpoint serves (`None` = same tier
    /// as the primary). Needed because real fallback lists mix tiers
    /// (youtube-music alternate bitrates, migu cross-tier candidates).
    #[serde(default)]
    pub quality: Option<StreamQuality>,
}

/// Resolved URL and transport metadata for a host application's player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub url: String,
    pub quality: StreamQuality,
    #[serde(default)]
    pub format: AudioFormat,
    #[serde(default)]
    pub duration: Option<std::time::Duration>,
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
    #[serde(default)]
    pub content_length: Option<u64>,
    #[serde(default)]
    pub checksum: Option<String>,
    /// How to verify `checksum` (MODEL_ALIGNMENT_DESIGN.md §2.3). `None`
    /// means MD5, which keeps legacy JSON (checksum without algorithm)
    /// behaving exactly as before.
    #[serde(default)]
    pub checksum_algorithm: Option<ChecksumAlgorithm>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Legacy string fallback slot, kept for compatibility (§2.1). New
    /// producers should prefer [`StreamInfo::fallback_endpoints`].
    #[serde(default)]
    pub fallback_urls: Vec<String>,
    /// Structured backup endpoints with per-URL transport metadata (§2.4).
    #[serde(default)]
    pub fallback_endpoints: Vec<FallbackEndpoint>,
    #[serde(default)]
    pub index_range: Option<ByteRange>,
    #[serde(default)]
    pub init_range: Option<ByteRange>,
    #[serde(default)]
    pub max_chunk_size: Option<u64>,
    /// The declared size is unreliable (some sources under-report by a few
    /// bytes): only reject files shorter than the declared value. Default
    /// `false` = byte-exact, the pre-M3 behavior.
    #[serde(default)]
    pub size_is_advisory: bool,
}
impl StreamInfo {
    /// The checksum usable by the engine's MD5 gate: `checksum` only when
    /// the algorithm is absent (legacy JSON) or explicitly MD5.
    ///
    /// Whitespace semantics are a deliberate behavior change versus a naive
    /// field copy, not a match of the engine rule: `verify_md5_sync` skips
    /// only an *exactly-empty* expectation, so whitespace-padded or
    /// whitespace-only checksums used to fail the hash compare. This adapter
    /// trims first: padded digests verify, whitespace-only values skip
    /// (MODEL_ALIGNMENT_DESIGN.md §2.3, pinned by the models and download
    /// tests).
    pub fn md5_checksum(&self) -> Option<&str> {
        let md5 = matches!(
            self.checksum_algorithm.unwrap_or(ChecksumAlgorithm::Md5),
            ChecksumAlgorithm::Md5
        );
        if !md5 {
            return None;
        }
        self.checksum
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// Ordered fallback URL list merging both slots (MODEL_ALIGNMENT_DESIGN.md
    /// §2.4): legacy `fallback_urls` first, then structured
    /// `fallback_endpoints`, trimmed, empties dropped, deduplicated keeping
    /// the first occurrence. The primary `url` is not included; the engine's
    /// `candidate_download_urls` owns the primary-first ordering and the
    /// NetEase node rewrite.
    pub fn ordered_fallback_urls(&self) -> Vec<String> {
        let mut urls: Vec<String> = Vec::new();
        let mut push = |value: &str| {
            let value = value.trim();
            if value.is_empty() || urls.iter().any(|existing| existing == value) {
                return;
            }
            urls.push(value.to_string());
        };
        for url in &self.fallback_urls {
            push(url);
        }
        for endpoint in &self.fallback_endpoints {
            push(&endpoint.url);
        }
        urls
    }

    /// Structured consumer face of the same fallback merge (M6,
    /// RESOURCE_DESIGN.md §6.3): legacy `fallback_urls` are lifted to
    /// minimal [`FallbackEndpoint`]s (url only, every metadata field
    /// `None`/empty), structured `fallback_endpoints` keep their per-URL
    /// headers/expiry/size/quality. Ordering, trimming, empty-dropping and
    /// keep-first dedup are identical to [`StreamInfo::ordered_fallback_urls`],
    /// and the primary `url` is never included. Consumers that need the
    /// per-candidate metadata (engine candidate switching, hosts) read this
    /// instead of re-parsing the string list; the list order is the
    /// try-order, nothing here fetches anything.
    pub fn ordered_fallback_endpoints(&self) -> Vec<FallbackEndpoint> {
        let mut endpoints: Vec<FallbackEndpoint> = Vec::new();
        let mut push = |endpoint: FallbackEndpoint| {
            let url = endpoint.url.trim().to_string();
            if url.is_empty() || endpoints.iter().any(|existing| existing.url == url) {
                return;
            }
            endpoints.push(FallbackEndpoint { url, ..endpoint });
        };
        for url in &self.fallback_urls {
            push(FallbackEndpoint {
                url: url.clone(),
                headers: Vec::new(),
                expires_at_unix: None,
                content_length: None,
                quality: None,
            });
        }
        for endpoint in &self.fallback_endpoints {
            push(endpoint.clone());
        }
        endpoints
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_str, from_value, to_value};

    /// T1 (§7): pre-M3 JSON, no new keys, must load with documented defaults.
    #[test]
    fn legacy_json_without_new_fields_loads_with_documented_defaults() {
        let legacy = r#"{
            "url": "https://cdn.example/a.mp3",
            "quality": "High",
            "format": "Mp3",
            "content_length": 123,
            "checksum": "abc",
            "headers": [["Referer", "https://x.example"]],
            "fallback_urls": ["https://cdn.example/b.mp3"],
            "max_chunk_size": 1048576
        }"#;
        let stream: StreamInfo = from_str(legacy).expect("legacy json loads");
        assert!(!stream.size_is_advisory);
        assert_eq!(stream.checksum_algorithm, None);
        assert!(stream.fallback_endpoints.is_empty());
        assert_eq!(stream.content_length, Some(123));
        assert_eq!(stream.md5_checksum(), Some("abc"));
    }

    /// T2 (§7): fully populated round trip, including the new structures.
    #[test]
    fn round_trip_preserves_every_field() {
        let stream = StreamInfo {
            url: "https://cdn.example/a.flac".into(),
            quality: StreamQuality::Lossless,
            format: AudioFormat::Flac,
            duration: Some(std::time::Duration::from_millis(241_500)),
            expires_at_unix: Some(1_800_000_000),
            content_length: Some(42),
            checksum: Some("deadbeef".into()),
            checksum_algorithm: Some(ChecksumAlgorithm::Sha256),
            headers: vec![("User-Agent".into(), "ua".into())],
            fallback_urls: vec!["https://cdn.example/legacy.mp3".into()],
            fallback_endpoints: vec![FallbackEndpoint {
                url: "https://cdn2.example/a.flac".into(),
                headers: vec![("Referer".into(), "https://r.example".into())],
                expires_at_unix: Some(1_800_000_100),
                content_length: Some(43),
                quality: Some(StreamQuality::High),
            }],
            index_range: Some(ByteRange { start: 0, end: 99 }),
            init_range: Some(ByteRange {
                start: 100,
                end: 199,
            }),
            max_chunk_size: Some(1 << 20),
            size_is_advisory: true,
        };
        let json = to_value(&stream).expect("serialize");
        let rebuilt: StreamInfo = from_value(json).expect("deserialize");
        assert_eq!(rebuilt, stream);
    }

    /// T3 (§7): both slots merge legacy-first, trimmed, deduplicated, and the
    /// primary URL is never auto-included.
    #[test]
    fn fallback_slot_ordering_and_dedup_is_pinned() {
        let stream = StreamInfo {
            url: "https://cdn.example/main.mp3".into(),
            quality: StreamQuality::High,
            format: AudioFormat::Mp3,
            duration: None,
            expires_at_unix: None,
            content_length: None,
            checksum: None,
            checksum_algorithm: None,
            headers: vec![],
            fallback_urls: vec![
                "https://cdn.example/b.mp3".into(),
                "  https://cdn.example/b.mp3  ".into(),
                " ".into(),
                "https://cdn.example/c.mp3".into(),
            ],
            fallback_endpoints: vec![
                FallbackEndpoint {
                    url: "https://cdn.example/b.mp3".into(),
                    headers: vec![],
                    expires_at_unix: None,
                    content_length: None,
                    quality: None,
                },
                FallbackEndpoint {
                    url: "  https://cdn.example/d.mp3  ".into(),
                    headers: vec![],
                    expires_at_unix: None,
                    content_length: None,
                    quality: None,
                },
            ],
            index_range: None,
            init_range: None,
            max_chunk_size: None,
            size_is_advisory: false,
        };
        assert_eq!(
            stream.ordered_fallback_urls(),
            vec![
                "https://cdn.example/b.mp3".to_string(),
                "https://cdn.example/c.mp3".to_string(),
                "https://cdn.example/d.mp3".to_string(),
            ]
        );
    }

    /// T4 (§7): the MD5 gate sees the checksum only for absent/MD5 algorithms.
    #[test]
    fn md5_checksum_only_for_md5_algorithm() {
        let base = |algorithm: Option<ChecksumAlgorithm>, checksum: &str| StreamInfo {
            url: "https://cdn.example/a.mp3".into(),
            quality: StreamQuality::Low,
            format: AudioFormat::Mp3,
            duration: None,
            expires_at_unix: None,
            content_length: None,
            checksum: Some(checksum.into()),
            checksum_algorithm: algorithm,
            headers: vec![],
            fallback_urls: vec![],
            fallback_endpoints: vec![],
            index_range: None,
            init_range: None,
            max_chunk_size: None,
            size_is_advisory: false,
        };
        // Absent algorithm = MD5 (legacy JSON semantics).
        assert_eq!(base(None, "abc").md5_checksum(), Some("abc"));
        assert_eq!(
            base(Some(ChecksumAlgorithm::Md5), "abc").md5_checksum(),
            Some("abc")
        );
        // Declared non-MD5 must never reach the MD5 gate.
        assert_eq!(
            base(Some(ChecksumAlgorithm::Sha1), "abc").md5_checksum(),
            None
        );
        assert_eq!(
            base(Some(ChecksumAlgorithm::Sha256), "abc").md5_checksum(),
            None
        );
        // Empty/whitespace checksums skip the check.
        assert_eq!(base(None, "").md5_checksum(), None);
        assert_eq!(base(None, "   ").md5_checksum(), None);
    }

    /// M6 §6.3 (R9): the structured fallback consumer mirrors the T3 string
    /// ordering exactly, lifts legacy strings to minimal endpoints, and
    /// keeps the per-endpoint metadata of structured entries (including a
    /// duplicate structured URL winning over its legacy twin).
    #[test]
    fn ordered_fallback_endpoints_mirror_string_ordering_with_metadata() {
        let stream = StreamInfo {
            url: "https://cdn.example/main.mp3".into(),
            quality: StreamQuality::High,
            format: AudioFormat::Mp3,
            duration: None,
            expires_at_unix: None,
            content_length: None,
            checksum: None,
            checksum_algorithm: None,
            headers: vec![],
            fallback_urls: vec![
                "https://cdn.example/b.mp3".into(),
                "  https://cdn.example/c.mp3  ".into(),
                " ".into(),
                "https://cdn.example/b.mp3".into(),
            ],
            fallback_endpoints: vec![FallbackEndpoint {
                url: "https://cdn.example/d.mp3".into(),
                headers: vec![("Referer".into(), "https://r.example".into())],
                expires_at_unix: Some(1_800_000_100),
                content_length: Some(43),
                quality: Some(StreamQuality::High),
            }],
            index_range: None,
            init_range: None,
            max_chunk_size: None,
            size_is_advisory: false,
        };
        // String face and endpoint face list the same URLs in the same
        // order, primary excluded.
        let urls: Vec<String> = stream
            .ordered_fallback_endpoints()
            .iter()
            .map(|endpoint| endpoint.url.clone())
            .collect();
        assert_eq!(urls, stream.ordered_fallback_urls());
        assert_eq!(
            urls,
            vec![
                "https://cdn.example/b.mp3".to_string(),
                "https://cdn.example/c.mp3".to_string(),
                "https://cdn.example/d.mp3".to_string(),
            ]
        );
        // Legacy strings carry no metadata...
        assert_eq!(stream.ordered_fallback_endpoints()[0].headers, vec![]);
        assert_eq!(stream.ordered_fallback_endpoints()[0].quality, None);
        // ...structured entries keep theirs.
        let structured = &stream.ordered_fallback_endpoints()[2];
        assert_eq!(
            structured.headers,
            vec![("Referer".into(), "https://r.example".into())]
        );
        assert_eq!(structured.expires_at_unix, Some(1_800_000_100));
        assert_eq!(structured.content_length, Some(43));
        assert_eq!(structured.quality, Some(StreamQuality::High));
    }
}
