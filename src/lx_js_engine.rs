//! LX Music JS bridge engine: spawns a hardened Node.js child process
//! (`node --permission`) with a generated wrapper script and talks JSON-lines
//! over stdio. Aligned with VoiceFox's `js/engine.rs` transport shape; the
//! protocol contract is docs/LX_JS_BRIDGE.md.
//!
//! Security: the wrapper script and the user's custom-source file are the
//! only files the child can read; no env inheritance; the session cookies
//! never enter the process.
//!
//! Errors are classified per the host contract (slice `lx-js-error-mapping`,
//! AUTH_REGISTRY_DESIGN.md §8 item 9 / §9.8): sandbox / bridge-infrastructure
//! failures → `ProviderInternal`, response-envelope violations →
//! `InvalidResponse`, a missing contract result → `UnsupportedOperation`.
//! Message texts are byte-identical to the pre-slice catch-all values and
//! ride the `detail` slot.
use crate::lx_js::{JsRequest, JsResponse};
use crate::{Error, ErrorDetail, Result};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Classification constructors for the bridge error mapping (slice
/// `lx-js-error-mapping`, AUTH_REGISTRY_DESIGN.md §8 item 9 / §9.8). The
/// pre-slice catch-all was `Error::Provider { provider: "lx-js", message }`;
/// every message text below is carried over **byte-identically** into the
/// controlled [`ErrorDetail`] slot, so only the category layer changes
/// (slice compatibility clause). All three landings are non-retryable,
/// matching the legacy `ErrorCategory::Provider` behavior.
///
/// - [`sandbox_error`] — the JS sandbox / bridge infrastructure itself
///   failed (spawn, pipes, stream closed, local bridge-layer internal
///   failure): `ProviderInternal` ("provider 自身内部错误（JS 沙箱…）",
///   design §1.1).
/// - [`response_contract_error`] — the child answered, but the response
///   violates the JSON-lines envelope contract (unparseable payload, id
///   out of order): `InvalidResponse` ("缺字段/不可解析", design §1.1).
/// - [`missing_result_error`] — the child answered `ok:true` but the
///   contract-mandated result artifact (`url` / `lyric`) is absent: the
///   source cannot deliver this operation's product →
///   `UnsupportedOperation` (slice anchor: 契约缺失→UnsupportedOperation).
fn sandbox_error(detail: impl Into<String>) -> Error {
    Error::ProviderInternal {
        provider: Some("lx-js".into()),
        detail: Some(ErrorDetail::new(detail)),
    }
}

/// See [`sandbox_error`] for the classification contract.
fn response_contract_error(detail: impl Into<String>) -> Error {
    Error::InvalidResponse {
        provider: Some("lx-js".into()),
        code: None,
        detail: Some(ErrorDetail::new(detail)),
    }
}

/// See [`sandbox_error`] for the classification contract.
fn missing_result_error(detail: impl Into<String>) -> Error {
    Error::UnsupportedOperation {
        provider: Some("lx-js".into()),
        detail: Some(ErrorDetail::new(detail)),
    }
}

/// `ok:false` is a *contract-valid* response: the sandboxed handler itself
/// failed and the engine reported it. The cause text is unstructured
/// user-source free text ("vip required", …), so no finer category is
/// invented by parsing it (§9.7 residual precedent); the honest structural
/// landing is `ProviderInternal` — the provider failed internally, as
/// opposed to the request envelope being wrong. Text byte-identical to the
/// pre-slice message.
fn handler_failure(label: &str, error: Option<String>) -> Error {
    sandbox_error(format!(
        "{label}: {}",
        error.unwrap_or_else(|| "unknown engine error".into())
    ))
}

pub struct NodeJsEngine {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: AtomicU64,
}

impl NodeJsEngine {
    /// Spawn `node --permission <wrapper>` with the wrapper script generated
    /// into `data_dir`. The wrapper requires the user's custom-source file
    /// path; the host passes the LX protocol handler module inside it.
    pub fn spawn(node_path: &str, wrapper_script: &str, data_dir: &str) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| sandbox_error(format!("data dir creation failed: {e}")))?;
        let wrapper_path = std::path::Path::new(data_dir).join("lx_bridge_wrapper.js");
        std::fs::write(&wrapper_path, wrapper_script)
            .map_err(|e| sandbox_error(format!("wrapper write failed: {e}")))?;
        let mut command = Command::new(node_path);
        command
            .arg("--permission")
            .arg(&wrapper_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // No env inheritance: the child gets nothing from the parent.
        command.env_clear();
        let mut child = command
            .spawn()
            .map_err(|e| sandbox_error(format!("engine spawn failed: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| sandbox_error("engine stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| sandbox_error("engine stdout unavailable"))?;
        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            stdout: BufReader::new(stdout),
            next_id: AtomicU64::new(1),
        })
    }
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }
    fn write_request(&mut self, request: &JsRequest) -> Result<()> {
        let mut line = serde_json::to_string(request)
            .map_err(|e| sandbox_error(format!("request serialize failed: {e}")))?;
        line.push('\n');
        self.stdin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(line.as_bytes())
            .map_err(|e| sandbox_error(format!("engine write failed: {e}")))
    }
    fn read_response(&mut self, expected_id: u64) -> Result<JsResponse> {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|e| sandbox_error(format!("engine read failed: {e}")))?;
        if line.trim().is_empty() {
            return Err(sandbox_error("engine closed the stream"));
        }
        parse_response_line(&line, expected_id)
    }
}

/// Envelope validation for one response line, kept pure so the two
/// contract-violation sites are unit-testable without a child process:
/// parse the JSON line and enforce the in-order id contract. A violation
/// means the JS side broke its response contract → `InvalidResponse`
/// (design §1.1: "缺字段/不可解析"), with the pre-slice text preserved.
fn parse_response_line(line: &str, expected_id: u64) -> Result<JsResponse> {
    let response: JsResponse = serde_json::from_str(line.trim())
        .map_err(|e| response_contract_error(format!("engine response parse failed: {e}")))?;
    if response.id != expected_id {
        return Err(response_contract_error(format!(
            "engine response id mismatch (expected {expected_id}, got {})",
            response.id
        )));
    }
    Ok(response)
}

impl Drop for NodeJsEngine {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The generated wrapper: reads JSON-lines from stdin, dispatches to the
/// user's custom-source handlers, writes JSON-lines to stdout. The user
/// source exposes `musicUrl(songId, quality)`, `lyric(songId)` and
/// `search(keyword, page, limit)` on its exports.
pub const WRAPPER_SCRIPT: &str = r#"// lx-bridge wrapper (generated)
const readline = require("readline");
const userSource = require(process.env.LX_USER_SOURCE || process.argv[2]);
const rl = readline.createInterface({ input: process.stdin });
rl.on("line", async (line) => {
  let request;
  try {
    request = JSON.parse(line);
  } catch (error) {
    return;
  }
  let response;
  try {
    let result;
    if (request.cmd === "musicUrl") {
      result = { url: await userSource.musicUrl(request.params.songId, request.params.quality) };
    } else if (request.cmd === "lyric") {
      result = { lyric: await userSource.lyric(request.params.songId) };
    } else if (request.cmd === "search") {
      result = await userSource.search(
        request.params.keyword,
        request.params.page,
        request.params.limit
      );
    } else if (request.cmd === "init") {
      result = {};
    } else {
      throw new Error(`unknown cmd: ${request.cmd}`);
    }
    response = { id: request.id, ok: true, result };
  } catch (error) {
    response = { id: request.id, ok: false, error: String(error && error.message || error) };
  }
  process.stdout.write(JSON.stringify(response) + "\n");
});
"#;

impl NodeJsEngine {
    /// One round-trip: write the request, read the matching response.
    fn round_trip(&mut self, cmd: &str, params: Value, label: &str) -> Result<Value> {
        let id = self.next_id();
        let request = JsRequest {
            id,
            cmd: cmd.to_owned(),
            params,
        };
        self.write_request(&request)?;
        let response = self.read_response(id)?;
        if response.ok {
            Ok(response.result.unwrap_or(Value::Null))
        } else {
            Err(handler_failure(label, response.error))
        }
    }
    pub fn music_url(&mut self, song_id: &str, quality: &str) -> Result<String> {
        let result = self.round_trip(
            "musicUrl",
            json!({"songId": song_id, "quality": quality}),
            "musicUrl",
        )?;
        result["url"]
            .as_str()
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| missing_result_error("engine returned no url"))
    }
    pub fn lyric(&mut self, song_id: &str) -> Result<String> {
        let result = self.round_trip("lyric", json!({"songId": song_id}), "lyric")?;
        result["lyric"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| missing_result_error("engine returned no lyric"))
    }
    pub fn search(&mut self, keyword: &str, page: u32, limit: u32) -> Result<Value> {
        self.round_trip(
            "search",
            json!({"keyword": keyword, "page": page, "limit": limit}),
            "search",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCategory;

    #[test]
    fn wrapper_script_handles_all_commands() {
        for cmd in ["init", "musicUrl", "lyric", "search"] {
            assert!(WRAPPER_SCRIPT.contains(&format!("request.cmd === \"{cmd}\"")));
        }
        assert!(WRAPPER_SCRIPT.contains("readline"));
        assert!(WRAPPER_SCRIPT.contains("JSON.stringify"));
    }
    #[test]
    fn protocol_version_is_bumped_on_breaking_changes() {
        // The envelope contract lives in lx_js.rs; keep the two in sync.
        let lx = crate::lx_js::PROTOCOL_VERSION;
        assert_eq!(lx, 1);
    }

    /// The three typed landings (slice `lx-js-error-mapping`, ask anchors:
    /// JS 沙箱错误→ProviderInternal, 契约缺失→UnsupportedOperation, JS-side
    /// response-contract violations→InvalidResponse). Every legacy
    /// `Error::Provider` site in this module now lands in exactly one of
    /// them: non-retryable (as the legacy catch-all was), attributed to
    /// `lx-js`, with the pre-slice message text preserved byte-identically
    /// in the detail slot.
    #[test]
    fn bridge_errors_classify_per_the_host_contract() {
        let cases: Vec<(Error, ErrorCategory)> = vec![
            // Sandbox / bridge-infrastructure failures (spawn, pipes,
            // stream closed) → ProviderInternal.
            (
                sandbox_error("engine spawn failed: no node"),
                ErrorCategory::ProviderInternal,
            ),
            (
                sandbox_error("engine closed the stream"),
                ErrorCategory::ProviderInternal,
            ),
            (
                sandbox_error("request serialize failed: boom"),
                ErrorCategory::ProviderInternal,
            ),
            // The ok:false handler failure is contract-valid, so the cause
            // stays unstructured and lands in ProviderInternal too.
            (
                handler_failure("musicUrl", Some("vip required".to_owned())),
                ErrorCategory::ProviderInternal,
            ),
            (
                handler_failure("lyric", None),
                ErrorCategory::ProviderInternal,
            ),
            // Response-envelope violations → InvalidResponse.
            (
                response_contract_error("engine response parse failed: x"),
                ErrorCategory::InvalidResponse,
            ),
            (
                response_contract_error("engine response id mismatch (expected 1, got 2)"),
                ErrorCategory::InvalidResponse,
            ),
            // Missing contract-mandated result → UnsupportedOperation.
            (
                missing_result_error("engine returned no url"),
                ErrorCategory::UnsupportedOperation,
            ),
            (
                missing_result_error("engine returned no lyric"),
                ErrorCategory::UnsupportedOperation,
            ),
        ];
        for (error, category) in &cases {
            assert_eq!(error.category(), *category, "{error:?}");
            assert_eq!(error.provider_id(), Some("lx-js"), "{error:?}");
            assert!(
                !error.is_retryable(),
                "legacy catch-all was non-retryable; {error:?}"
            );
            assert!(
                error.detail().is_some(),
                "underlying text must be preserved: {error:?}"
            );
        }
        // Category layer only: the message texts survive byte-identically
        // (slice compatibility clause "消息文本不变").
        assert_eq!(
            handler_failure("musicUrl", Some("vip required".to_owned())).detail(),
            Some("musicUrl: vip required")
        );
        assert_eq!(
            handler_failure("lyric", None).detail(),
            Some("lyric: unknown engine error")
        );
        assert_eq!(
            missing_result_error("engine returned no url").detail(),
            Some("engine returned no url")
        );
    }

    /// The two envelope-violation sites (`read_response` parse + id check)
    /// are driven through the pure validator with real lines: unparseable
    /// payload and an out-of-order id both classify as `InvalidResponse`,
    /// and an in-order response passes through untouched.
    #[test]
    fn envelope_violations_classify_as_invalid_response() {
        let parse = parse_response_line("not json", 1).unwrap_err();
        assert_eq!(parse.category(), ErrorCategory::InvalidResponse);
        assert!(
            parse
                .detail()
                .unwrap()
                .starts_with("engine response parse failed:"),
            "got {parse:?}"
        );

        let line = r#"{"id":2,"ok":true,"result":{}}"#;
        let mismatch = parse_response_line(line, 1).unwrap_err();
        assert_eq!(mismatch.category(), ErrorCategory::InvalidResponse);
        assert_eq!(
            mismatch.detail(),
            Some("engine response id mismatch (expected 1, got 2)")
        );

        let response = parse_response_line(line, 2).expect("in-order response");
        assert_eq!(response.id, 2);
        assert!(response.ok);
    }
}
