//! NCM container adapter (optional `ncm` feature).
//!
//! An NCM file is NetEase Cloud Music's encrypted download container: a
//! fixed-key AES layer over the song metadata and an NCM-specific RC4-like
//! stream cipher over the audio bytes. The audio inside is a plain MP3 / FLAC
//! / M4A / WAV / Ogg stream; this handler strips the container layer and
//! streams the decrypted audio to the caller. It does not transcode, fetch,
//! or play anything.
//!
//! Byte-level work is delegated to the external upstream crate
//! [`ncm2mp3-core`](https://crates.io/crates/ncm2mp3-core) 0.3.2
//! (Apache-2.0, <https://github.com/Johnserf-Seed/ncm2mp3>), audited
//! 2026-11-18: pure Rust, no network, no build script; its constants match
//! the independent C++ reference implementation (`ncmdump`) item by item.
//! Keep the upstream attribution when redistributing.
//!
//! Audit facts this adapter relies on:
//!
//! * the whole file is little-endian: 8-byte magic `CTENFDAM`, 2-byte gap,
//!   `u32` key blob, `u32` metadata blob (length 0 is legal), 4-byte CRC32 +
//!   1-byte image version (skipped, never validated: the algorithm is
//!   undocumented upstream), `u32` cover frame length + `u32` cover length,
//!   the plain JPEG/PNG cover, then padding up to the frame length, then the
//!   audio to EOF;
//! * the cover frame must be consumed through `cover_frame_len`: NetEase
//!   client 3.x reserves frame space (often with `cover_len == 0`), and
//!   treating the padding as audio desynchronizes the keystream into noise
//!   (upstream issues ncmdump#26, ncm2mp3#3). `NcmDecoder::from_reader` does
//!   this correctly, so the adapter never parses segments by hand;
//! * the audio cipher is position-addressable (XOR is self-inverse), so
//!   upstream's 32 KiB chunked streaming is byte-identical to a one-shot
//!   decrypt;
//! * `metadata.format` is only a hint; the real format is sniffed from the
//!   first 16 decrypted bytes (`NcmHeaders::effective_format` prefers the
//!   sniff, falls back to the hint). A sniff of `Unknown` means the audio is
//!   most likely noise (corrupt or unsupported variant); like the upstream
//!   core library (as opposed to its CLI, which refuses output) the handler
//!   still reports metadata and streams whatever decrypted, leaving the
//!   refuse-or-accept policy to the host application;
//! * known upstream boundary: `NcmStreamCipher::new` panics on an empty RC4
//!   key while `read_rc4_key` never checks for one, so a crafted key blob
//!   that decrypts to exactly the 17-byte `neteasecloudmusic` prefix panics
//!   the published crate (reproduced against 0.3.2 with the
//!   `tests/fixtures/ncm/panic_key.ncm` vector). The adapter therefore
//!   pre-parses magic + key segment with the upstream crate's own parser
//!   functions and rejects empty key material as `Error::InvalidInput`
//!   before constructing a decoder: hostile/corrupt input must surface as an
//!   error, never a panic;
//! * the CRC32 field is skipped, not verified (no documented algorithm);
//!   `.ncm` writing/encapsulation does not exist upstream, and
//!   `metadata.album_pic` URLs are never fetched (only the embedded cover is
//!   reported).

use std::fs::File;
use std::io::{BufReader, Write};
use std::path::Path;

use ncm2mp3_core::parser::{read_and_verify_magic, read_rc4_key};
use ncm2mp3_core::{AudioFormat as NcmAudioFormat, NcmDecoder, NcmError, NcmHeaders};

use crate::Result;
use crate::error::Error;
use crate::formats::{ContainerFormatHandler, ContainerMetadata};
use crate::models::AudioFormat;

/// NCM container handler.
///
/// `inspect` parses the headers (metadata, embedded cover, sniffed audio
/// format) without decrypting more than the 16-byte audio head;
/// `extract_audio_to` streams the fully decrypted audio. NCM is a
/// single-stream container, so it uses the trait's default empty
/// `inspect_tracks`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NcmHandler;

impl ContainerFormatHandler for NcmHandler {
    fn id(&self) -> &str {
        "ncm"
    }

    fn extensions(&self) -> &[&str] {
        &["ncm"]
    }

    /// Metadata, embedded cover and sniffed audio format of the container.
    /// Decrypts no more than the 16-byte audio head the upstream crate sniffs
    /// to determine the real format.
    fn inspect(&self, path: &Path) -> Result<ContainerMetadata> {
        let (_, headers) = open_guarded(path)?;
        Ok(headers_to_metadata(&headers))
    }

    /// Streams the decrypted audio (a plain MP3/FLAC/M4A/WAV/Ogg stream) to
    /// `output`. Bytes already written on a late I/O error are not rolled
    /// back, matching the upstream streaming decoder.
    fn extract_audio_to(
        &self,
        path: &Path,
        mut output: &mut dyn Write,
    ) -> Result<ContainerMetadata> {
        let (mut decoder, headers) = open_guarded(path)?;
        let metadata = headers_to_metadata(&headers);
        // `&mut dyn Write` itself implements `Write`, so the upstream decoder
        // can stream straight into the caller's writer.
        decoder
            .decode_to_writer(&mut output)
            .map_err(|error| ncm_error_at(path, error))?;
        Ok(metadata)
    }
}

/// Opens `path` and returns the upstream decoder plus its headers.
///
/// Two passes over the (small) header region: the first pass parses the
/// magic and the RC4 key segment with the upstream crate's own parser
/// functions and rejects empty key material *before*
/// [`NcmDecoder::from_reader`] builds `NcmStreamCipher`, which panics on an
/// empty key (upstream 0.3.2 `crypto/rc4.rs:25-26`, an `assert!` never
/// converted to an error variant). Corrupt or hostile files must surface as
/// errors, not panics.
fn open_guarded(path: &Path) -> Result<(NcmDecoder<BufReader<File>>, NcmHeaders)> {
    {
        let mut reader = BufReader::new(File::open(path)?);
        read_and_verify_magic(&mut reader).map_err(|error| ncm_error_at(path, error))?;
        let key = read_rc4_key(&mut reader).map_err(|error| ncm_error_at(path, error))?;
        if key.is_empty() {
            return Err(Error::InvalidInput(format!(
                "NCM 密钥段剥离 'neteasecloudmusic' 前缀后为空（文件损坏或恶意构造）: {}",
                path.display()
            )));
        }
    }
    let (decoder, headers) = NcmDecoder::from_reader(BufReader::new(File::open(path)?))
        .map_err(|error| ncm_error_at(path, error))?;
    Ok((decoder, headers))
}

/// Maps an upstream failure onto the crate error type: I/O keeps its
/// variant, everything else means the file is corrupt or not NCM at all.
fn ncm_error_at(path: &Path, error: NcmError) -> Error {
    match error {
        NcmError::Io(io) => Error::Io(io),
        other => Error::InvalidInput(format!("解析 NCM 失败 ({}): {other}", path.display())),
    }
}

/// Flattens the upstream headers into the container view.
///
/// `metadata.format` is only a hint, so the sniffed format wins through
/// `effective_format` (hint fallback). The bitrate has no slot in
/// `ContainerMetadata` and is dropped; the album-pic URL is never fetched
/// upstream and stays unused.
fn headers_to_metadata(headers: &NcmHeaders) -> ContainerMetadata {
    let meta = &headers.metadata;
    ContainerMetadata {
        title: non_empty(&meta.title),
        artists: meta.artists.clone(),
        album: non_empty(&meta.album),
        duration_ms: meta.duration,
        audio_format: map_audio_format(headers.effective_format()),
        artwork: headers.cover.as_ref().map(|cover| cover.data.clone()),
    }
}

/// `Some(value)` unless the string is empty.
fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

/// Maps the crate's audio format onto the `models` subset it shares.
fn map_audio_format(format: NcmAudioFormat) -> AudioFormat {
    match format {
        NcmAudioFormat::Mp3 => AudioFormat::Mp3,
        NcmAudioFormat::Flac => AudioFormat::Flac,
        NcmAudioFormat::M4a => AudioFormat::M4a,
        NcmAudioFormat::Wav => AudioFormat::Wav,
        NcmAudioFormat::Ogg => AudioFormat::Ogg,
        NcmAudioFormat::Unknown => AudioFormat::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Audit golden vector: see tests/fixtures/ncm/README.md for provenance
    /// and the expected decode values (cross-checked against the published
    /// ncm2mp3-core 0.3.2 decoder).
    const GOLDEN: &[u8] = include_bytes!("../tests/fixtures/ncm/golden.ncm");
    /// Adversarial empty-key vector: key blob decrypts to exactly the
    /// `neteasecloudmusic` prefix; published ncm2mp3-core 0.3.2 panics on it.
    const EMPTY_KEY: &[u8] = include_bytes!("../tests/fixtures/ncm/panic_key.ncm");
    /// Non-NCM bytes with a wrong magic (upstream parser test shape).
    const BAD_MAGIC: &[u8] = b"NOTCTENFDAM....";

    /// Cover bytes of the golden vector: JPEG head + ASCII `JFIF-audit`.
    const GOLDEN_COVER: [u8; 16] = [
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', b'-', b'a', b'u', b'd', b'i',
        b't',
    ];

    /// Exact audio plaintext of the golden vector: `ID3` v2 head + 3000
    /// bytes of `(i * 11 + 3) & 0xFF` (first 32 bytes match the audit hex
    /// `4944330400000000000a030e19242f3a45505b66717c87929da8b3bec9d4dfea`).
    fn golden_audio_plaintext() -> Vec<u8> {
        let mut audio = Vec::with_capacity(3010);
        audio.extend_from_slice(b"ID3\x04\x00\x00\x00\x00\x00\x0A");
        audio.extend((0..3000u32).map(|i| ((i * 11 + 3) & 0xFF) as u8));
        audio
    }

    /// Hosts fixture bytes in a fresh temp directory; removed when dropped
    /// (same pattern as the CUE adapter tests).
    struct NcmFixture {
        root: std::path::PathBuf,
    }

    impl NcmFixture {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("musid-dli-ncm-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self {
                root: fs::canonicalize(dir).unwrap(),
            }
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for NcmFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn handler_identity_matches_contract() {
        assert_eq!(NcmHandler.id(), "ncm");
        assert_eq!(NcmHandler.extensions(), ["ncm"]);
    }

    #[test]
    fn golden_inspect_reports_metadata_cover_and_format() {
        let fx = NcmFixture::new("golden-inspect");
        let path = fx.write("golden.ncm", GOLDEN);

        let meta = NcmHandler.inspect(&path).unwrap();

        assert_eq!(meta.title.as_deref(), Some("Audit Golden Vector"));
        assert_eq!(meta.artists, ["Auditor"]);
        assert_eq!(meta.album.as_deref(), Some("ncm-handler audit"));
        assert_eq!(meta.duration_ms, Some(6000));
        assert_eq!(meta.audio_format, AudioFormat::Mp3);
        let artwork = meta.artwork.expect("golden carries a cover");
        assert_eq!(artwork, GOLDEN_COVER);
    }

    #[test]
    fn golden_extract_streams_exact_plaintext() {
        let fx = NcmFixture::new("golden-extract");
        let path = fx.write("golden.ncm", GOLDEN);

        let mut out = Vec::new();
        let meta = NcmHandler.extract_audio_to(&path, &mut out).unwrap();

        // Byte-exact against the audit vector (stronger than the recorded
        // SHA-256 6c2cd8d7... of the same plaintext).
        assert_eq!(out, golden_audio_plaintext());
        assert_eq!(out.len(), 3010);

        // The returned metadata matches `inspect` on the same file.
        assert_eq!(meta.title.as_deref(), Some("Audit Golden Vector"));
        assert_eq!(meta.artists, ["Auditor"]);
        assert_eq!(meta.album.as_deref(), Some("ncm-handler audit"));
        assert_eq!(meta.duration_ms, Some(6000));
        assert_eq!(meta.audio_format, AudioFormat::Mp3);
        assert_eq!(meta.artwork.as_deref(), Some(GOLDEN_COVER.as_slice()));
    }

    #[test]
    fn empty_key_file_is_rejected_as_invalid_input_not_panic() {
        let fx = NcmFixture::new("empty-key");
        let path = fx.write("panic_key.ncm", EMPTY_KEY);

        let inspected = NcmHandler.inspect(&path).unwrap_err();
        assert!(
            matches!(inspected, Error::InvalidInput(_)),
            "inspect: {inspected}"
        );

        let mut out = Vec::new();
        let extracted = NcmHandler.extract_audio_to(&path, &mut out).unwrap_err();
        assert!(
            matches!(extracted, Error::InvalidInput(_)),
            "extract: {extracted}"
        );
        assert!(out.is_empty());
    }

    #[test]
    fn non_ncm_file_is_rejected_as_invalid_input() {
        let fx = NcmFixture::new("bad-magic");
        let path = fx.write("not_ncm.ncm", BAD_MAGIC);

        let err = NcmHandler.inspect(&path).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err}");

        let mut out = Vec::new();
        let err = NcmHandler.extract_audio_to(&path, &mut out).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err}");
        assert!(out.is_empty());
    }

    #[test]
    fn single_stream_container_reports_no_tracks() {
        let fx = NcmFixture::new("tracks");
        let path = fx.write("golden.ncm", GOLDEN);
        assert!(NcmHandler.inspect_tracks(&path).unwrap().is_empty());
    }

    #[test]
    fn crate_audio_format_maps_onto_models_subset() {
        use NcmAudioFormat as Ncm;
        assert_eq!(map_audio_format(Ncm::Mp3), AudioFormat::Mp3);
        assert_eq!(map_audio_format(Ncm::Flac), AudioFormat::Flac);
        assert_eq!(map_audio_format(Ncm::M4a), AudioFormat::M4a);
        assert_eq!(map_audio_format(Ncm::Wav), AudioFormat::Wav);
        assert_eq!(map_audio_format(Ncm::Ogg), AudioFormat::Ogg);
        assert_eq!(map_audio_format(Ncm::Unknown), AudioFormat::Unknown);
    }
}
