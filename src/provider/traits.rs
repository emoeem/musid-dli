use crate::{Error, models::*};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub track_search: bool,
    pub stream_resolve: bool,
    pub lyrics: bool,
    pub playlists: bool,
    pub albums: bool,
    pub artists: bool,
    pub charts: bool,
    pub link_resolve: bool,
    pub authentication: bool,
    /// Engine-side download capability (TRANSPORT_DESIGN.md §6). A provider
    /// object may declare it for its own diagnostics, but the registry never
    /// surfaces it: neither the descriptor aggregate (`record`) nor the live
    /// capability view (`live_capabilities`) merges this bit, so a registry
    /// capability predicate on `download` is always false
    /// (AUTH_REGISTRY_DESIGN.md §5.6).
    pub download: bool,
    /// Artwork face (M6, RESOURCE_DESIGN.md §1.3). Unlike `download`, this
    /// bit IS merged like every other capability: a provider declaring it
    /// must implement [`crate::artwork::ArtworkProvider`] and register the
    /// artwork slot (the no-empty-shells rule, tasks.md §1.3#7); the slot
    /// stays empty until a real implementation exists.
    pub artwork: bool,
}
/// Authentication capability description (AUTH_REGISTRY_DESIGN.md §5.4,
/// tasks.md §5.4 "查询是否支持认证" upgraded from the single
/// `ProviderCapabilities::authentication` bool, which stays unchanged).
///
/// Default derivation in [`MusicProvider::auth_capability`]: `authentication
/// == true` maps to [`AuthCapability::Cookie`] (the real mechanism of every
/// currently-authenticating provider), otherwise [`AuthCapability::None`];
/// registering a `QrAuthenticationProvider` upgrades the descriptor to
/// [`AuthCapability::Qr`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AuthCapability {
    /// No account system / no authentication needed.
    #[default]
    None,
    /// Cookie session (netease/qq/kugou/kuwo/bilibili).
    Cookie,
    /// QR login (`QrAuthenticationProvider` registered).
    Qr,
    /// OAuth flow.
    OAuth,
    /// Anonymous device session (kuwo web token / migu sid style), not an
    /// account login.
    Anonymous,
    /// Username/password login (tasks.md §10.2; only when the platform
    /// explicitly supports it).
    Password,
    /// Bearer/static-token login (tasks.md §10.2).
    Token,
    /// Other platform-specific flow.
    Other,
}
pub trait MusicProvider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }
    /// Implementation version for the registry descriptor (tasks.md §5.4
    /// "查询 Provider 版本"). `None` = not declared.
    fn version(&self) -> Option<&str> {
        None
    }
    /// Ordering hint for host-side provider selection (tasks.md §5.4
    /// "查询 Provider 优先级"). Higher wins; the registry only surfaces the
    /// value, hosts do the sorting. Default 0 = neutral.
    fn priority(&self) -> i32 {
        0
    }
    /// Authentication capability for the registry descriptor. The default
    /// derives from the (unchanged) `authentication` bool; providers with a
    /// more precise mechanism override this in their own slices.
    fn auth_capability(&self) -> AuthCapability {
        if self.capabilities().authentication {
            AuthCapability::Cookie
        } else {
            AuthCapability::None
        }
    }
    /// Quality tiers this provider can resolve, for the registry descriptor.
    /// Empty = not declared (see `models::StreamQuality`).
    fn supported_qualities(&self) -> Vec<crate::models::StreamQuality> {
        Vec::new()
    }
}
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub keyword: String,
    pub page: u32,
    pub page_size: u32,
}
impl SearchQuery {
    pub fn new(keyword: impl Into<String>) -> Self {
        Self {
            keyword: keyword.into(),
            page: 1,
            page_size: 30,
        }
    }
}
#[derive(Debug, Clone, Default)]
pub struct SearchPage {
    pub items: Vec<Track>,
    pub total: Option<u64>,
    pub has_more: bool,
}
#[derive(Debug, Clone, Copy)]
pub struct ResolveOptions {
    pub quality: StreamQuality,
}
impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            quality: StreamQuality::High,
        }
    }
}
#[async_trait]
pub trait TrackSearch: MusicProvider {
    async fn search(&self, query: &SearchQuery) -> Result<SearchPage, Error>;
}
#[async_trait]
pub trait StreamResolver: MusicProvider {
    async fn resolve(&self, track: &Track, options: &ResolveOptions) -> Result<StreamInfo, Error>;
}
#[async_trait]
pub trait LyricsProvider: MusicProvider {
    async fn lyrics(&self, track: &Track) -> Result<Option<LyricDocument>, Error>;
}
#[async_trait]
pub trait CollectionProvider: MusicProvider {
    async fn playlist(&self, id: &str) -> Result<Playlist, Error>;
    async fn playlist_tracks(&self, _id: &str) -> Result<Vec<Track>, Error> {
        Err(Error::Unsupported(self.id().into()))
    }
}
#[async_trait]
pub trait PlaylistProvider: MusicProvider {
    async fn categories(&self) -> Result<Vec<PlaylistCategory>, Error>;
    async fn playlists(
        &self,
        category: &str,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<Playlist>, Error>;
    async fn search_playlists(&self, keyword: &str, page: u32) -> Result<Vec<Playlist>, Error>;
    async fn user_playlists(&self, page: u32, page_size: u32) -> Result<Vec<Playlist>, Error>;
}
#[async_trait]
pub trait AlbumProvider: MusicProvider {
    async fn album(&self, id: &str) -> Result<Album, Error>;
    async fn album_tracks(&self, _id: &str) -> Result<Vec<Track>, Error> {
        Err(Error::Unsupported(self.id().into()))
    }
}
#[async_trait]
pub trait ArtistProvider: MusicProvider {
    async fn artist(&self, id: &str) -> Result<Artist, Error>;
    async fn artist_top_tracks(&self, _id: &str) -> Result<Vec<Track>, Error> {
        Err(Error::Unsupported(self.id().into()))
    }
    async fn artist_albums(
        &self,
        _id: &str,
        _page: u32,
        _page_size: u32,
    ) -> Result<Vec<Album>, Error> {
        Err(Error::Unsupported(self.id().into()))
    }
}
#[async_trait]
pub trait ChartProvider: MusicProvider {
    async fn charts(&self) -> Result<Vec<Chart>, Error>;
    async fn chart_tracks(&self, id: &str) -> Result<Vec<ChartEntry>, Error>;
}
#[derive(Debug, Clone)]
pub enum ResolvedLink {
    Track(Track),
    Playlist(Playlist),
    Album(Album),
    Artist(Artist),
}
#[async_trait]
pub trait LinkResolver: MusicProvider {
    async fn resolve_link(&self, url: &str) -> Result<ResolvedLink, Error>;
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStatus {
    LoggedOut,
    LoggedIn {
        user_id: Option<String>,
        display_name: Option<String>,
    },
    Expired,
}

/// Unified authentication state machine (M2 core, AUTH_REGISTRY_DESIGN.md §2,
/// tasks.md §10.2-10.6). Closed seven-state set; the legacy three-state
/// `LoginStatus` folds into it losslessly via `From`.
///
/// `Authenticated` means "a session is valid" and does NOT imply an account
/// identity: anonymous device sessions (kuwo web token style) are
/// `Authenticated` with `identity: None` when folded back through
/// [`AuthState::to_login_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AuthState {
    /// No session.
    #[default]
    LoggedOut,
    /// A login flow is in progress (QR shown/scanned/reshown).
    PendingQrScan,
    /// A session is valid (account identity lives in `ProviderSession`).
    Authenticated,
    /// The session expired; refresh or re-login can recover.
    Expired,
    /// The platform invalidated the session (revocation, password change,
    /// device kick); refresh cannot recover, a fresh login is required.
    Invalidated,
    /// A session refresh is in flight (`refresh_session`).
    Refreshing,
    /// The session exists but the platform demands interactive
    /// re-authorization (risk control, scope change).
    ReauthorizationRequired,
}

/// Guard failure of [`AuthState::transition`]. A programming-contract type,
/// deliberately not a `crate::Error` variant (state misuse is not a platform
/// failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthTransitionError {
    pub from: AuthState,
    pub to: AuthState,
}
impl std::fmt::Display for AuthTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal auth state transition: {:?} -> {:?}",
            self.from, self.to
        )
    }
}
impl std::error::Error for AuthTransitionError {}

impl AuthState {
    /// Whether `next` may follow `self` (AUTH_REGISTRY_DESIGN.md §2.2 table).
    /// Self-transitions are always allowed so idempotent marking (e.g. a
    /// second `mark_session_expired`) never errors.
    pub fn can_transition_to(self, next: AuthState) -> bool {
        use AuthState::*;
        match (self, next) {
            // Idempotent self-transitions.
            (a, b) if a == b => true,
            // Expired: a persisted expired session can be restored/imported
            // (validate_session over an expired stored session).
            (LoggedOut, PendingQrScan | Authenticated | Expired | Refreshing) => true,
            (PendingQrScan, LoggedOut | Authenticated | ReauthorizationRequired) => true,
            (Authenticated, _) => true,
            // LoggedOut: logout (the documented revoke) must stay reachable
            // while a refresh is in flight.
            (
                Refreshing,
                LoggedOut | Authenticated | Expired | Invalidated | ReauthorizationRequired,
            ) => true,
            (Expired, LoggedOut | PendingQrScan | Authenticated | Invalidated | Refreshing) => true,
            (Invalidated, LoggedOut | PendingQrScan | Authenticated) => true,
            (ReauthorizationRequired, LoggedOut | PendingQrScan | Authenticated) => true,
            _ => false,
        }
    }

    /// Guarded write of the state machine; see [`Self::can_transition_to`].
    pub fn transition(&mut self, next: AuthState) -> Result<(), AuthTransitionError> {
        if self.can_transition_to(next) {
            *self = next;
            Ok(())
        } else {
            Err(AuthTransitionError {
                from: *self,
                to: next,
            })
        }
    }

    /// Legacy three-state view. The reverse fold needs the account identity
    /// (which lives in `ProviderSession`, not in the state), so this is a
    /// parameterized method rather than a `From` impl.
    ///
    /// - `LoggedOut | PendingQrScan` -> `LoggedOut`;
    /// - `Authenticated | Refreshing` -> `LoggedIn` (with `identity: None`
    ///   folding to `LoggedIn { user_id: None, .. }`: anonymous
    ///   authenticated);
    /// - `Expired | Invalidated | ReauthorizationRequired` -> `Expired`
    ///   (the legacy model has one "must re-login" state).
    pub fn to_login_status(self, identity: Option<(&str, &str)>) -> LoginStatus {
        match self {
            AuthState::LoggedOut | AuthState::PendingQrScan => LoginStatus::LoggedOut,
            AuthState::Authenticated | AuthState::Refreshing => LoginStatus::LoggedIn {
                user_id: identity.map(|(id, _)| id.to_owned()),
                display_name: identity.map(|(_, name)| name.to_owned()),
            },
            AuthState::Expired | AuthState::Invalidated | AuthState::ReauthorizationRequired => {
                LoginStatus::Expired
            }
        }
    }
}

impl From<&LoginStatus> for AuthState {
    fn from(value: &LoginStatus) -> Self {
        match value {
            LoginStatus::LoggedOut => AuthState::LoggedOut,
            LoginStatus::LoggedIn { .. } => AuthState::Authenticated,
            LoginStatus::Expired => AuthState::Expired,
        }
    }
}
impl From<LoginStatus> for AuthState {
    fn from(value: LoginStatus) -> Self {
        AuthState::from(&value)
    }
}

impl From<&QrLoginStatus> for AuthState {
    /// Lossy fold of a login-flow status into a session state
    /// (AUTH_REGISTRY_DESIGN.md §2.3). QR expiry and `NetworkError` stay
    /// inside the pending flow (a fresh code is shown / the poll continues);
    /// `ServerError`/`Failed`/`InvalidSession` end the attempt without a
    /// session (MODEL_ALIGNMENT_DESIGN.md §6.1).
    fn from(value: &QrLoginStatus) -> Self {
        match value {
            QrLoginStatus::Waiting
            | QrLoginStatus::Scanned
            | QrLoginStatus::Expired
            | QrLoginStatus::NetworkError => AuthState::PendingQrScan,
            QrLoginStatus::Success => AuthState::Authenticated,
            QrLoginStatus::RiskControl => AuthState::ReauthorizationRequired,
            QrLoginStatus::ServerError | QrLoginStatus::Failed | QrLoginStatus::InvalidSession => {
                AuthState::LoggedOut
            }
        }
    }
}

/// QR polling status machine (G8, MODEL_ALIGNMENT_DESIGN.md §6.1; voicefox
/// `core/src/model/login.rs:14-33`). Nine states: the two in-progress ones
/// (`Waiting`/`Scanned`), the three transient ones (QR still valid, keep
/// polling, [`Self::is_transient`]) and the four terminal ones (attempt over,
/// [`Self::is_terminal`]). Serde casing is upstream's `snake_case` wire
/// format (`login.rs:13`), so a host can move a serialized status across the
/// boundary without a name map (§1 R8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QrLoginStatus {
    /// Waiting for a scan.
    Waiting,
    /// Scanned, waiting for the user to confirm on the device.
    Scanned,
    /// Login succeeded; the session cookie was written by the source side.
    Success,
    /// QR expired; a fresh code must be generated.
    Expired,
    /// The platform demanded risk control/verification; the QR is still
    /// valid, keep polling.
    RiskControl,
    /// Server-side temporary failure; the QR is still valid, keep polling.
    ServerError,
    /// Login failed.
    Failed,
    /// Network temporarily unavailable; the QR is still valid, keep polling.
    NetworkError,
    /// The local session was invalidated; a fresh QR is needed.
    InvalidSession,
}

impl QrLoginStatus {
    /// The QR is still valid and the poll loop must keep going (voicefox
    /// `login.rs:36-42`): {`NetworkError`, `RiskControl`, `ServerError`}.
    pub const fn is_transient(self) -> bool {
        matches!(
            self,
            Self::NetworkError | Self::RiskControl | Self::ServerError
        )
    }

    /// The attempt has ended (voicefox `login.rs:44-49`):
    /// {`Success`, `Expired`, `Failed`, `InvalidSession`}.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Success | Self::Expired | Self::Failed | Self::InvalidSession
        )
    }
}

/// QR login channel (G9, MODEL_ALIGNMENT_DESIGN.md §6.2; voicefox
/// `core/src/model/login.rs:55-73`). One source can offer several QR
/// channels; the channel only changes how the credential is obtained, the
/// resulting session is identical, so the kind lives only on the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QrLoginKind {
    /// The source's own QR channel (QQ Music = QQ scan;
    /// netease/bilibili/kugou = their own apps).
    #[default]
    Standard,
    /// WeChat scan. Only QQ Music offers it today (WeChat account into
    /// QQ Music).
    WeChat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QrLoginSession {
    /// Polling ticket.
    pub key: String,
    /// QR content the host renders. Today's producers put the QR image here
    /// as a `data:image/...;base64` URI where the platform serves bytes.
    pub url: String,
    /// QR validity in seconds.
    pub expires_in: u64,
    /// QR image bytes base64 (QQ=PNG / WeChat=JPEG; voicefox
    /// `login.rs:86-89`), alternative rendering of the same code. QQ Music
    /// fills it from the bytes already in hand at both create sites; the
    /// netease/kugou/bilibili APIs return only an H5 link (no image bytes
    /// in hand), so those producers keep it `None`
    /// (MODEL_ALIGNMENT_DESIGN.md §6); `#[serde(default)]` keeps
    /// pre-s5 session JSON loadable (§1 R1).
    #[serde(default)]
    pub image_png: Option<String>,
    /// Which channel this session belongs to (G9). `#[serde(default)]`
    /// loads pre-s5 JSON as [`QrLoginKind::Standard`], the legacy semantics
    /// (§1 R1/R4).
    #[serde(default)]
    pub kind: QrLoginKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QrLoginPoll {
    pub status: QrLoginStatus,
    /// Status text for direct UI display.
    pub message: String,
    pub user_id: Option<String>,
    pub display_name: Option<String>,
}

impl QrLoginPoll {
    /// voicefox-shaped single display name for the
    /// `QrLoginPoll` ↔ `QrLoginResult { status, message, user_name }` fold
    /// (MODEL_ALIGNMENT_DESIGN.md §6.3; voicefox `login.rs:99-106` has one
    /// slot): `display_name` when present, else `user_id` — prefer the
    /// human-readable name, never concatenate. Both `None` fold to `None`:
    /// missing identity is carried as a missing name, not collapsed into an
    /// empty string. The reverse direction cannot split a `user_name` back
    /// into the pair: lossy by design.
    pub fn user_name(&self) -> Option<String> {
        self.display_name.clone().or_else(|| self.user_id.clone())
    }
}
#[async_trait]
pub trait QrAuthenticationProvider: MusicProvider {
    async fn create_qr_login(&self) -> Result<QrLoginSession, Error>;
    async fn poll_qr_login(&self, key: &str) -> Result<QrLoginPoll, Error>;
}

#[async_trait]
pub trait AuthenticationProvider: MusicProvider {
    async fn login_status(&self) -> Result<LoginStatus, Error>;
    /// Revoke the current session (tasks.md 10.5#7 "支持主动注销"; this is
    /// the revoke operation, no separate method on purpose).
    async fn logout(&self) -> Result<(), Error>;
    /// Refresh the stored session (tasks.md 10.3). Default: not implemented,
    /// reported as `Error::UnsupportedOperation` carrying the provider id.
    async fn refresh_session(&self) -> Result<AuthState, Error> {
        Err(Error::UnsupportedOperation {
            provider: Some(self.id().to_owned()),
            detail: Some(crate::error::ErrorDetail::new(
                "session refresh is not implemented",
            )),
        })
    }
    /// Validate the current session (tasks.md 10.3, 10.5#6). Default: derive
    /// the state locally from `login_status` without any network probe;
    /// providers with a cheap liveness endpoint override this.
    async fn validate_session(&self) -> Result<AuthState, Error> {
        Ok(AuthState::from(&self.login_status().await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_states() -> [AuthState; 7] {
        [
            AuthState::LoggedOut,
            AuthState::PendingQrScan,
            AuthState::Authenticated,
            AuthState::Expired,
            AuthState::Invalidated,
            AuthState::Refreshing,
            AuthState::ReauthorizationRequired,
        ]
    }

    /// The §2.2 transition table of AUTH_REGISTRY_DESIGN.md, pinned 7x7.
    #[test]
    fn auth_state_transition_table_is_pinned() {
        // (from, to, allowed) for every NON-self combination; self moves are
        // separately pinned below.
        let allowed: &[(AuthState, AuthState)] = &[
            (AuthState::LoggedOut, AuthState::PendingQrScan),
            (AuthState::LoggedOut, AuthState::Authenticated),
            // A persisted expired session can be restored/imported.
            (AuthState::LoggedOut, AuthState::Expired),
            (AuthState::LoggedOut, AuthState::Refreshing),
            (AuthState::PendingQrScan, AuthState::LoggedOut),
            (AuthState::PendingQrScan, AuthState::Authenticated),
            (AuthState::PendingQrScan, AuthState::ReauthorizationRequired),
            (AuthState::Authenticated, AuthState::LoggedOut),
            (AuthState::Authenticated, AuthState::PendingQrScan),
            (AuthState::Authenticated, AuthState::Expired),
            (AuthState::Authenticated, AuthState::Invalidated),
            (AuthState::Authenticated, AuthState::Refreshing),
            (AuthState::Authenticated, AuthState::ReauthorizationRequired),
            // Logout (the documented revoke) stays reachable mid-refresh.
            (AuthState::Refreshing, AuthState::LoggedOut),
            (AuthState::Refreshing, AuthState::Authenticated),
            (AuthState::Refreshing, AuthState::Expired),
            (AuthState::Refreshing, AuthState::Invalidated),
            (AuthState::Refreshing, AuthState::ReauthorizationRequired),
            (AuthState::Expired, AuthState::LoggedOut),
            (AuthState::Expired, AuthState::PendingQrScan),
            (AuthState::Expired, AuthState::Authenticated),
            (AuthState::Expired, AuthState::Invalidated),
            (AuthState::Expired, AuthState::Refreshing),
            (AuthState::Invalidated, AuthState::LoggedOut),
            (AuthState::Invalidated, AuthState::PendingQrScan),
            (AuthState::Invalidated, AuthState::Authenticated),
            (AuthState::ReauthorizationRequired, AuthState::LoggedOut),
            (AuthState::ReauthorizationRequired, AuthState::PendingQrScan),
            (AuthState::ReauthorizationRequired, AuthState::Authenticated),
        ];
        for from in all_states() {
            for to in all_states() {
                // Self-transitions are always allowed (idempotent marking).
                let pinned = from == to || allowed.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    pinned,
                    "{from:?} -> {to:?} diverges from the pinned table"
                );
                // transition() agrees with the predicate and enforces it.
                let mut state = from;
                match state.transition(to) {
                    Ok(()) => assert_eq!(state, to),
                    Err(err) => {
                        assert_eq!(err.from, from);
                        assert_eq!(err.to, to);
                        assert_eq!(state, from, "failed transition must not mutate");
                    }
                }
            }
        }
    }

    #[test]
    fn login_status_folds_into_auth_state_losslessly() {
        assert_eq!(
            AuthState::from(&LoginStatus::LoggedOut),
            AuthState::LoggedOut
        );
        assert_eq!(
            AuthState::from(&LoginStatus::LoggedIn {
                user_id: Some("u1".into()),
                display_name: Some("名".into()),
            }),
            AuthState::Authenticated
        );
        assert_eq!(AuthState::from(&LoginStatus::Expired), AuthState::Expired);
    }

    #[test]
    fn qr_login_status_folds_lossily_per_table() {
        assert_eq!(
            AuthState::from(&QrLoginStatus::Waiting),
            AuthState::PendingQrScan
        );
        assert_eq!(
            AuthState::from(&QrLoginStatus::Scanned),
            AuthState::PendingQrScan
        );
        assert_eq!(
            AuthState::from(&QrLoginStatus::Expired),
            AuthState::PendingQrScan
        );
        assert_eq!(
            AuthState::from(&QrLoginStatus::Success),
            AuthState::Authenticated
        );
        assert_eq!(
            AuthState::from(&QrLoginStatus::RiskControl),
            AuthState::ReauthorizationRequired
        );
        assert_eq!(
            AuthState::from(&QrLoginStatus::ServerError),
            AuthState::LoggedOut
        );
        assert_eq!(
            AuthState::from(&QrLoginStatus::Failed),
            AuthState::LoggedOut
        );
        // §6.1 extension: the poll continues inside the pending flow on a
        // transient network error; an invalidated local session needs a
        // fresh QR and folds to LoggedOut.
        assert_eq!(
            AuthState::from(&QrLoginStatus::NetworkError),
            AuthState::PendingQrScan
        );
        assert_eq!(
            AuthState::from(&QrLoginStatus::InvalidSession),
            AuthState::LoggedOut
        );
    }

    /// The nine-state set of G8 (voicefox `login.rs:14-33`) with the
    /// upstream classification (`login.rs:36-49`).
    #[test]
    fn qr_login_status_transient_and_terminal_tables_are_pinned() {
        let all: &[(QrLoginStatus, bool, bool)] = &[
            (QrLoginStatus::Waiting, false, false),
            (QrLoginStatus::Scanned, false, false),
            (QrLoginStatus::Success, false, true),
            (QrLoginStatus::Expired, false, true),
            (QrLoginStatus::RiskControl, true, false),
            (QrLoginStatus::ServerError, true, false),
            (QrLoginStatus::Failed, false, true),
            (QrLoginStatus::NetworkError, true, false),
            (QrLoginStatus::InvalidSession, false, true),
        ];
        assert_eq!(all.len(), 9, "the closed nine-state set must not shrink");
        for (status, transient, terminal) in all {
            assert_eq!(status.is_transient(), *transient, "{status:?} transient");
            assert_eq!(status.is_terminal(), *terminal, "{status:?} terminal");
        }
        // Transient and terminal are disjoint; what is neither is in-progress.
        for (status, transient, terminal) in all {
            assert!(
                !(*transient && *terminal),
                "{status:?} cannot be both transient and terminal"
            );
            assert!(
                *transient
                    || *terminal
                    || matches!(status, QrLoginStatus::Waiting | QrLoginStatus::Scanned),
                "{status:?} is neither transient nor terminal nor in-progress"
            );
        }
    }

    /// G8: the wire format is upstream's snake_case (`login.rs:13`), and the
    /// upstream literals decode back (§1 R8).
    #[test]
    fn qr_login_status_wire_format_is_upstream_snake_case() {
        let pinned = [
            (QrLoginStatus::Waiting, "\"waiting\""),
            (QrLoginStatus::Scanned, "\"scanned\""),
            (QrLoginStatus::Success, "\"success\""),
            (QrLoginStatus::Expired, "\"expired\""),
            (QrLoginStatus::RiskControl, "\"risk_control\""),
            (QrLoginStatus::ServerError, "\"server_error\""),
            (QrLoginStatus::Failed, "\"failed\""),
            (QrLoginStatus::NetworkError, "\"network_error\""),
            (QrLoginStatus::InvalidSession, "\"invalid_session\""),
        ];
        for (status, json) in pinned {
            assert_eq!(serde_json::to_string(&status).unwrap(), json);
            assert_eq!(serde_json::from_str::<QrLoginStatus>(json).unwrap(), status);
        }
    }

    /// G9: the kind defaults to `Standard` on the wire and the upstream
    /// literals decode (`login.rs:55-73`). `WeChat` serializes `we_chat`:
    /// upstream applies the same `rename_all = "snake_case"` to the same
    /// variant name (`login.rs:56`), so the wire values are identical by
    /// construction (§1 R8).
    #[test]
    fn qr_login_kind_default_and_wire_format_are_pinned() {
        assert_eq!(QrLoginKind::default(), QrLoginKind::Standard);
        assert_eq!(
            serde_json::to_string(&QrLoginKind::Standard).unwrap(),
            "\"standard\""
        );
        assert_eq!(
            serde_json::to_string(&QrLoginKind::WeChat).unwrap(),
            "\"we_chat\""
        );
        assert_eq!(
            serde_json::from_str::<QrLoginKind>("\"we_chat\"").unwrap(),
            QrLoginKind::WeChat
        );
    }

    /// §1 R1/R4: pre-s5 stored session JSON (key/url/expires_in only) loads
    /// with the documented defaults, and upstream voicefox session JSON
    /// (`login.rs:77-92`, including its `source` key) loads unchanged —
    /// unknown keys are ignored (R2).
    #[test]
    fn qr_login_session_legacy_json_loads_with_documented_defaults() {
        let legacy: QrLoginSession = serde_json::from_str(
            r#"{"key":"k1","url":"https://example.test/qr","expires_in":300}"#,
        )
        .unwrap();
        assert_eq!(legacy.key, "k1");
        assert_eq!(legacy.expires_in, 300);
        assert_eq!(legacy.image_png, None);
        assert_eq!(legacy.kind, QrLoginKind::Standard);

        let upstream: QrLoginSession = serde_json::from_str(
            r#"{"source":"tx","kind":"we_chat","key":"uuid","url":"","image_png":"aGVsbG8=","expires_in":120}"#,
        )
        .unwrap();
        assert_eq!(upstream.kind, QrLoginKind::WeChat);
        assert_eq!(upstream.image_png.as_deref(), Some("aGVsbG8="));
    }

    /// §1 R1/R8: a fully populated session round-trips field by field and
    /// the wire keys are upstream's (`login.rs:77-92`).
    #[test]
    fn qr_login_session_round_trip_preserves_every_field() {
        let session = QrLoginSession {
            key: "k".into(),
            url: "data:image/png;base64,AAAA".into(),
            expires_in: 180,
            image_png: Some("AAAA".into()),
            kind: QrLoginKind::WeChat,
        };
        let value = serde_json::to_value(&session).unwrap();
        let object = value.as_object().unwrap();
        for key in ["key", "url", "expires_in", "image_png", "kind"] {
            assert!(object.contains_key(key), "missing wire key {key}");
        }
        assert_eq!(
            serde_json::from_value::<QrLoginSession>(value).unwrap(),
            session
        );
    }

    /// §6.3 poll round-trip: the poll shape serializes and the user_name
    /// fold rule (`display_name.or(user_id)`) pins every missing-field case.
    #[test]
    fn qr_login_poll_round_trip_and_user_name_fold_rule() {
        let poll = QrLoginPoll {
            status: QrLoginStatus::Success,
            message: "登录成功".into(),
            user_id: Some("u1".into()),
            display_name: Some("名".into()),
        };
        let value = serde_json::to_value(&poll).unwrap();
        assert_eq!(value["status"], "success");
        assert_eq!(serde_json::from_value::<QrLoginPoll>(value).unwrap(), poll);
        // Display name wins when present...
        assert_eq!(poll.user_name().as_deref(), Some("名"));
        // ...the account id is the fallback...
        let only_id = QrLoginPoll {
            display_name: None,
            ..poll.clone()
        };
        assert_eq!(only_id.user_name().as_deref(), Some("u1"));
        // ...and missing identity stays missing, never an empty string.
        let none = QrLoginPoll {
            user_id: None,
            ..only_id
        };
        assert_eq!(none.user_name(), None);
    }

    #[test]
    fn to_login_status_folds_must_relogin_states_into_expired() {
        let identity = Some(("u1", "名"));
        assert_eq!(
            AuthState::Authenticated.to_login_status(identity),
            LoginStatus::LoggedIn {
                user_id: Some("u1".into()),
                display_name: Some("名".into()),
            }
        );
        // Anonymous authenticated (kuwo web token): identity-less LoggedIn.
        assert_eq!(
            AuthState::Authenticated.to_login_status(None),
            LoginStatus::LoggedIn {
                user_id: None,
                display_name: None,
            }
        );
        assert_eq!(
            AuthState::Refreshing.to_login_status(identity),
            AuthState::Authenticated.to_login_status(identity)
        );
        for state in [
            AuthState::Expired,
            AuthState::Invalidated,
            AuthState::ReauthorizationRequired,
        ] {
            assert_eq!(state.to_login_status(identity), LoginStatus::Expired);
        }
        for state in [AuthState::LoggedOut, AuthState::PendingQrScan] {
            assert_eq!(state.to_login_status(identity), LoginStatus::LoggedOut);
        }
    }

    struct StubAuth {
        status: LoginStatus,
    }
    impl MusicProvider for StubAuth {
        fn id(&self) -> &str {
            "stub"
        }
        fn name(&self) -> &str {
            "Stub"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                authentication: true,
                ..Default::default()
            }
        }
    }
    #[async_trait]
    impl AuthenticationProvider for StubAuth {
        async fn login_status(&self) -> Result<LoginStatus, Error> {
            Ok(self.status.clone())
        }
        async fn logout(&self) -> Result<(), Error> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn default_validate_session_derives_from_login_status_without_network() {
        let stub = StubAuth {
            status: LoginStatus::Expired,
        };
        assert_eq!(stub.validate_session().await.unwrap(), AuthState::Expired);
    }

    #[tokio::test]
    async fn default_refresh_session_reports_unsupported_operation_with_provider_id() {
        let stub = StubAuth {
            status: LoginStatus::LoggedOut,
        };
        let error = stub.refresh_session().await.unwrap_err();
        assert_eq!(error.category(), crate::ErrorCategory::UnsupportedOperation);
        assert_eq!(error.provider_id(), Some("stub"));
    }

    #[test]
    fn default_music_provider_descriptors_are_empty() {
        struct Bare;
        impl MusicProvider for Bare {
            fn id(&self) -> &str {
                "bare"
            }
            fn name(&self) -> &str {
                "Bare"
            }
        }
        let bare = Bare;
        assert_eq!(bare.version(), None);
        assert_eq!(bare.auth_capability(), AuthCapability::None);
        assert!(bare.supported_qualities().is_empty());
        assert!(!bare.capabilities().download);
    }

    #[test]
    fn auth_capability_default_derives_from_the_authentication_bool() {
        struct NoAuth;
        impl MusicProvider for NoAuth {
            fn id(&self) -> &str {
                "noauth"
            }
            fn name(&self) -> &str {
                "NoAuth"
            }
        }
        assert_eq!(NoAuth.auth_capability(), AuthCapability::None);
        let stub = StubAuth {
            status: LoginStatus::LoggedOut,
        };
        assert_eq!(stub.auth_capability(), AuthCapability::Cookie);
    }
}
