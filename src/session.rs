use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};
/// Provider credentials. Debug output intentionally never includes secret values.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ProviderSession {
    pub cookies: BTreeMap<String, String>,
    pub tokens: BTreeMap<String, String>,
    pub user_id: Option<String>,
    pub display_name: Option<String>,
}
impl std::fmt::Debug for ProviderSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSession")
            .field("cookie_count", &self.cookies.len())
            .field("token_names", &self.tokens.keys().collect::<Vec<_>>())
            .field("user_id", &self.user_id)
            .field("display_name", &self.display_name)
            .finish()
    }
}
#[derive(Clone, Default)]
pub struct SessionStore(Arc<RwLock<BTreeMap<String, ProviderSession>>>);
impl SessionStore {
    pub fn get(&self, provider: &str) -> Option<ProviderSession> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(provider)
            .cloned()
    }
    pub fn set(&self, provider: impl Into<String>, session: ProviderSession) -> Result<()> {
        let provider = provider.into();
        if provider.trim().is_empty() {
            return Err(Error::InvalidInput("provider id is empty".into()));
        }
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(provider, session);
        Ok(())
    }
    pub fn clear(&self, provider: &str) {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(provider);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn debug_redacts_secret_values() {
        let mut session = ProviderSession::default();
        session
            .cookies
            .insert("MUSIC_U".into(), "secret-cookie".into());
        session
            .tokens
            .insert("refresh_token".into(), "secret-token".into());
        let debug = format!("{session:?}");
        assert!(!debug.contains("secret-cookie"));
        assert!(!debug.contains("secret-token"));
    }
}
