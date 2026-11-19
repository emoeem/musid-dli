//! Artwork resolver face (M6, RESOURCE_DESIGN.md §1): the trait, the model
//! types and the bounded artwork cache all compile without any feature
//! gate, so third-party providers can implement the face while paying for
//! none of our implementations. The concrete local source lives in
//! [`crate::artwork_local`] behind `cfg(all(feature = "artwork", feature =
//! "local-cover"))` (selected by the `artwork-local` gate feature).
//!
//! Contract pins (RESOURCE_DESIGN.md §1.2):
//! - no default method bodies (the no-empty-shells rule, tasks.md §1.3#7):
//!   a provider implements the face fully or does not register the slot;
//! - `Ok(None)` = looked up, genuinely absent. Missing artwork is never
//!   synthesized into a placeholder by this crate;
//! - a kind a provider cannot serve is `Error::Unsupported`, never
//!   `Ok(None)`;
//! - cache keys are `TrackId` / provider-scoped album/artist ids / local
//!   paths: stable identity, never a song title.
use crate::provider::MusicProvider;
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// What is being resolved. The provider-scoped id fields mirror `TrackId`
/// semantics (models/track.rs:5-17): platform ids are not globally unique.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArtworkKey {
    /// Song cover, keyed by the provider-scoped track id.
    Track(crate::models::TrackId),
    /// Album cover, keyed by (provider, album id).
    Album { provider: String, id: String },
    /// Artist avatar, keyed by (provider, artist id).
    Artist { provider: String, id: String },
    /// Local embedded/external artwork, keyed by the audio file path. The
    /// path is the local track's identity (the M5 content-hash keys below
    /// it are derived from it, local_cover.rs:118-146). `cache_key`
    /// canonicalizes first so one file maps to one key across path
    /// spellings, and encodes the raw path bytes losslessly (RESOURCE_DESIGN.md
    /// §1.4).
    Local { audio_path: PathBuf },
}
impl ArtworkKey {
    /// Stable cache key (RESOURCE_DESIGN.md §0): ids, canonicalized paths
    /// and hashes only. The string is exact-match stable; no title ever
    /// enters it. The id part is length-prefixed, so the encoding is
    /// injective even when a provider or resource id contains the
    /// separator byte (an id string can never shift the provider/id split).
    pub fn cache_key(&self) -> String {
        match self {
            Self::Track(id) => format!("track:{}:{}:{}", id.provider.len(), id.provider, id.id),
            Self::Album { provider, id } => {
                format!("album:{}:{}:{}", provider.len(), provider, id)
            }
            Self::Artist { provider, id } => {
                format!("artist:{}:{}:{}", provider.len(), provider, id)
            }
            Self::Local { audio_path } => {
                // Canonicalize first: resolves relative spellings, `..`
                // segments and symlinks so the same file spelled differently
                // maps to one key. A non-existent path cannot be
                // canonicalized; the verbatim path is used (the lookup will
                // report absence anyway, and the absence entry expires on
                // the missing-domain TTL).
                let resolved = audio_path
                    .canonicalize()
                    .unwrap_or_else(|_| audio_path.clone());
                format!("local:{}", local_path_key(&resolved))
            }
        }
    }
}

/// Collision-free key text for a resolved path. On unix the raw `OsStr`
/// bytes are hex-encoded: lossless for non-UTF-8 file names, so two
/// different files can never collapse into one key (a lossy
/// `to_string_lossy` could map two distinct non-UTF-8 paths to the same
/// text and poison both cache domains). On other platforms std offers no
/// byte view of a path, so the lossy string is the only option; the
/// platform note is documented in RESOURCE_DESIGN.md §1.4.
fn local_path_key(path: &std::path::Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let mut hex = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
        }
        hex
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned()
    }
}

/// Request-side size hint (RESOURCE_DESIGN.md §1.2). Providers map this
/// onto their own discrete size parameters and return whatever variants
/// they actually have; `Side(px)` is a square side length in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ArtworkSize {
    /// The provider's native/maximum size.
    #[default]
    Original,
    /// A square of `px` on a side (best effort).
    Side(u32),
}

/// Image format sniffed from content or declared by the producer. Sniffed,
/// never trusted: a mismatching declaration degrades to [`ArtworkFormat::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ArtworkFormat {
    Png,
    Jpeg,
    Webp,
    Gif,
    #[default]
    Unknown,
}

/// One resolvable artwork rendition (RESOURCE_DESIGN.md §1.2). `url` is
/// set for network sources, `local_path` for local ones; at least one is
/// `Some` on any variant a source returns.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ArtworkVariant {
    pub url: Option<String>,
    pub local_path: Option<PathBuf>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(default)]
    pub format: ArtworkFormat,
    /// Content hash of the image bytes when the producer has them (sha256
    /// lowercase hex, the `local_cover::content_hash` recipe). A
    /// dedup/persistence key for hosts; never required.
    #[serde(default)]
    pub bytes_sha256: Option<String>,
    /// When this rendition's URL stops working, unix seconds.
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
    /// Request headers this rendition requires (Referer/UA are common on
    /// cover CDNs; the same reason `StreamInfo` carries them).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

/// A resolved artwork: provider, key, and the size variants in try-order
/// (best first). `expires_at_unix` is the artwork-level freshness carrier
/// (variant-level expiry may differ per rendition).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artwork {
    /// The provider id that resolved this (must match the registry id).
    pub provider: String,
    pub key: ArtworkKey,
    /// Ordered, best first. Empty means the provider had the key but no
    /// usable rendition; prefer returning `Ok(None)` in that case instead.
    pub variants: Vec<ArtworkVariant>,
    /// Unix seconds when this resolution happened.
    pub resolved_at_unix: u64,
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
}

/// The artwork resolver face (RESOURCE_DESIGN.md §1). Registered in the
/// twelfth registry slot; no default method bodies: implement fully or do
/// not register.
#[async_trait::async_trait]
pub trait ArtworkProvider: MusicProvider {
    /// Resolve artwork for `key` at `size`. `Ok(None)` = genuinely absent;
    /// errors are lookup failures (`Error::Unsupported` for a kind this
    /// provider cannot serve at all).
    async fn artwork(&self, key: &ArtworkKey, size: ArtworkSize) -> crate::Result<Option<Artwork>>;
}

/// Configuration of [`ArtworkCache`] (RESOURCE_DESIGN.md §2.2 defaults:
/// positive 1 h, missing 10 min, capacity 256 entries per domain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtworkCacheConfig {
    pub ttl: Duration,
    pub missing_ttl: Duration,
    pub capacity: usize,
}
impl Default for ArtworkCacheConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(3600),
            missing_ttl: Duration::from_secs(600),
            capacity: 256,
        }
    }
}

/// Bounded TTL cache for resolved artwork (RESOURCE_DESIGN.md §1.4/§2.2):
/// the positive domain stores [`Artwork`] values keyed by
/// [`ArtworkKey::cache_key`], the negative domain stores an explicit
/// "genuinely absent" so an absent cover is not re-requested on every
/// paint. The positive TTL is capped by the value's own expiry
/// ([`Artwork::expires_at_unix`] and the fastest-expiring
/// [`ArtworkVariant::expires_at_unix`]): a signed cover URL never outlives
/// its validity, the same absolute-cap rule as `StreamCache`
/// (cache.rs:111-123). Network faces are expected to sit behind this; the
/// local face uses the on-disk content-addressed cache instead.
pub struct ArtworkCache {
    /// The configured positive TTL ceiling (the expiry cap can only
    /// shorten an entry, never extend it).
    ttl: Duration,
    positive: super::cache::TtlCache<String, Artwork>,
    missing: super::cache::TtlCache<String, ()>,
}
impl ArtworkCache {
    pub fn new(config: ArtworkCacheConfig) -> Self {
        Self {
            ttl: config.ttl,
            positive: super::cache::TtlCache::new(config.ttl, config.capacity),
            missing: super::cache::TtlCache::new(config.missing_ttl, config.capacity),
        }
    }
    /// The effective positive TTL for one artwork: the configured TTL
    /// capped by the artwork-level expiry and the fastest variant expiry,
    /// whichever is sooner (remaining seconds saturate at zero, so an
    /// already-expired URL is not cached at all).
    fn positive_ttl(&self, artwork: &Artwork) -> Duration {
        let now = unix_now();
        let mut remaining = self.ttl.as_secs();
        if let Some(until) = artwork.expires_at_unix {
            remaining = remaining.min(until.saturating_sub(now));
        }
        for variant in &artwork.variants {
            if let Some(until) = variant.expires_at_unix {
                remaining = remaining.min(until.saturating_sub(now));
            }
        }
        Duration::from_secs(remaining)
    }
    /// `None` = not cached; `Some(None)` = cached absence.
    pub fn get(&self, key: &ArtworkKey) -> Option<Option<Artwork>> {
        let cache_key = key.cache_key();
        if let Some(artwork) = self.positive.get(&cache_key) {
            return Some(Some(artwork));
        }
        if self.missing.get(&cache_key).is_some() {
            return Some(None);
        }
        None
    }
    pub fn put(&self, key: &ArtworkKey, artwork: Option<&Artwork>) {
        let cache_key = key.cache_key();
        match artwork {
            Some(artwork) => {
                self.missing.invalidate(&cache_key);
                self.positive.insert_with_ttl(
                    cache_key,
                    artwork.clone(),
                    self.positive_ttl(artwork),
                );
            }
            None => {
                self.positive.invalidate(&cache_key);
                self.missing.insert(cache_key, ());
            }
        }
    }
    pub fn invalidate(&self, key: &ArtworkKey) {
        let cache_key = key.cache_key();
        self.positive.invalidate(&cache_key);
        self.missing.invalidate(&cache_key);
    }
    pub fn clear(&self) {
        self.positive.clear();
        self.missing.clear();
    }
    pub fn len(&self) -> usize {
        self.positive.len() + self.missing.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Unix seconds now (the `resolved_at_unix` producer).
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R13: the cache-key table is pinned; ids/canonicalized paths/hashes
    /// only, no title can enter by construction (the key types carry no
    /// title field).
    #[test]
    fn cache_keys_are_stable_id_shaped() {
        // Length-prefixed provider part: the encoding is injective even
        // when ids contain the separator byte.
        assert_eq!(
            ArtworkKey::Track(crate::models::TrackId::new("netease", "1")).cache_key(),
            "track:7:netease:1"
        );
        assert_eq!(
            ArtworkKey::Album {
                provider: "qq".into(),
                id: "9".into()
            }
            .cache_key(),
            "album:2:qq:9"
        );
        assert_eq!(
            ArtworkKey::Artist {
                provider: "qq".into(),
                id: "7".into()
            }
            .cache_key(),
            "artist:2:qq:7"
        );
        // Collision regression: `(provider, id)` splits of different shapes
        // ("a:b", "c" vs "a", "b:c") can never share a key.
        assert_ne!(
            ArtworkKey::Album {
                provider: "a:b".into(),
                id: "c".into()
            }
            .cache_key(),
            ArtworkKey::Album {
                provider: "a".into(),
                id: "b:c".into()
            }
            .cache_key()
        );
        // Local keys: hex of the (canonicalized) path bytes; a non-existent
        // path falls back to the verbatim path, so the shape stays stable.
        let local = ArtworkKey::Local {
            audio_path: PathBuf::from("/music/a.flac"),
        }
        .cache_key();
        assert!(local.starts_with("local:"), "{local}");
        assert!(
            local["local:".len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "the path is hex-encoded: {local}"
        );
        // Stability: same key bytes, same string.
        assert_eq!(
            ArtworkKey::Track(crate::models::TrackId::new("netease", "1")).cache_key(),
            ArtworkKey::Track(crate::models::TrackId::new("netease", "1")).cache_key()
        );
        assert_eq!(local, local.clone());
        // Distinct paths stay distinct (no lossy collapse).
        assert_ne!(
            ArtworkKey::Local {
                audio_path: PathBuf::from("/music/a.flac")
            }
            .cache_key(),
            ArtworkKey::Local {
                audio_path: PathBuf::from("/music/b.flac")
            }
            .cache_key()
        );
    }

    /// Review P1: one file maps to one cache key across path spellings
    /// (`..` segments; symlinks on unix). Canonicalization happens inside
    /// `cache_key`, so the caller never has to normalize first.
    #[test]
    fn local_keys_survive_path_spelling_differences() {
        let dir =
            std::env::temp_dir().join(format!("musid-dli-artwork-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let audio = nested.join("a.flac");
        std::fs::write(&audio, b"audio").unwrap();
        // Absolute canonical spelling vs a spelling through `..` segments.
        let direct = ArtworkKey::Local {
            audio_path: audio.clone(),
        }
        .cache_key();
        let winding = ArtworkKey::Local {
            audio_path: dir.join("nested").join("..").join("nested").join("a.flac"),
        }
        .cache_key();
        assert_eq!(direct, winding, "one file, one key");
        #[cfg(unix)]
        {
            // A symlinked spelling resolves to the same key too.
            let link = dir.join("link.flac");
            let _ = std::fs::remove_file(&link);
            if std::os::unix::fs::symlink(&audio, &link).is_ok() {
                let via_link = ArtworkKey::Local { audio_path: link }.cache_key();
                assert_eq!(direct, via_link, "symlinks resolve to the same key");
            }
            // Lossy collapse regression: two distinct non-UTF-8 names must
            // never share a key (a to_string_lossy key would collide here).
            use std::os::unix::ffi::OsStringExt;
            let odd_a = dir.join(std::ffi::OsString::from_vec(vec![b'o', b'd', b'd', 0xFF]));
            let odd_b = dir.join(std::ffi::OsString::from_vec(vec![b'o', b'd', b'd', 0xFE]));
            std::fs::write(&odd_a, b"a").unwrap();
            std::fs::write(&odd_b, b"b").unwrap();
            assert_ne!(
                ArtworkKey::Local { audio_path: odd_a }.cache_key(),
                ArtworkKey::Local { audio_path: odd_b }.cache_key(),
                "distinct non-UTF-8 paths must not collapse"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn artwork(key: &ArtworkKey) -> Artwork {
        Artwork {
            provider: "netease".into(),
            key: key.clone(),
            variants: vec![ArtworkVariant {
                url: Some("https://cover.example/1.jpg".into()),
                local_path: None,
                width: Some(300),
                height: Some(300),
                format: ArtworkFormat::Jpeg,
                bytes_sha256: None,
                expires_at_unix: None,
                headers: vec![("Referer".into(), "https://example.invalid".into())],
            }],
            resolved_at_unix: unix_now(),
            expires_at_unix: None,
        }
    }

    /// R13: positive domain, explicit-absence domain, invalidation, and
    /// bounded capacity (earliest-expiry eviction via TtlCache).
    #[test]
    fn artwork_cache_covers_positive_missing_and_invalidation() {
        let cache = ArtworkCache::new(ArtworkCacheConfig::default());
        let key = ArtworkKey::Track(crate::models::TrackId::new("netease", "1"));
        assert_eq!(cache.get(&key), None, "uncached");
        cache.put(&key, Some(&artwork(&key)));
        assert_eq!(cache.get(&key).unwrap().unwrap().variants.len(), 1);
        // Absence is cached distinctly from "not looked up".
        let absent = ArtworkKey::Track(crate::models::TrackId::new("netease", "2"));
        cache.put(&absent, None);
        assert_eq!(cache.get(&absent), Some(None));
        // Putting absence clears a stale positive and vice versa.
        cache.put(&key, None);
        assert_eq!(cache.get(&key), Some(None));
        cache.put(&key, Some(&artwork(&key)));
        assert!(cache.get(&key).unwrap().is_some());
        cache.invalidate(&key);
        assert_eq!(cache.get(&key), None);
        assert_eq!(cache.len(), 1, "only the absence entry remains");
        cache.clear();
        assert!(cache.is_empty());
    }

    /// R13: the model round-trips through serde (§1 R1 pin, hosts persist
    /// artwork results).
    #[test]
    fn artwork_round_trips_through_serde() {
        let key = ArtworkKey::Album {
            provider: "netease".into(),
            id: "9".into(),
        };
        let artwork = artwork(&key);
        let json = serde_json::to_value(&artwork).unwrap();
        let rebuilt: Artwork = serde_json::from_value(json).unwrap();
        assert_eq!(rebuilt, artwork);
        // Legacy-style JSON without the additive fields loads.
        let minimal: Artwork = serde_json::from_str(
            r#"{"provider":"netease","key":{"Track":{"provider":"netease","id":"1"}},"variants":[],"resolved_at_unix":0}"#,
        )
        .unwrap();
        assert_eq!(
            minimal.key,
            ArtworkKey::Track(crate::models::TrackId::new("netease", "1"))
        );
        assert!(minimal.variants.is_empty());
    }

    /// Review P1: the positive TTL is capped by the value's own expiry,
    /// artwork-level and per-variant, so a signed cover URL never outlives
    /// its validity (the StreamCache absolute-cap rule).
    #[test]
    fn artwork_cache_caps_entry_ttl_by_the_fastest_expiry() {
        let cache = ArtworkCache::new(ArtworkCacheConfig::default());
        // Artwork-level expiry one second out: the entry (1 h TTL ceiling)
        // must die with the URL.
        let artwork_key = ArtworkKey::Track(crate::models::TrackId::new("netease", "9"));
        let mut expiring = artwork(&artwork_key);
        expiring.expires_at_unix = Some(unix_now() + 1);
        cache.put(&artwork_key, Some(&expiring));
        assert!(cache.get(&artwork_key).unwrap().is_some());
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(
            cache.get(&artwork_key),
            None,
            "a signed URL must not outlive its validity"
        );
        // Variant-level expiry caps the entry even when the artwork-level
        // slot carries none.
        let variant_key = ArtworkKey::Track(crate::models::TrackId::new("netease", "10"));
        let mut expiring_variant = artwork(&variant_key);
        expiring_variant.variants[0].expires_at_unix = Some(unix_now() + 1);
        cache.put(&variant_key, Some(&expiring_variant));
        assert!(cache.get(&variant_key).unwrap().is_some());
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(cache.get(&variant_key), None);
        // An already-expired URL is not cached at all.
        let stale_key = ArtworkKey::Track(crate::models::TrackId::new("netease", "11"));
        let mut stale = artwork(&stale_key);
        stale.expires_at_unix = Some(unix_now().saturating_sub(1));
        cache.put(&stale_key, Some(&stale));
        assert_eq!(cache.get(&stale_key), None);
        // Untouched expiry keeps the full configured TTL (regression guard:
        // the cap only shortens).
        let long_key = ArtworkKey::Track(crate::models::TrackId::new("netease", "12"));
        cache.put(&long_key, Some(&artwork(&long_key)));
        assert!(cache.get(&long_key).unwrap().is_some());
    }
}
