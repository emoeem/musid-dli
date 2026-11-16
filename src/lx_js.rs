//! LX Music JS custom-source bridge: protocol types and the engine seam.
//!
//! Design contract in docs/LX_JS_BRIDGE.md. This module defines the JSON-lines
//! envelope, a [`JsBridgeEngine`] trait that hosts implement to plug their own
//! JS runtime (Node with `--permission`, rquickjs, boa, …), and provider
//! wrappers that translate engine responses into musid-dli results. The
//! engine itself is deliberately out of scope: musid-dli never grows a JS
//! runtime dependency, and LX custom sources have no login flow, so the
//! bridge exposes no [`crate::provider::AuthenticationProvider`].
//!
//! Feature-gated behind `lx-js-bridge`; the default build never compiles it.
use crate::{Error, Result, models::StreamQuality};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire protocol version; bump on incompatible envelope changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// One request line.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct JsRequest {
    pub id: u64,
    pub cmd: String,
    pub params: Value,
}

/// One response line.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct JsResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Transport seam: turn a request line into a response line. The host's
/// engine implementation owns the process lifecycle and sandboxing.
pub trait JsBridgeEngine: Send + Sync {
    fn send_request(&self, request: &JsRequest) -> Result<JsResponse>;
}

/// LX quality strings mapped to musid-dli tiers. Public: hosts composing
/// their own engines reuse this mapping.
pub fn lx_quality(quality: StreamQuality) -> &'static str {
    match quality {
        StreamQuality::Lossless | StreamQuality::HiRes => "flac",
        StreamQuality::High => "320k",
        _ => "128k",
    }
}
/// Quality from an LX string; unknown strings fall back to Low.
pub fn quality_from_lx(text: &str) -> StreamQuality {
    match text {
        "flac" => StreamQuality::Lossless,
        "320k" => StreamQuality::High,
        _ => StreamQuality::Low,
    }
}
/// Convert a response into a result value or a provider error.
pub fn response_value(response: JsResponse, label: &str) -> Result<Value> {
    if response.ok {
        response.result.ok_or_else(|| Error::Provider {
            provider: "lx-js".into(),
            message: format!("{label}: ok response without result"),
        })
    } else {
        Err(Error::Provider {
            provider: "lx-js".into(),
            message: format!(
                "{label}: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown engine error".into())
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lx_quality_strings_map_to_tiers() {
        assert_eq!(lx_quality(StreamQuality::Lossless), "flac");
        assert_eq!(lx_quality(StreamQuality::High), "320k");
        assert_eq!(lx_quality(StreamQuality::Low), "128k");
        assert_eq!(quality_from_lx("flac"), StreamQuality::Lossless);
        assert_eq!(quality_from_lx("320k"), StreamQuality::High);
        assert_eq!(quality_from_lx("weird"), StreamQuality::Low);
    }

    #[test]
    fn response_errors_become_provider_errors() {
        let response = JsResponse {
            id: 1,
            ok: false,
            result: None,
            error: Some("vip required".into()),
        };
        let error = response_value(response, "musicUrl").unwrap_err();
        assert!(error.to_string().contains("vip required"));
    }
}
