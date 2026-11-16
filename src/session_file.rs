//! Atomic, per-provider session persistence with restrictive Unix permissions.
use crate::{Error, Result, session::ProviderSession};
use std::{fs, io::Write, path::PathBuf};

#[derive(Clone, Debug)]
pub struct FileSessionStore {
    root: PathBuf,
}
impl FileSessionStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { root })
    }
    pub fn load(&self, provider: &str) -> Result<Option<ProviderSession>> {
        match fs::read(self.path(provider)?) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::Io(error)),
        }
    }
    pub fn save(&self, provider: &str, session: &ProviderSession) -> Result<()> {
        let target = self.path(provider)?;
        let bytes = serde_json::to_vec(session)?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(&target)
            .map_err(|error| Error::Io(error.error))?;
        Ok(())
    }
    pub fn clear(&self, provider: &str) -> Result<()> {
        match fs::remove_file(self.path(provider)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::Io(error)),
        }
    }
    fn path(&self, provider: &str) -> Result<PathBuf> {
        if provider.is_empty()
            || !provider
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::InvalidInput(
                "invalid provider ID for session path".into(),
            ));
        }
        Ok(self.root.join(format!("{provider}.json")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persists_sessions_and_rejects_path_traversal() {
        let root = std::env::temp_dir().join(format!("musid-dli-session-{}", std::process::id()));
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
}
