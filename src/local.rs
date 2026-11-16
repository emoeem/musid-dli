//! Local library metadata and scanning are separate from online providers.
use crate::{Error, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: u64,
    pub format: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTrack {
    pub path: PathBuf,
    pub metadata: LocalMetadata,
}

/// Local library interface; deliberately does not inherit MusicProvider.
pub trait LocalLibraryProvider: Send + Sync {
    fn scan(&self, root: &Path, recursive: bool) -> Result<Vec<LocalTrack>>;
    fn read_track(&self, path: &Path) -> Result<LocalTrack>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FilesystemLibrary;
impl LocalLibraryProvider for FilesystemLibrary {
    fn scan(&self, root: &Path, recursive: bool) -> Result<Vec<LocalTrack>> {
        scan_audio_files(root, recursive)?
            .iter()
            .map(|path| self.read_track(path))
            .collect()
    }
    fn read_track(&self, path: &Path) -> Result<LocalTrack> {
        Ok(LocalTrack {
            path: path.to_path_buf(),
            metadata: read_metadata(path)?,
        })
    }
}

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "m4a", "mp4", "aac", "ogg", "opus", "wav", "wma", "ape", "aiff", "aif",
];

pub fn scan_audio_files(root: impl AsRef<Path>, recursive: bool) -> Result<Vec<PathBuf>> {
    let depth = if recursive { usize::MAX } else { 1 };
    walkdir::WalkDir::new(root)
        .max_depth(depth)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_file() => Some(Ok(entry.into_path())),
            Ok(_) => None,
            Err(error) => Some(Err(Error::Provider {
                provider: "local".into(),
                message: error.to_string(),
            })),
        })
        .filter_map(|entry| match entry {
            Ok(path)
                if path
                    .extension()
                    .and_then(|s| s.to_str())
                    .is_some_and(|ext| {
                        AUDIO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
                    }) =>
            {
                Some(Ok(path))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

pub fn read_metadata(path: impl AsRef<Path>) -> Result<LocalMetadata> {
    use lofty::{
        file::{AudioFile, TaggedFileExt},
        probe::Probe,
        tag::Accessor,
    };
    let path = path.as_ref();
    let tagged = Probe::open(path)
        .map_err(local_error)?
        .read()
        .map_err(local_error)?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    Ok(LocalMetadata {
        title: tag.and_then(|tag| tag.title().map(|s| s.to_string())),
        artist: tag.and_then(|tag| tag.artist().map(|s| s.to_string())),
        album: tag.and_then(|tag| tag.album().map(|s| s.to_string())),
        duration_ms: tagged
            .properties()
            .duration()
            .as_millis()
            .min(u64::MAX as u128) as u64,
        format: path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase),
    })
}
fn local_error(error: impl std::fmt::Display) -> Error {
    Error::Provider {
        provider: "local".into(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn audio_extensions_are_case_insensitive() {
        assert!(AUDIO_EXTENSIONS.contains(&"flac"));
        assert!(AUDIO_EXTENSIONS.contains(&"opus"));
    }
}
