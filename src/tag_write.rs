//! Tag-write unit shared by the `local` and `download` features
//! (docs/integration/LOCAL_ENHANCEMENT_DESIGN.md §4, M5 slice s3).
//!
//! `MetadataEdit`/`write_metadata` previously lived in `src/download.rs`
//! (`download.rs:1218-1262`, a verbatim port of voicefox
//! `source/src/local/metadata.rs write_metadata`). The behavior is preserved
//! with deliberate changes: the write is no longer applied to the source file
//! in place. lofty's `save_to_path` rewrote the user's file directly, so a
//! failure halfway through could leave a half-written file behind. The write
//! now goes through a same-directory temp file:
//!
//! 1. `fs::copy` the source bytes to `.musid-tmp-{pid}-{seq}{ext}`;
//! 2. edit the tags on the copy and `save_to_path` it (never the source);
//! 3. fsync the temp file, then re-read it with lofty and assert the edited
//!    fields landed (verification happens before the rename, so a failed
//!    verification never touches the source);
//! 4. atomically `rename` the temp file onto the source.
//!
//! Failure guarantees (the exact invariant): the temp file is removed ONLY
//! when the first rename fails while the target does not exist, because then
//! the target was never touched. Once the Windows-style fallback enters (the
//! target still existed, the staged copy exists), the staged copy is the only
//! surviving content and is NEVER deleted on any later failure: a failure
//! there keeps the staged file on disk and names it in the error so the
//! content stays recoverable. The destructive fallback itself only runs while
//! a staged copy actually exists; if the rename failed because the staged
//! copy is already gone, the target is left untouched. Side effect worth
//! knowing: on POSIX a plain EACCES (read-only directory) fails the first
//! rename with the target still present and the temp copy kept, leaving a
//! `.musid-tmp-*` file behind whose path the error names, which preserves the
//! staged content instead of destroying it.
//!
//! POSIX rename replaces the target atomically. Windows `std::fs::rename`
//! (MoveFileExW + MOVEFILE_REPLACE_EXISTING) also replaces an existing file,
//! but refuses while another process holds the target open; when the rename
//! reports the target still present, the fallback removes it and renames
//! again (one retry). The removal makes the staged copy the only copy until
//! the rename lands; that is the platform window the hard boundary accepts as
//! "platform-permitted", and the no-delete rule above bounds its worst case.
//! Note the rename is not followed by a directory fsync, so the promise is
//! "no process-visible failure destroys or corrupts the source", not
//! crash durability across a power loss (a host that needs it fsyncs the
//! directory itself after a successful write).
//!
//! Two behavior differences against the old in-place write, both documented
//! in the design doc §4.2: (1) the staging/replace flow above; (2) symbolic
//! links are refused outright. `fs::copy` would follow the link and the final
//! rename would silently replace the link with a regular file, destroying the
//! user's link layout; the old in-place write happened to write through the
//! link instead. [`write_metadata`] now errors on links so nothing is
//! modified implicitly; hosts resolve the link and write the target.
//!
//! [`atomic_replace`] is the reusable primitive; the M5 cover-cache slice
//! reuses it for corrupt-cache rebuilds. The unit compiles under
//! `cfg(any(feature = "local", feature = "download"))` and does **not** make
//! `download` imply `local`: both features already pull `dep:lofty`
//! independently (FEATURE_DEPENDENCY_MATRIX.md §4), and that recorded
//! coupling stays unchanged. `download.rs` re-exports both items, so the
//! public paths `download::write_metadata` / `download::MetadataEdit` are
//! unchanged.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Prefix of the temp files this module creates inside the target directory.
/// Tests assert which of them survive a failed or finished write.
const TEMP_PREFIX: &str = ".musid-tmp-";

/// Process-local sequence number for temp file names (pid + seq is unique
/// among live writers of this process; no `tempfile` dependency is added).
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Tag fields writable into an audio file (`None` keeps the existing value).
#[derive(Debug, Clone, Default)]
pub struct MetadataEdit {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover: Option<Vec<u8>>,
    /// Embedded lyrics text (LRC). Containers without lyric slots ignore it.
    pub lyrics: Option<String>,
}

/// Writes the tags and the optional cover/lyrics into the local audio file
/// through a temp file and an atomic rename (see the module docs). Returns
/// `Err` with the source file untouched whenever any step fails while the
/// source still exists. Symbolic links are refused (they would be replaced by
/// a regular file, see the module docs).
pub fn write_metadata(path: &Path, edit: &MetadataEdit) -> Result<(), String> {
    // A staged-file replace cannot preserve a symlink: the rename would put a
    // regular file where the link was. Refuse instead of modifying the user's
    // file layout implicitly.
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(format!(
            "refusing to write tags through the symbolic link {}: the staged replace would turn the link into a regular file; resolve the link and write its target instead",
            path.display()
        ));
    }

    atomic_replace(path, |tmp| {
        use lofty::config::WriteOptions;
        use lofty::file::{AudioFile, TaggedFileExt};
        use lofty::picture::Picture;
        use lofty::tag::{Accessor, ItemKey, Tag};

        // Stage: work on a byte copy of the source. `fs::copy` also carries
        // the file permissions over, so the replacement keeps the mode.
        std::fs::copy(path, tmp)
            .map_err(|error| format!("failed to stage a copy of {}: {error}", path.display()))?;

        let mut tagged =
            lofty::read_from_path(tmp).map_err(|error| format!("failed to read tags: {error}"))?;

        if tagged.primary_tag_mut().is_none() {
            let tag_type = tagged.primary_tag_type();
            if !tagged.supports_tag_type(tag_type) {
                return Err(format!("{} does not support tag writing", path.display()));
            }
            tagged.insert_tag(Tag::new(tag_type));
        }
        let tag = tagged
            .primary_tag_mut()
            .ok_or_else(|| format!("{} has no writable tag", path.display()))?;
        if let Some(title) = edit.title.as_ref() {
            tag.set_title(title.clone());
        }
        if let Some(artist) = edit.artist.as_ref() {
            tag.set_artist(artist.clone());
        }
        if let Some(album) = edit.album.as_ref() {
            tag.set_album(album.clone());
        }
        if let Some(cover) = edit.cover.as_ref() {
            let picture = Picture::from_reader(&mut std::io::Cursor::new(cover))
                .map_err(|error| format!("invalid cover format: {error}"))?;
            tag.set_picture(0, picture);
        }
        if let Some(lyrics) = edit.lyrics.as_ref()
            && !lyrics.trim().is_empty()
        {
            tag.insert_text(ItemKey::Lyrics, lyrics.clone());
        }

        tagged
            .save_to_path(tmp, WriteOptions::default())
            .map_err(|error| format!("failed to write tags: {error}"))?;

        // Flush the temp copy before it becomes the source file.
        std::fs::File::options()
            .write(true)
            .open(tmp)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("failed to flush the staged tags: {error}"))?;

        verify_written_tags(tmp, edit)?;
        Ok(())
    })
}

/// Re-reads the staged file and asserts the edited fields are on disk. Only
/// fields present in `edit` are asserted; the tag written above is the same
/// one the rename promotes to the source file.
fn verify_written_tags(path: &Path, edit: &MetadataEdit) -> Result<(), String> {
    use lofty::file::TaggedFileExt;
    use lofty::tag::{Accessor, ItemKey};

    let tagged = lofty::read_from_path(path)
        .map_err(|error| format!("post-write verification failed to read tags: {error}"))?;
    let tag = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .ok_or_else(|| "post-write verification found no tag".to_string())?;

    let expect = |actual: Option<std::borrow::Cow<'_, str>>, name: &str| {
        actual
            .map(|value| value.into_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("post-write verification lost the {name} field"))
    };
    if let Some(title) = edit.title.as_ref().filter(|title| !title.is_empty()) {
        let written = expect(tag.title(), "title")?;
        if written != *title {
            return Err(format!(
                "post-write verification mismatched the title: {written:?}"
            ));
        }
    }
    if let Some(artist) = edit.artist.as_ref().filter(|artist| !artist.is_empty()) {
        let written = expect(tag.artist(), "artist")?;
        if written != *artist {
            return Err(format!(
                "post-write verification mismatched the artist: {written:?}"
            ));
        }
    }
    if let Some(album) = edit.album.as_ref().filter(|album| !album.is_empty()) {
        let written = expect(tag.album(), "album")?;
        if written != *album {
            return Err(format!(
                "post-write verification mismatched the album: {written:?}"
            ));
        }
    }
    if edit.cover.is_some() && tag.pictures().is_empty() {
        return Err("post-write verification lost the cover".to_string());
    }
    if let Some(lyrics) = edit
        .lyrics
        .as_ref()
        .filter(|lyrics| !lyrics.trim().is_empty())
    {
        let written = tag
            .get_string(&ItemKey::Lyrics)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "post-write verification lost the lyrics".to_string())?;
        if written != lyrics.as_str() {
            return Err("post-write verification mismatched the lyrics".to_string());
        }
    }
    Ok(())
}

/// Replaces `target` with whatever `stage` writes into the handed temp path
/// (same directory as `target`), fsync and rename included. While the target
/// exists, every failure removes the temp file and leaves the target
/// byte-for-byte unchanged. The stage must fsync or otherwise finalize the
/// temp file itself if it needs durability guarantees beyond the rename.
///
/// The stage receives the temp path including `target`'s extension, so
/// extension-keyed readers (lofty probing) keep working on it.
pub fn atomic_replace<T>(
    target: &Path,
    stage: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    // A staged-file replace cannot preserve a symlink: the rename would put a
    // regular file where the link was. The primitive refuses exactly like
    // [`write_metadata`] so no caller (the cover cache, for example) can
    // silently destroy the user's link layout.
    if let Ok(metadata) = std::fs::symlink_metadata(target)
        && metadata.file_type().is_symlink()
    {
        return Err(format!(
            "refusing to replace the symbolic link {}: the staged replace would turn the link into a regular file; resolve the link and replace its target instead",
            target.display()
        ));
    }

    let directory = target.parent().unwrap_or_else(|| Path::new("."));
    let extension = target
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    let tmp = directory.join(format!(
        "{TEMP_PREFIX}{}-{}{extension}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed),
    ));

    match stage(&tmp) {
        Ok(value) => match promote(&tmp, target) {
            Ok(()) => Ok(value),
            Err(error) => Err(error),
        },
        Err(error) => {
            // The target was never touched; the staged copy is disposable.
            let _ = std::fs::remove_file(&tmp);
            Err(error)
        }
    }
}

/// Rename promotion with the Windows-style fallback. While the target exists,
/// every failure removes the temp file (the target is untouched). Once the
/// fallback has removed the target, the staged copy is the only surviving
/// content and is NEVER deleted on failure: the error names its path so the
/// content stays recoverable. One rename retry covers transient errors
/// (EINTR-style) inside the fallback window.
fn promote(tmp: &Path, target: &Path) -> Result<(), String> {
    match std::fs::rename(tmp, target) {
        Ok(()) => Ok(()),
        Err(first) => {
            if !target.exists() {
                // The target was never touched, so the staged copy is
                // disposable and the source is byte-for-byte intact.
                let _ = std::fs::remove_file(tmp);
                return Err(format!("failed to replace {}: {first}", target.display()));
            }
            // Enter the destructive fallback only when a staged copy actually
            // survives: after the removal below it is the only content, and
            // with no staged copy there is nothing to rename, so the target
            // must stay untouched.
            if !tmp.exists() {
                return Err(format!(
                    "failed to replace {}: rename failed ({first}) and the staged copy is gone; the target was left untouched",
                    target.display()
                ));
            }
            // The target still exists (Windows refuses an open/locked target).
            // Remove it, then rename; after the removal the staged copy is the
            // only surviving content, so no failure below may delete it.
            let removal = std::fs::remove_file(target);
            let second = match removal {
                Ok(()) => std::fs::rename(tmp, target).or_else(|_| std::fs::rename(tmp, target)),
                Err(fallback) => Err(fallback),
            };
            match second {
                Ok(()) => Ok(()),
                Err(fallback) => Err(format!(
                    "failed to replace {}: rename failed ({first}); the fallback remove+rename failed ({fallback}); the staged copy was kept at {} and still holds the full edited content",
                    target.display(),
                    tmp.display()
                )),
            }
        }
    }
}

/// True when `name` is one of this module's leftover temp files.
#[cfg(test)]
fn is_temp_leftover(name: &str) -> bool {
    name.starts_with(TEMP_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Committed ffmpeg-built MP3 fixture (same precedent as src/local.rs
    /// tests; see tests/fixtures/local for the build commands).
    const MP3_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/local/spec_mp3_cbr128.mp3");

    struct TempDir {
        root: PathBuf,
    }
    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("musid-dli-tagwrite-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self {
                root: fs::canonicalize(dir).unwrap(),
            }
        }
        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
        fn leftovers(&self) -> Vec<String> {
            fs::read_dir(&self.root)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| is_temp_leftover(name))
                .collect()
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn read_tag_string(
        path: &Path,
        read: impl Fn(&lofty::tag::Tag) -> Option<String>,
    ) -> Option<String> {
        use lofty::file::TaggedFileExt;
        let tagged = lofty::read_from_path(path).unwrap();
        let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
        read(tag)
    }

    #[test]
    fn tags_roundtrip_through_the_atomic_write() {
        let dir = TempDir::new("roundtrip");
        let target = dir.path("song.mp3");
        fs::write(&target, MP3_FIXTURE).unwrap();
        let edit = MetadataEdit {
            title: Some("原子写标题".to_string()),
            artist: Some("Atomic Artist".to_string()),
            album: Some("Atomic Album".to_string()),
            lyrics: Some("[00:01.00]原子写歌词".to_string()),
            cover: None,
        };

        write_metadata(&target, &edit).unwrap();

        let title = read_tag_string(&target, |tag| {
            use lofty::tag::Accessor;
            Accessor::title(tag).map(|value| value.into_owned())
        });
        assert_eq!(title.as_deref(), Some("原子写标题"));
        let artist = read_tag_string(&target, |tag| {
            use lofty::tag::Accessor;
            Accessor::artist(tag).map(|value| value.into_owned())
        });
        assert_eq!(artist.as_deref(), Some("Atomic Artist"));
        let album = read_tag_string(&target, |tag| {
            use lofty::tag::Accessor;
            Accessor::album(tag).map(|value| value.into_owned())
        });
        assert_eq!(album.as_deref(), Some("Atomic Album"));
        let lyrics = read_tag_string(&target, |tag| {
            tag.get_string(&lofty::tag::ItemKey::Lyrics)
                .map(str::to_string)
        });
        assert_eq!(lyrics.as_deref(), Some("[00:01.00]原子写歌词"));
        // No temp file survives a finished write.
        assert!(dir.leftovers().is_empty(), "{:?}", dir.leftovers());
    }

    #[test]
    fn failed_cover_write_leaves_the_source_intact() {
        let dir = TempDir::new("corrupt-cover");
        let target = dir.path("song.mp3");
        fs::write(&target, MP3_FIXTURE).unwrap();
        let original = fs::read(&target).unwrap();
        // Bytes with no image magic: lofty's picture parser rejects them
        // outright ("Encountered invalid data"). (Probed 2026-11-20: PNG
        // magic followed by junk is accepted, lofty only sniffs the magic.)
        let corrupt = b"this is not a picture".to_vec();
        let edit = MetadataEdit {
            title: Some("never lands".to_string()),
            cover: Some(corrupt),
            ..MetadataEdit::default()
        };

        let error = write_metadata(&target, &edit).unwrap_err();

        assert!(error.contains("invalid cover format"), "{error}");
        // The source bytes are untouched and no temp file is left behind.
        assert_eq!(fs::read(&target).unwrap(), original);
        assert!(dir.leftovers().is_empty(), "{:?}", dir.leftovers());
    }

    #[test]
    fn failed_stage_leaves_the_target_intact() {
        let dir = TempDir::new("stage-fail");
        let target = dir.path("keep.txt");
        fs::write(&target, b"original").unwrap();

        let error = atomic_replace(&target, |_tmp| -> Result<(), String> {
            Err("stage refused".to_string())
        })
        .unwrap_err();

        assert_eq!(error, "stage refused");
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert!(dir.leftovers().is_empty(), "{:?}", dir.leftovers());
    }

    #[test]
    fn successful_stage_replaces_the_target() {
        let dir = TempDir::new("stage-ok");
        let target = dir.path("swap.txt");
        fs::write(&target, b"old bytes").unwrap();

        let written = atomic_replace(&target, |tmp| {
            fs::write(tmp, b"new bytes").unwrap();
            Ok(42u32)
        })
        .unwrap();

        assert_eq!(written, 42);
        assert_eq!(fs::read(&target).unwrap(), b"new bytes");
        assert!(dir.leftovers().is_empty(), "{:?}", dir.leftovers());
    }

    #[test]
    fn failed_promotion_keeps_the_staged_copy() {
        // rename(file -> existing directory) is refused by the OS, so the
        // first rename fails with the target still present; the fallback's
        // remove_file then fails too (target is a directory). No content was
        // destroyed, but the target still exists, so per the promote contract
        // the staged copy is preserved and named in the error instead of
        // being deleted.
        let dir = TempDir::new("rename-fail");
        let target = dir.path("target-dir");
        fs::create_dir_all(&target).unwrap();

        let error = atomic_replace(&target, |tmp| {
            fs::write(tmp, b"staged").unwrap();
            Ok(())
        })
        .unwrap_err();

        assert!(error.contains("failed to replace"), "{error}");
        assert!(error.contains("staged copy was kept at"), "{error}");
        assert!(target.is_dir());
        let survivors = dir.leftovers();
        assert_eq!(survivors.len(), 1, "{survivors:?}");
    }

    #[test]
    fn promotion_failure_without_a_removed_target_cleans_the_temp_file_up() {
        // The stage itself removes the temp file, so the rename fails while
        // the target never existed: the safe branch cleans up (a no-op) and
        // reports the error.
        let dir = TempDir::new("rename-safe");
        let target = dir.path("keep.txt");
        fs::write(&target, b"original").unwrap();

        let error = atomic_replace(&target, |tmp| {
            fs::write(tmp, b"staged").unwrap();
            fs::remove_file(tmp).unwrap();
            Ok(())
        })
        .unwrap_err();

        assert!(error.contains("failed to replace"), "{error}");
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert!(dir.leftovers().is_empty(), "{:?}", dir.leftovers());
    }

    #[test]
    #[cfg(unix)]
    fn symbolic_links_are_refused_instead_of_replaced() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new("symlink-refuse");
        let real = dir.path("real.mp3");
        fs::write(&real, MP3_FIXTURE).unwrap();
        let original = fs::read(&real).unwrap();
        let link = dir.path("link.mp3");
        symlink(&real, &link).unwrap();

        let edit = MetadataEdit {
            title: Some("must not land".to_string()),
            ..MetadataEdit::default()
        };
        let error = write_metadata(&link, &edit).unwrap_err();

        assert!(error.contains("symbolic link"), "{error}");
        // the link still is a link and the target bytes are untouched
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&real).unwrap(), original);
        assert!(dir.leftovers().is_empty(), "{:?}", dir.leftovers());
    }

    #[test]
    fn unsupported_container_is_rejected_with_the_source_intact() {
        // The rejection face a caller can actually hit. Probed 2026-11-20
        // (lofty 0.22.4 default features): every built-in FileType supports
        // its primary tag type, so the explicit "does not support tag
        // writing" branch above (kept verbatim from the old in-place write)
        // is defensive-dead for natural containers; non-audio bytes under an
        // audio extension fail at lofty's read layer instead, before anything
        // is staged. This pins that semantics: Err, source bytes untouched,
        // no temp leftovers.
        let dir = TempDir::new("no-tag-container");
        let target = dir.path("notes.mp3");
        fs::write(&target, b"plain text, not an audio container").unwrap();
        let original = fs::read(&target).unwrap();
        let edit = MetadataEdit {
            title: Some("must not land".to_string()),
            ..MetadataEdit::default()
        };

        let error = write_metadata(&target, &edit).unwrap_err();

        assert!(error.starts_with("failed to read tags"), "{error}");
        assert_eq!(fs::read(&target).unwrap(), original);
        assert!(dir.leftovers().is_empty(), "{:?}", dir.leftovers());
    }

    #[test]
    #[cfg(unix)]
    fn atomic_replace_refuses_symbolic_links() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new("primitive-symlink");
        let real = dir.path("real.mp3");
        fs::write(&real, MP3_FIXTURE).unwrap();
        let original = fs::read(&real).unwrap();
        let link = dir.path("link.mp3");
        symlink(&real, &link).unwrap();

        let error = atomic_replace(&link, |tmp| {
            fs::write(tmp, b"staged").unwrap();
            Ok(())
        })
        .unwrap_err();

        assert!(error.contains("symbolic link"), "{error}");
        // the link survives as a link and the target bytes are untouched
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&real).unwrap(), original);
        assert!(dir.leftovers().is_empty(), "{:?}", dir.leftovers());
    }
}
