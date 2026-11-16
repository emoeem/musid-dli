//! Local container/format handlers (e.g. NCM) are not online music providers.
use crate::{Result, models::AudioFormat};
use std::{io::Write, path::Path};

#[derive(Debug, Clone, Default)]
pub struct ContainerMetadata {
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub audio_format: AudioFormat,
    pub artwork: Option<Vec<u8>>,
}

/// Implementations inspect/decrypt local containers and stream extracted audio
/// to a caller-owned writer; they do not download or play the resulting audio.
pub trait ContainerFormatHandler: Send + Sync {
    fn id(&self) -> &str;
    fn extensions(&self) -> &[&str];
    fn inspect(&self, path: &Path) -> Result<ContainerMetadata>;
    fn extract_audio_to(&self, path: &Path, output: &mut dyn Write) -> Result<ContainerMetadata>;
}
