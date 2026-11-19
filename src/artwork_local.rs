//! Local artwork face (M6, RESOURCE_DESIGN.md §1.4): reuses the M5 s8
//! cover-read face ([`crate::local_cover::read_cover`], embedded/external
//! with priority) as an [`crate::artwork::ArtworkProvider`] source. Gated
//! on `cfg(all(feature = "artwork", feature = "local-cover"))`; the
//! `artwork-local` gate feature names that union so outer gates can select
//! it by name (the M5 s3 `cfg`-union precedent).
//!
//! The on-disk cover cache is already owned by `read_cover` (host-specified
//! directory, decode-validated store, corrupt rebuild), so this face adds
//! no second cache layer; in-memory caching belongs to
//! [`crate::artwork::ArtworkCache`] and is optional. The resolution does
//! bounded filesystem work (one tag read or one directory listing), the
//! same shape as the sync local scan face.
use crate::artwork::{
    Artwork, ArtworkFormat, ArtworkKey, ArtworkProvider, ArtworkSize, ArtworkVariant, unix_now,
};
use crate::local_cover::{self, CoverPriority};
use crate::provider::MusicProvider;
use std::path::PathBuf;

pub struct LocalArtwork {
    cache_dir: PathBuf,
    priority: CoverPriority,
}
impl LocalArtwork {
    pub fn new(cache_dir: impl Into<PathBuf>, priority: CoverPriority) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            priority,
        }
    }
}
impl MusicProvider for LocalArtwork {
    fn id(&self) -> &str {
        "local"
    }
    fn name(&self) -> &str {
        "Local"
    }
}

#[async_trait::async_trait]
impl ArtworkProvider for LocalArtwork {
    /// Resolve `ArtworkKey::Local` through `read_cover`. Track/Album/Artist
    /// keys fail with `Error::InvalidInput` (this source has no
    /// provider-side ids). `Ok(None)` = neither the embedded nor the
    /// external face produced a usable path, exactly `read_cover`'s `None`.
    async fn artwork(
        &self,
        key: &ArtworkKey,
        _size: ArtworkSize,
    ) -> crate::Result<Option<Artwork>> {
        let audio_path = match key {
            ArtworkKey::Local { audio_path } => audio_path,
            _ => {
                return Err(crate::Error::InvalidInput(
                    "local artwork source only resolves ArtworkKey::Local".into(),
                ));
            }
        };
        match local_cover::read_cover(audio_path, &self.cache_dir, self.priority) {
            Some(path) => Ok(Some(Artwork {
                provider: self.id().to_owned(),
                key: key.clone(),
                variants: vec![variant(&path)],
                resolved_at_unix: unix_now(),
                expires_at_unix: None,
            })),
            None => Ok(None),
        }
    }
}

/// Decorate a resolved path with its sniffed format and decoded dimensions.
/// Both are best-effort: an unreadable file would not have come out of
/// `read_cover` (the embedded face stores decode-validated bytes), but a
/// concurrently deleted file degrades to a path-only variant instead of an
/// error.
fn variant(path: &std::path::Path) -> ArtworkVariant {
    let dimensions = local_cover::probe_dimensions(path);
    ArtworkVariant {
        url: None,
        local_path: Some(path.to_path_buf()),
        width: dimensions.map(|(width, _)| width),
        height: dimensions.map(|(_, height)| height),
        format: sniff_format(path),
        bytes_sha256: None,
        expires_at_unix: None,
        headers: Vec::new(),
    }
}

/// Sniff the image format (the decode-validated cache stores real content
/// under a placeholder `.jpg` name, local_cover.rs `embedded_cover_path`).
/// Content detection first; `with_guessed_format` falls back to the file
/// extension when the header is unrecognizable, so an external `folder.jpg`
/// with junk content still reports `Jpeg` (extension-truthful). Unknown
/// only when neither content nor extension names a format.
fn sniff_format(path: &std::path::Path) -> ArtworkFormat {
    image::ImageReader::open(path)
        .ok()
        .and_then(|reader| reader.with_guessed_format().ok())
        .and_then(|reader| reader.format())
        .map(|format| match format {
            image::ImageFormat::Png => ArtworkFormat::Png,
            image::ImageFormat::Jpeg => ArtworkFormat::Jpeg,
            image::ImageFormat::WebP => ArtworkFormat::Webp,
            image::ImageFormat::Gif => ArtworkFormat::Gif,
            _ => ArtworkFormat::Unknown,
        })
        .unwrap_or(ArtworkFormat::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TrackId;
    use std::fs;
    use std::path::PathBuf;

    /// Committed ffmpeg-built MP3 fixture (the local_cover test precedent).
    const MP3_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/local/spec_mp3_cbr128.mp3");

    /// Synthetic PNG via the local `image` dependency (pulled by
    /// local-cover with the png feature).
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    struct TempDir {
        root: PathBuf,
    }
    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("musid-dli-artwork-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self {
                root: fs::canonicalize(dir).unwrap(),
            }
        }
        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn tagged_audio(dir: &TempDir, name: &str) -> PathBuf {
        let audio = dir.path(name);
        fs::write(&audio, MP3_FIXTURE).unwrap();
        crate::tag_write::write_metadata(
            &audio,
            &crate::tag_write::MetadataEdit {
                cover: Some(png_bytes(2, 2)),
                ..Default::default()
            },
        )
        .unwrap();
        audio
    }

    /// R13: the embedded face resolves through the content-addressed
    /// cache, with dimensions and sniffed format; a second resolution hits
    /// the same cache path.
    #[tokio::test]
    async fn local_artwork_resolves_the_embedded_face() {
        let temp = TempDir::new("embedded");
        let cache_dir = temp.path("covers");
        let audio = tagged_audio(&temp, "a.mp3");
        let source = LocalArtwork::new(&cache_dir, CoverPriority::EmbeddedFirst);
        let key = ArtworkKey::Local {
            audio_path: audio.clone(),
        };
        let resolved = source.artwork(&key, ArtworkSize::Original).await.unwrap();
        let resolved = resolved.expect("embedded cover resolves");
        assert_eq!(resolved.provider, "local");
        assert_eq!(resolved.key, key);
        assert_eq!(resolved.variants.len(), 1);
        let variant = &resolved.variants[0];
        assert!(variant.url.is_none());
        let path = variant.local_path.as_ref().unwrap();
        assert!(
            path.starts_with(&cache_dir),
            "cache file lives under the host dir"
        );
        assert_eq!(variant.width, Some(2));
        assert_eq!(variant.height, Some(2));
        assert_eq!(variant.format, ArtworkFormat::Png);
        // Second resolution: the same content-addressed cache file.
        let again = source
            .artwork(&key, ArtworkSize::Side(300))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.variants[0].local_path, variant.local_path);
    }

    /// R13: the external face and the priority flip; a file without any
    /// cover is explicit absence, never a placeholder.
    #[tokio::test]
    async fn local_artwork_serves_external_and_explicit_absence() {
        let temp = TempDir::new("external");
        let cache_dir = temp.path("covers");
        // Untagged WAV stub: no embedded face possible.
        let audio = temp.path("b.wav");
        fs::write(&audio, b"RIFF").unwrap();
        let folder = temp.path("folder.jpg");
        // A real JPEG payload is not needed: the external face returns the
        // path as-is (never copied, never decode-validated), and format
        // sniffing falls back to the file extension for junk content
        // (extension-truthful, content-first).
        fs::write(&folder, b"not really a jpeg").unwrap();
        let source = LocalArtwork::new(&cache_dir, CoverPriority::ExternalFirst);
        let key = ArtworkKey::Local {
            audio_path: audio.clone(),
        };
        let resolved = source
            .artwork(&key, ArtworkSize::Original)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            resolved.variants[0]
                .local_path
                .as_ref()
                .unwrap()
                .file_name()
                .unwrap(),
            "folder.jpg"
        );
        assert_eq!(resolved.variants[0].format, ArtworkFormat::Jpeg);
        assert_eq!(
            resolved.variants[0].width, None,
            "junk content has no decodable dimensions"
        );
        // EmbeddedFirst on the same file: the embedded face is empty, the
        // external face is still the fallback.
        let embedded_first = LocalArtwork::new(&cache_dir, CoverPriority::EmbeddedFirst);
        let resolved = embedded_first
            .artwork(&key, ArtworkSize::Original)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            resolved.variants[0]
                .local_path
                .as_ref()
                .unwrap()
                .file_name()
                .unwrap(),
            "folder.jpg"
        );
        // Neither face: explicit absence (no directory art, no tag).
        let empty = TempDir::new("empty");
        let bare = empty.path("c.wav");
        fs::write(&bare, b"RIFF").unwrap();
        let resolved = embedded_first
            .artwork(
                &ArtworkKey::Local { audio_path: bare },
                ArtworkSize::Original,
            )
            .await
            .unwrap();
        assert_eq!(
            resolved, None,
            "missing artwork is Ok(None), never a placeholder"
        );
    }

    /// R13: non-local keys are a caller fault, not a lookup miss.
    #[tokio::test]
    async fn non_local_keys_fail_with_invalid_input() {
        let temp = TempDir::new("keys");
        let source = LocalArtwork::new(temp.path("covers"), CoverPriority::EmbeddedFirst);
        let error = source
            .artwork(
                &ArtworkKey::Track(TrackId::new("netease", "1")),
                ArtworkSize::Original,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, crate::Error::InvalidInput(_)));
        assert!(matches!(
            source
                .artwork(
                    &ArtworkKey::Album {
                        provider: "qq".into(),
                        id: "1".into()
                    },
                    ArtworkSize::Original
                )
                .await
                .unwrap_err(),
            crate::Error::InvalidInput(_)
        ));
    }

    /// The provider descriptor: the local face registers under the `local`
    /// id with no capabilities (M6 §1.4).
    #[test]
    fn local_artwork_provider_descriptor() {
        let temp = TempDir::new("descriptor");
        let source = LocalArtwork::new(temp.path("covers"), CoverPriority::EmbeddedFirst);
        assert_eq!(source.id(), "local");
        assert_eq!(source.name(), "Local");
        assert!(
            !source.capabilities().artwork,
            "capabilities stay descriptor-level"
        );
    }
}
