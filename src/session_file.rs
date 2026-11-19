//! Atomic, per-provider-and-account session persistence with restrictive Unix
//! permissions, corrupt-file quarantine and a pluggable storage codec
//! (M2 core, AUTH_REGISTRY_DESIGN.md §4; tasks.md §10.5).
use crate::session::{DEFAULT_ACCOUNT_ID, ProviderSession};
use crate::{Error, ErrorCategory, Result};
use std::{fs, io::Write, path::PathBuf, sync::Arc};

/// Storage codec seam (tasks.md 10.5#5 encryption extension point).
///
/// The default [`PlainCodec`] stores the JSON envelope as-is: plaintext on
/// disk, which is an explicit, documented choice rather than an accident.
/// A host that requires encrypted-at-rest sessions injects its own codec via
/// [`FileSessionStore::open_with_codec`]; every read/write goes through
/// `decode`/`encode`, so the on-disk format is entirely the codec's business.
pub trait SessionCodec: Send + Sync {
    /// Transform plaintext bytes into stored bytes.
    fn encode(&self, plaintext: &[u8]) -> Result<Vec<u8>>;
    /// Transform stored bytes back into plaintext bytes. Returning an
    /// `Error::Serialization` is the codec's explicit declaration that the
    /// stored file is corrupt and triggers quarantine (see `load_for`); any
    /// other error (transient key store outage, KMS throttling, ...) is
    /// passed through and the file stays in place.
    fn decode(&self, stored: &[u8]) -> Result<Vec<u8>>;
}

/// Identity codec: plaintext JSON in, plaintext JSON out.
pub struct PlainCodec;
impl SessionCodec for PlainCodec {
    fn encode(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        Ok(plaintext.to_vec())
    }
    fn decode(&self, stored: &[u8]) -> Result<Vec<u8>> {
        Ok(stored.to_vec())
    }
}

/// Path-safe id charset shared by provider and account ids (matches the
/// pre-M2 provider rule, extended to account ids).
fn is_path_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Clone)]
pub struct FileSessionStore {
    root: PathBuf,
    codec: Arc<dyn SessionCodec>,
}
impl std::fmt::Debug for FileSessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSessionStore")
            .field("root", &self.root)
            .field("codec", &"<dyn SessionCodec>")
            .finish()
    }
}
impl FileSessionStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with_codec(root, Arc::new(PlainCodec))
    }
    /// Open with a custom codec (encryption extension point,
    /// tasks.md 10.5#5). The default path is [`PlainCodec`], byte-identical
    /// with the pre-M2 on-disk format.
    pub fn open_with_codec(root: impl Into<PathBuf>, codec: Arc<dyn SessionCodec>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { root, codec })
    }
    /// Load the default account's session (legacy single-provider API).
    pub fn load(&self, provider: &str) -> Result<Option<ProviderSession>> {
        self.load_for(provider, DEFAULT_ACCOUNT_ID)
    }
    /// Save the default account's session (legacy single-provider API).
    pub fn save(&self, provider: &str, session: &ProviderSession) -> Result<()> {
        self.save_for(provider, DEFAULT_ACCOUNT_ID, session)
    }
    /// Remove the default account's session file (legacy single-provider API).
    pub fn clear(&self, provider: &str) -> Result<()> {
        self.clear_for(provider, DEFAULT_ACCOUNT_ID)
    }

    /// Load one account's session from `root/{provider}.json` (default
    /// account, pre-M2 file) or `root/{provider}.{account}.json`.
    ///
    /// Quarantine contract (AUTH_REGISTRY_DESIGN.md §4.2): only a file whose
    /// JSON cannot be parsed, or whose codec explicitly declares corruption
    /// (a decode error in the `Serialization` category), is moved aside to
    /// `root/{original}.corrupt` (or `.corrupt-1`, `.corrupt-2`, ... if a
    /// quarantine copy already exists: no copy is ever overwritten) and the
    /// classified `Serialization` error is returned; the next load observes
    /// an empty store and the file can be re-saved. Any other codec failure
    /// (e.g. a transient keystore outage in an encrypted codec) is passed
    /// through untouched and the file stays exactly where it is.
    pub fn load_for(&self, provider: &str, account: &str) -> Result<Option<ProviderSession>> {
        let path = self.path_for(provider, account)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::Io(error)),
        };
        let plaintext = match self.codec.decode(&bytes) {
            Ok(plaintext) => plaintext,
            Err(error) => {
                // Only a codec-declared CORRUPT file (Error::Serialization,
                // see the SessionCodec contract) triggers quarantine. Any
                // other decode failure (e.g. a transient keystore outage in
                // an encrypted codec) is passed through untouched so it
                // cannot destroy a recoverable file.
                if matches!(error.category(), ErrorCategory::Serialization) {
                    return Err(self.quarantine_corrupt(&path, error));
                }
                return Err(error);
            }
        };
        match serde_json::from_slice::<ProviderSession>(&plaintext) {
            Ok(session) => Ok(Some(session)),
            Err(error) => Err(self.quarantine_corrupt(&path, Error::Serialization(error))),
        }
    }
    /// Save one account's session atomically (tempfile + persist, 0o600).
    pub fn save_for(&self, provider: &str, account: &str, session: &ProviderSession) -> Result<()> {
        let target = self.path_for(provider, account)?;
        let plaintext = serde_json::to_vec(session)?;
        let bytes = self.codec.encode(&plaintext)?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(&target)
            .map_err(|error| Error::Io(error.error))?;
        Ok(())
    }
    /// Remove one account's session file; a missing file stays `Ok(())`.
    pub fn clear_for(&self, provider: &str, account: &str) -> Result<()> {
        match fs::remove_file(self.path_for(provider, account)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::Io(error)),
        }
    }
    /// Accounts with a stored session for `provider`, sorted. The default
    /// account is reported as [`DEFAULT_ACCOUNT_ID`] even when its file is
    /// the pre-M2 `{provider}.json` layout.
    pub fn list_accounts(&self, provider: &str) -> Result<Vec<String>> {
        if !is_path_safe_id(provider) {
            return Err(Error::InvalidInput(
                "invalid provider ID for session path".into(),
            ));
        }
        let default_file = format!("{provider}.json");
        let prefix = format!("{provider}.");
        let mut accounts = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if name == default_file {
                accounts.push(DEFAULT_ACCOUNT_ID.to_owned());
            } else if let Some(account) = name
                .strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(".json"))
            {
                // `.corrupt` quarantine files never end in `.json`.
                if is_path_safe_id(account) {
                    accounts.push(account.to_owned());
                }
            }
        }
        accounts.sort();
        accounts.dedup();
        Ok(accounts)
    }

    fn path_for(&self, provider: &str, account: &str) -> Result<PathBuf> {
        if !is_path_safe_id(provider) {
            return Err(Error::InvalidInput(
                "invalid provider ID for session path".into(),
            ));
        }
        if !is_path_safe_id(account) {
            return Err(Error::InvalidInput(
                "invalid account ID for session path".into(),
            ));
        }
        if account == DEFAULT_ACCOUNT_ID {
            Ok(self.root.join(format!("{provider}.json")))
        } else {
            Ok(self.root.join(format!("{provider}.{account}.json")))
        }
    }
    /// Best-effort quarantine of a corrupt file; the original error is
    /// always returned regardless of whether the rename succeeds.
    ///
    /// Never overwrites an existing quarantine copy: the first corrupt copy
    /// keeps the canonical `{original}.corrupt` name, later corruptions of
    /// the same file land on `{original}.corrupt-1`, `.corrupt-2`, ... (up
    /// to a bounded number of numbered copies, after which the file is left
    /// in place and only the error surfaces). A plain POSIX rename would
    /// silently destroy the previous copy, which may be the only salvageable
    /// session data.
    fn quarantine_corrupt(&self, path: &std::path::Path, error: Error) -> Error {
        const MAX_QUARANTINE_COPIES: u32 = 16;
        let canonical = {
            let mut name = path.as_os_str().to_owned();
            name.push(".corrupt");
            PathBuf::from(name)
        };
        let mut target = canonical.clone();
        for index in 1..=MAX_QUARANTINE_COPIES {
            if !target.exists() {
                break;
            }
            let mut numbered = path.as_os_str().to_owned();
            numbered.push(format!(".corrupt-{index}"));
            target = PathBuf::from(numbered);
        }
        if !target.exists() {
            let _ = fs::rename(path, target);
        }
        error
    }
    /// Class of the error a corrupt session file produces (exposed for the
    /// recovery tests and host diagnostics).
    pub fn corrupt_file_category() -> ErrorCategory {
        ErrorCategory::Serialization
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("musid-dli-session-{tag}-{}", std::process::id()))
    }

    #[test]
    fn persists_sessions_and_rejects_path_traversal() {
        let root = temp_root("legacy");
        let store = FileSessionStore::open(&root).unwrap();
        let mut session = ProviderSession::default();
        session.cookies.insert("sid".into(), "secret".into());
        store.save("netease", &session).unwrap();
        assert_eq!(
            store.load("netease").unwrap().unwrap().cookies["sid"],
            "secret"
        );
        assert!(store.load("../outside").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root.join("netease.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        store.clear("netease").unwrap();
        assert!(store.load("netease").unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_file_is_quarantined_and_store_recovers() {
        let root = temp_root("corrupt");
        let store = FileSessionStore::open(&root).unwrap();
        let mut session = ProviderSession::default();
        session.cookies.insert("sid".into(), "secret".into());
        store.save("netease", &session).unwrap();
        // Corrupt the file in place.
        fs::write(root.join("netease.json"), b"\xff\xfe not json {{{{").unwrap();
        let error = store.load("netease").unwrap_err();
        assert_eq!(error.category(), FileSessionStore::corrupt_file_category());
        assert_eq!(error.category(), ErrorCategory::Serialization);
        // Original path is gone, quarantined twin exists.
        assert!(!root.join("netease.json").exists());
        assert!(root.join("netease.json.corrupt").exists());
        // Store recovered: empty read, then a working save/load cycle.
        assert!(store.load("netease").unwrap().is_none());
        store.save("netease", &session).unwrap();
        assert_eq!(
            store.load("netease").unwrap().unwrap().cookies["sid"],
            "secret"
        );
        // Same treatment under the account-key API.
        fs::write(root.join("netease.acct-9.json"), b"garbage").unwrap();
        let error = store.load_for("netease", "acct-9").unwrap_err();
        assert_eq!(error.category(), ErrorCategory::Serialization);
        assert!(root.join("netease.acct-9.json.corrupt").exists());
        assert!(store.load_for("netease", "acct-9").unwrap().is_none());
        // A SECOND corruption of the same file must never overwrite the
        // first quarantine copy (first-round review data-loss face): the
        // canonical copy keeps its bytes, the new one lands on .corrupt-1.
        let first_copy = fs::read(root.join("netease.json.corrupt")).unwrap();
        fs::write(root.join("netease.json"), b"second corruption ??").unwrap();
        store.load("netease").unwrap_err();
        assert_eq!(
            fs::read(root.join("netease.json.corrupt")).unwrap(),
            first_copy
        );
        assert!(root.join("netease.json.corrupt-1").exists());
        assert!(!root.join("netease.json").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn account_files_are_isolated_with_default_account_backcompat() {
        let root = temp_root("accounts");
        let store = FileSessionStore::open(&root).unwrap();
        let mut default_session = ProviderSession::default();
        default_session
            .cookies
            .insert("sid".into(), "default-value".into());
        let mut acct_a = ProviderSession::default();
        acct_a.cookies.insert("sid".into(), "a-value".into());
        let mut acct_b = ProviderSession::default();
        acct_b.tokens.insert("tok".into(), "b-value".into());
        acct_b.region = Some("CN".into());
        // Pre-M2 layout: default account still uses {provider}.json.
        store.save("netease", &default_session).unwrap();
        store.save_for("netease", "acct-a", &acct_a).unwrap();
        store.save_for("netease", "acct-b", &acct_b).unwrap();
        assert!(root.join("netease.json").exists());
        assert!(root.join("netease.acct-a.json").exists());
        assert_eq!(
            store.load("netease").unwrap().unwrap().cookies["sid"],
            "default-value"
        );
        assert_eq!(
            store
                .load_for("netease", "acct-a")
                .unwrap()
                .unwrap()
                .cookies["sid"],
            "a-value"
        );
        assert_eq!(
            store.load_for("netease", "acct-b").unwrap().unwrap().tokens["tok"],
            "b-value"
        );
        assert_eq!(
            store.list_accounts("netease").unwrap(),
            vec![
                "acct-a".to_owned(),
                "acct-b".to_owned(),
                DEFAULT_ACCOUNT_ID.to_owned()
            ]
        );
        // New fields survive persistence (serde defaults keep old files loadable).
        assert_eq!(
            store
                .load_for("netease", "acct-b")
                .unwrap()
                .unwrap()
                .region
                .as_deref(),
            Some("CN")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root.join("netease.acct-a.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        store.clear_for("netease", "acct-a").unwrap();
        assert!(store.load_for("netease", "acct-a").unwrap().is_none());
        assert!(store.load_for("netease", "acct-b").unwrap().is_some());
        // Account ids obey the same path-safety rule as provider ids.
        assert!(store.load_for("netease", "../../etc").is_err());
        assert!(store.load_for("../outside", "acct-a").is_err());
        let _ = fs::remove_dir_all(root);
    }

    struct XorCodec(u8);
    impl SessionCodec for XorCodec {
        fn encode(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
            Ok(plaintext.iter().map(|b| b ^ self.0).collect())
        }
        fn decode(&self, stored: &[u8]) -> Result<Vec<u8>> {
            Ok(stored.iter().map(|b| b ^ self.0).collect())
        }
    }

    /// Codec that fails decode with a transient (non-corrupt) error until
    /// `heal` flips, mirroring a locked keystore / throttled KMS.
    struct FlakyCodec {
        heal: std::sync::atomic::AtomicBool,
    }
    impl SessionCodec for FlakyCodec {
        fn encode(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
            Ok(plaintext.to_vec())
        }
        fn decode(&self, stored: &[u8]) -> Result<Vec<u8>> {
            if self.heal.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(stored.to_vec())
            } else {
                // Transient, NOT a corruption marker.
                Err(Error::Io(std::io::Error::other(
                    "key store temporarily locked",
                )))
            }
        }
    }

    #[test]
    fn transient_decode_failure_does_not_quarantine_or_destroy_the_file() {
        let root = temp_root("flaky");
        let codec = Arc::new(FlakyCodec {
            heal: std::sync::atomic::AtomicBool::new(false),
        });
        let store = FileSessionStore::open_with_codec(&root, codec.clone()).unwrap();
        let mut session = ProviderSession::default();
        session.cookies.insert("sid".into(), "recoverable".into());
        store.save_for("netease", "acct-1", &session).unwrap();
        // Transient failure: classified error, file untouched, no .corrupt.
        let error = store.load_for("netease", "acct-1").unwrap_err();
        assert_eq!(error.category(), ErrorCategory::Io);
        assert!(root.join("netease.acct-1.json").exists());
        assert!(!root.join("netease.acct-1.json.corrupt").exists());
        // After the transient outage ends, the same file still loads.
        codec.heal.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            store
                .load_for("netease", "acct-1")
                .unwrap()
                .unwrap()
                .cookies["sid"],
            "recoverable"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// Codec that declares corruption the way the trait contract requires:
    /// by returning the Serialization category.
    struct CorruptDeclaringCodec;
    impl SessionCodec for CorruptDeclaringCodec {
        fn encode(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
            Ok(plaintext.to_vec())
        }
        fn decode(&self, _stored: &[u8]) -> Result<Vec<u8>> {
            Err(Error::Serialization(
                serde_json::from_str::<serde_json::Value>("not-json").unwrap_err(),
            ))
        }
    }

    #[test]
    fn codec_declared_corruption_quarantines_the_file() {
        // The corrupt dialect is the Serialization category (SessionCodec
        // contract): that, and only that, moves the file to *.corrupt.
        let root = temp_root("declared-corrupt");
        let store =
            FileSessionStore::open_with_codec(&root, Arc::new(CorruptDeclaringCodec)).unwrap();
        let session = ProviderSession::default();
        store.save_for("netease", "acct-2", &session).unwrap();
        let error = store.load_for("netease", "acct-2").unwrap_err();
        assert_eq!(error.category(), ErrorCategory::Serialization);
        assert!(!root.join("netease.acct-2.json").exists());
        assert!(root.join("netease.acct-2.json.corrupt").exists());
        assert!(store.load_for("netease", "acct-2").unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn codec_extension_point_round_trips_and_scrambles_disk_bytes() {
        let root = temp_root("codec");
        let store = FileSessionStore::open_with_codec(&root, Arc::new(XorCodec(0x5A))).unwrap();
        let mut session = ProviderSession::default();
        session.cookies.insert("sid".into(), "secret".into());
        store.save_for("netease", "acct-1", &session).unwrap();
        let raw = fs::read(root.join("netease.acct-1.json")).unwrap();
        assert!(
            std::str::from_utf8(&raw).is_err() || !raw.windows(6).any(|w| w == b"secret"),
            "codec must not leave plaintext on disk"
        );
        assert_eq!(
            store
                .load_for("netease", "acct-1")
                .unwrap()
                .unwrap()
                .cookies["sid"],
            "secret"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn exported_session_moves_through_file_store_with_redacted_debug() {
        let root = temp_root("export");
        let store = FileSessionStore::open(&root).unwrap();
        let mut session = ProviderSession::default();
        session
            .tokens
            .insert("refresh_token".into(), "secret-token".into());
        let export = session.export("netease", "acct-2").unwrap();
        let debug = format!("{export:?}");
        assert!(
            !debug.contains("secret-token"),
            "export Debug leaked: {debug}"
        );
        let imported = ProviderSession::import("netease", "acct-2", export.as_bytes()).unwrap();
        store.save_for("netease", "acct-2", &imported).unwrap();
        assert_eq!(
            store.load_for("netease", "acct-2").unwrap().unwrap().tokens["refresh_token"],
            "secret-token"
        );
        let _ = fs::remove_dir_all(root);
    }
}
