//! Provider credentials and the multi-account session store (M2 core,
//! AUTH_REGISTRY_DESIGN.md §3; tasks.md §10.4-10.6).
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, RwLock},
};

/// Account key of the legacy single-provider API (`SessionStore::get/set/clear`,
/// `FileSessionStore::load/save/clear`). Both old methods delegate here, so a
/// pre-M2 caller reads and writes exactly one account per provider.
pub const DEFAULT_ACCOUNT_ID: &str = "default";

/// Wire format version of [`ProviderSession::export`] envelopes.
const EXPORT_FORMAT: u32 = 1;

/// Provider credentials. Debug output intentionally never includes secret values.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ProviderSession {
    pub cookies: BTreeMap<String, String>,
    pub tokens: BTreeMap<String, String>,
    pub user_id: Option<String>,
    pub display_name: Option<String>,
    /// Unix seconds when the platform session expires. `None` = unknown / not
    /// provided by the platform (serde default keeps old files loadable).
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Unix seconds of the last update (serde default keeps old files loadable).
    #[serde(default)]
    pub updated_at: Option<u64>,
    /// Region tag reported by the platform (e.g. "CN"); identity metadata,
    /// not a credential (serde default keeps old files loadable).
    #[serde(default)]
    pub region: Option<String>,
}
impl std::fmt::Debug for ProviderSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSession")
            .field("cookie_count", &self.cookies.len())
            .field("token_names", &self.tokens.keys().collect::<Vec<_>>())
            .field("user_id", &self.user_id)
            .field("display_name", &self.display_name)
            .field("expires_at", &self.expires_at)
            .field("updated_at", &self.updated_at)
            .field("region", &self.region)
            .finish()
    }
}
impl ProviderSession {
    /// Export as transportable bytes (JSON envelope
    /// `{ format, provider, account, session }`). The envelope pins the
    /// origin, so `import` can refuse cross-provider / cross-account
    /// credential injection.
    pub fn export(&self, provider: &str, account: &str) -> Result<SessionExport> {
        validate_key(provider, "provider")?;
        validate_key(account, "account")?;
        let bytes = serde_json::to_vec(&SessionExportEnvelope {
            format: EXPORT_FORMAT,
            provider,
            account,
            session: self,
        })?;
        Ok(SessionExport {
            provider: provider.to_owned(),
            account: account.to_owned(),
            bytes,
        })
    }

    /// Restore from `export` bytes. The envelope's provider/account must
    /// match the requested target exactly; anything else is rejected as
    /// `InvalidInput` instead of silently planting another account's
    /// credentials.
    pub fn import(provider: &str, account: &str, bytes: &[u8]) -> Result<ProviderSession> {
        validate_key(provider, "provider")?;
        validate_key(account, "account")?;
        let envelope: SessionImportEnvelope = serde_json::from_slice(bytes)?;
        if envelope.format != EXPORT_FORMAT {
            return Err(Error::InvalidInput(
                "unsupported session export format".into(),
            ));
        }
        if envelope.provider != provider || envelope.account != account {
            return Err(Error::InvalidInput(
                "session export does not match the target provider/account".into(),
            ));
        }
        Ok(envelope.session)
    }
}

#[derive(Serialize)]
struct SessionExportEnvelope<'a> {
    format: u32,
    provider: &'a str,
    account: &'a str,
    session: &'a ProviderSession,
}
#[derive(Deserialize)]
struct SessionImportEnvelope {
    format: u32,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    account: String,
    session: ProviderSession,
}

/// Exported session bytes. Debug never prints the payload, only the origin
/// and the length (AUTH_REGISTRY_DESIGN.md §3.3).
pub struct SessionExport {
    provider: String,
    account: String,
    bytes: Vec<u8>,
}
impl fmt::Debug for SessionExport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionExport")
            .field("provider", &self.provider)
            .field("account", &self.account)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}
impl SessionExport {
    /// Origin provider id.
    pub fn provider(&self) -> &str {
        &self.provider
    }
    /// Origin account id.
    pub fn account(&self) -> &str {
        &self.account
    }
    /// Exported bytes (JSON envelope).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Consume into the raw bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

fn validate_key(key: &str, kind: &str) -> Result<()> {
    if key.trim().is_empty() {
        return Err(Error::InvalidInput(format!("{kind} id is empty")));
    }
    Ok(())
}

/// In-memory session store keyed by `(provider id, account id)`, so several
/// accounts of one provider coexist without overwriting each other
/// (tasks.md 10.4: "一个 Provider 可以存在多个账户").
#[derive(Clone, Default)]
pub struct SessionStore(Arc<RwLock<BTreeMap<(String, String), ProviderSession>>>);
impl SessionStore {
    /// Read the default account's session (legacy single-provider API).
    pub fn get(&self, provider: &str) -> Option<ProviderSession> {
        self.get_for(provider, DEFAULT_ACCOUNT_ID)
    }
    /// Write the default account's session (legacy single-provider API).
    pub fn set(&self, provider: impl Into<String>, session: ProviderSession) -> Result<()> {
        self.set_for(provider, DEFAULT_ACCOUNT_ID, session)
    }
    /// Drop the default account's session (legacy single-provider API).
    pub fn clear(&self, provider: &str) {
        self.clear_for(provider, DEFAULT_ACCOUNT_ID);
    }

    /// Read one account's session.
    pub fn get_for(&self, provider: &str, account: &str) -> Option<ProviderSession> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(provider.to_owned(), account.to_owned()))
            .cloned()
    }
    /// Write one account's session; provider and account ids must be
    /// non-empty (same rule as the legacy `set`).
    pub fn set_for(
        &self,
        provider: impl Into<String>,
        account: impl Into<String>,
        session: ProviderSession,
    ) -> Result<()> {
        let provider = provider.into();
        let account = account.into();
        validate_key(&provider, "provider")?;
        validate_key(&account, "account")?;
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert((provider, account), session);
        Ok(())
    }
    /// Drop one account's session; returns the removed session, if any.
    pub fn clear_for(&self, provider: &str, account: &str) -> Option<ProviderSession> {
        self.0
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(provider.to_owned(), account.to_owned()))
    }
    /// Accounts that currently hold a session for `provider`, sorted.
    ///
    /// Scans from `(provider, "")` with an open upper bound and stops at the
    /// first key belonging to a different provider. A closed bound on the
    /// account part would miss accounts whose first character sorts above
    /// the bound (e.g. one starting at U+10FFFF).
    pub fn list_accounts(&self, provider: &str) -> Vec<String> {
        self.0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .range((provider.to_owned(), String::new())..)
            .take_while(|((id, _), _)| id == provider)
            .map(|((_, account), _)| account.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn session_with_cookie(name: &str, value: &str) -> ProviderSession {
        let mut session = ProviderSession::default();
        session.cookies.insert(name.to_owned(), value.to_owned());
        session
    }

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

    #[test]
    fn legacy_api_reads_and_writes_the_default_account() {
        let store = SessionStore::default();
        store
            .set("netease", session_with_cookie("MUSIC_U", "legacy"))
            .unwrap();
        assert_eq!(store.get("netease").unwrap().cookies["MUSIC_U"], "legacy");
        assert_eq!(
            store
                .get_for("netease", DEFAULT_ACCOUNT_ID)
                .unwrap()
                .cookies["MUSIC_U"],
            "legacy"
        );
        // A named account never leaks into the legacy view.
        store
            .set_for("netease", "acct-2", session_with_cookie("MUSIC_U", "other"))
            .unwrap();
        assert_eq!(store.get("netease").unwrap().cookies["MUSIC_U"], "legacy");
        assert_eq!(
            store.get_for("netease", "acct-2").unwrap().cookies["MUSIC_U"],
            "other"
        );
        assert_eq!(
            store.list_accounts("netease"),
            vec!["acct-2".to_owned(), DEFAULT_ACCOUNT_ID.to_owned()]
        );
        store.clear("netease");
        assert!(store.get("netease").is_none());
        assert!(store.get_for("netease", "acct-2").is_some());
        let removed = store.clear_for("netease", "acct-2");
        assert!(removed.is_some());
        assert!(store.list_accounts("netease").is_empty());
    }

    #[test]
    fn set_for_rejects_empty_provider_or_account() {
        let store = SessionStore::default();
        let session = ProviderSession::default();
        assert!(store.set_for("", "a", session.clone()).is_err());
        assert!(store.set_for("netease", "  ", session).is_err());
        assert!(store.get("netease").is_none());
    }

    #[test]
    fn list_accounts_sees_accounts_sorting_above_any_closed_bound() {
        // Regression: a closed upper bound on the account part
        // ((p, "\\u{10FFFF}")..=(p, max)) missed accounts whose first
        // character is U+10FFFF ("\\u{10FFFF}x" > "\\u{10FFFF}").
        let store = SessionStore::default();
        for account in ["a", "\u{10FFFF}x", DEFAULT_ACCOUNT_ID] {
            store
                .set_for("netease", account, ProviderSession::default())
                .unwrap();
        }
        assert_eq!(
            store.list_accounts("netease"),
            vec![
                "a".to_owned(),
                DEFAULT_ACCOUNT_ID.to_owned(),
                "\u{10FFFF}x".to_owned()
            ]
        );
        // Neighbouring providers still do not leak into the listing.
        store
            .set_for("netezz", "\u{10FFFF}x", ProviderSession::default())
            .unwrap();
        assert_eq!(
            store.list_accounts("netease").len(),
            3,
            "netezz accounts must not leak into netease"
        );
        assert_eq!(store.list_accounts("nope"), Vec::<String>::new());
    }

    #[test]
    fn export_import_roundtrip_pins_origin_and_rejects_cross_injection() {
        let mut session = ProviderSession::default();
        session.cookies.insert("MUSIC_U".into(), "secret".into());
        session.expires_at = Some(1_900_000_000);
        session.region = Some("CN".into());
        let export = session.export("netease", "acct-7").unwrap();
        assert_eq!(export.provider(), "netease");
        assert_eq!(export.account(), "acct-7");
        // Exported artifact Debug carries origin + length, never content.
        let debug = format!("{export:?}");
        assert!(debug.contains("netease") && debug.contains("acct-7"));
        assert!(
            !debug.contains("secret"),
            "export Debug leaked payload: {debug}"
        );
        // Same origin: restore.
        let restored = ProviderSession::import("netease", "acct-7", export.as_bytes()).unwrap();
        assert_eq!(restored.cookies["MUSIC_U"], "secret");
        assert_eq!(restored.expires_at, Some(1_900_000_000));
        assert_eq!(restored.region.as_deref(), Some("CN"));
        // Cross provider / cross account / garbage: rejected, not planted.
        assert!(ProviderSession::import("qqmusic", "acct-7", export.as_bytes()).is_err());
        assert!(ProviderSession::import("netease", "other", export.as_bytes()).is_err());
        assert!(ProviderSession::import("netease", "acct-7", b"not json").is_err());
    }

    #[test]
    fn multi_account_isolation_under_concurrency() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 50;
        let store = Arc::new(SessionStore::default());
        let writers: Vec<_> = (0..THREADS)
            .map(|i| {
                let store = store.clone();
                thread::spawn(move || {
                    let account = format!("acct-{i}");
                    let marker = format!("writer-{i}");
                    for round in 0..ROUNDS {
                        let mut session = ProviderSession::default();
                        session
                            .tokens
                            .insert("sid".into(), format!("{marker}-{round}"));
                        store.set_for("netease", account.clone(), session).unwrap();
                    }
                    marker
                })
            })
            .collect();
        // Contended reader: account 0 must only ever expose writer 0 values.
        let reader = {
            let store = store.clone();
            thread::spawn(move || {
                for _ in 0..(ROUNDS * THREADS) {
                    if let Some(session) = store.get_for("netease", "acct-0") {
                        let value = session.tokens.get("sid").unwrap();
                        assert!(
                            value.starts_with("writer-0-"),
                            "cross-account bleed detected: {value}"
                        );
                    }
                }
            })
        };
        let markers: Vec<_> = writers.into_iter().map(|h| h.join().unwrap()).collect();
        reader.join().unwrap();
        // Final state per account comes from its own writer only.
        for (i, marker) in markers.iter().enumerate() {
            let account = format!("acct-{i}");
            let session = store.get_for("netease", &account).unwrap();
            let value = session.tokens.get("sid").unwrap();
            assert!(
                value.starts_with(marker) && value.ends_with(&format!("-{}", ROUNDS - 1)),
                "account {account} ended with foreign value {value}"
            );
        }
        assert_eq!(store.list_accounts("netease").len(), THREADS);
    }
}
