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

// Artwork resolver face (M6, docs/integration/RESOURCE_DESIGN.md §1):
// trait, models and the bounded artwork cache are core and always compile;
// the concrete local source is gated on the artwork+local-cover union
// (`artwork-local` names it) and no network implementation exists in M6
// (the registry slot stays empty until provider slices land real ones).
pub mod artwork;

// Lyrics cache domains (M6, RESOURCE_DESIGN.md §8): positive search/document
// domains plus the negative domain behind `lyrics-cache`; the search-domain
// face additionally needs `lyrics-search` and is gated inside the module.
#[cfg(feature = "lyrics-cache")]
pub mod lyrics_cache;
#[cfg(feature = "session-file")]
pub mod session_file;

/// Unified transport seam (injectable `Transport`, `ClientProfile`, retry
/// engine, mock). Compiled exactly under the twelve reqwest-bearing features,
/// so `--no-default-features` stays reqwest-free; the list self-validates in
/// CI's per-feature isolation job. See docs/integration/TRANSPORT_DESIGN.md §3.
#[cfg(any(
    feature = "source-netease",
    feature = "source-qqmusic",
    feature = "source-kugou",
    feature = "source-kuwo",
    feature = "source-migu",
    feature = "source-bilibili",
    feature = "source-jamendo",
    feature = "source-apple",
    feature = "source-fivesing",
    feature = "source-qianqian",
    feature = "source-joox",
    feature = "download",
))]
pub mod transport;
#[cfg(any(
    feature = "source-netease",
    feature = "source-qqmusic",
    feature = "source-kugou",
    feature = "source-kuwo",
    feature = "source-migu",
    feature = "source-bilibili",
    feature = "source-jamendo",
    feature = "source-apple",
    feature = "source-fivesing",
    feature = "source-qianqian",
    feature = "source-joox",
    feature = "download",
))]
pub use transport::{
    ClientProfile, Http, HttpMethod, MockTransport, Transport, TransportError, TransportRequest,
    TransportResponse,
};

pub use registry::ProviderDescriptor;
#[cfg(feature = "session-file")]
pub use session_file::FileSessionStore;

#[cfg(feature = "lyrics")]
pub mod lyrics;

/// Lyric conversion semantics between the voicefox `LyricData` shape and the
/// musid-dli `LyricDocument` model (M3 slice s3, feature gate at
/// `Cargo.toml` `lyrics-conversions`; contract in
/// docs/integration/MODEL_ALIGNMENT_DESIGN.md §4).
#[cfg(feature = "lyrics-conversions")]
pub mod lyrics_convert;

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

#[cfg(feature = "source-qianqian")]
pub mod source_qianqian;

#[cfg(feature = "source-joox")]
pub mod source_joox;

#[cfg(feature = "lx-js-bridge")]
pub mod lx_js;

#[cfg(feature = "lx-js-bridge")]
pub mod lx_js_engine;

#[cfg(feature = "lx-js-bridge")]
pub mod lx_js_provider;

#[cfg(feature = "local")]
pub mod formats_cue;

#[cfg(feature = "ncm")]
pub mod formats_ncm;

#[cfg(feature = "download")]
pub mod download;

#[cfg(feature = "local")]
pub mod local;

// Cover read face (docs/integration/LOCAL_ENHANCEMENT_DESIGN.md §3.11,
// M5 slice s8): embedded extraction + validated cache + external detection.
#[cfg(feature = "local-cover")]
pub mod local_cover;

// Local artwork source (M6, docs/integration/RESOURCE_DESIGN.md §1.4):
// reuses read_cover as an ArtworkProvider source; needs both the artwork
// face and the local cover face, so the `artwork-local` gate feature names
// exactly this union.
#[cfg(all(feature = "artwork", feature = "local-cover"))]
pub mod artwork_local;

// M5 local scan core (docs/integration/LOCAL_ENHANCEMENT_DESIGN.md §3).
#[cfg(feature = "local")]
pub mod local_scan;

// Watcher event stream abstraction (docs/integration/
// LOCAL_ENHANCEMENT_DESIGN.md §5.2, M5 slice s9, parity L7): notify
// recommended_watcher mapped onto the library-owned WatchEvent with a
// callback-or-std::mpsc delivery face, a std-only debounce and a
// Drop-stops-and-joins StopHandle. Zero tokio/runtime is a hard boundary;
// the platform FFI backends stay behind this opt-in feature (module docs and
// docs/DEPENDENCY_AUDIT.md, 2026-11-20 entry).
#[cfg(feature = "local-watch")]
pub mod local_watch;

// Tag-write unit shared by `local` and `download` without either implying the
// other (docs/integration/LOCAL_ENHANCEMENT_DESIGN.md §4).
#[cfg(any(feature = "local", feature = "download"))]
pub mod tag_write;

pub use error::{Error, ErrorCategory, ErrorDetail, Result};
pub use models::*;
pub use provider::*;
pub use registry::{ProviderRegistry, RegistryError};
pub use session::{ProviderSession, SessionExport, SessionStore};
