//! musid-dli: reusable music data and source aggregation library.
//!
//! This crate intentionally does not decode or play audio. Stream URLs and
//! request headers are returned to the host application (VoiceFox currently
//! delegates playback to libmpv through lx-player).

pub mod cache;
pub mod error;
pub mod formats;
pub mod health;
pub(crate) mod lrc;
pub mod matcher;
pub mod models;
pub mod provider;
pub mod registry;
pub mod resolve;
pub(crate) mod retry;
pub mod search;
pub mod session;
#[cfg(feature = "session-file")]
pub mod session_file;
pub use registry::ProviderDescriptor;
#[cfg(feature = "session-file")]
pub use session_file::FileSessionStore;

#[cfg(feature = "lyrics")]
pub mod lyrics;

#[cfg(feature = "source-youtube-music")]
pub mod source_youtube_music;

#[cfg(feature = "source-netease")]
pub mod source_netease;

#[cfg(feature = "source-qqmusic")]
pub mod source_qqmusic;

#[cfg(feature = "source-kugou")]
pub mod source_kugou;

#[cfg(feature = "source-kuwo")]
pub mod source_kuwo;

#[cfg(feature = "source-migu")]
pub mod source_migu;

#[cfg(feature = "source-bilibili")]
pub mod source_bilibili;

#[cfg(feature = "source-jamendo")]
pub mod source_jamendo;

#[cfg(feature = "source-apple")]
pub mod source_apple;

#[cfg(feature = "source-fivesing")]
pub mod source_fivesing;

#[cfg(feature = "lx-js-bridge")]
pub mod lx_js;

#[cfg(feature = "lx-js-bridge")]
pub mod lx_js_engine;

#[cfg(feature = "local")]
pub mod local;

pub use error::{Error, Result};
pub use models::*;
pub use provider::*;
pub use registry::ProviderRegistry;
