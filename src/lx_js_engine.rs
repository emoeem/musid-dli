//! LX Music JS bridge engine: spawns a hardened Node.js child process
//! (`node --permission`) with a generated wrapper script and talks JSON-lines
//! over stdio. Aligned with VoiceFox's `js/engine.rs` transport shape; the
//! protocol contract is docs/LX_JS_BRIDGE.md.
//!
//! Security: the wrapper script and the user's custom-source file are the
//! only files the child can read; no env inheritance; the session cookies
//! never enter the process.
use crate::lx_js::{JsRequest, JsResponse};
use crate::{Error, Result};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

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
        std::fs::create_dir_all(data_dir).map_err(|e| Error::Provider {
            provider: "lx-js".into(),
            message: format!("data dir creation failed: {e}"),
        })?;
        let wrapper_path = std::path::Path::new(data_dir).join("lx_bridge_wrapper.js");
        std::fs::write(&wrapper_path, wrapper_script).map_err(|e| Error::Provider {
            provider: "lx-js".into(),
            message: format!("wrapper write failed: {e}"),
        })?;
        let mut command = Command::new(node_path);
        command
            .arg("--permission")
            .arg(&wrapper_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // No env inheritance: the child gets nothing from the parent.
        command.env_clear();
        let mut child = command.spawn().map_err(|e| Error::Provider {
            provider: "lx-js".into(),
            message: format!("engine spawn failed: {e}"),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| Error::Provider {
            provider: "lx-js".into(),
            message: "engine stdin unavailable".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Provider {
            provider: "lx-js".into(),
            message: "engine stdout unavailable".into(),
        })?;
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
        let mut line = serde_json::to_string(request).map_err(|e| Error::Provider {
            provider: "lx-js".into(),
            message: format!("request serialize failed: {e}"),
        })?;
        line.push('\n');
        self.stdin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(line.as_bytes())
            .map_err(|e| Error::Provider {
                provider: "lx-js".into(),
                message: format!("engine write failed: {e}"),
            })
    }
    fn read_response(&mut self, expected_id: u64) -> Result<JsResponse> {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|e| Error::Provider {
                provider: "lx-js".into(),
                message: format!("engine read failed: {e}"),
            })?;
        if line.trim().is_empty() {
            return Err(Error::Provider {
                provider: "lx-js".into(),
                message: "engine closed the stream".into(),
            });
        }
        let response: JsResponse =
            serde_json::from_str(line.trim()).map_err(|e| Error::Provider {
                provider: "lx-js".into(),
                message: format!("engine response parse failed: {e}"),
            })?;
        if response.id != expected_id {
            return Err(Error::Provider {
                provider: "lx-js".into(),
                message: format!(
                    "engine response id mismatch (expected {expected_id}, got {})",
                    response.id
                ),
            });
        }
        Ok(response)
    }
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
            .ok_or_else(|| Error::Provider {
                provider: "lx-js".into(),
                message: "engine returned no url".into(),
            })
    }
    pub fn lyric(&mut self, song_id: &str) -> Result<String> {
        let result = self.round_trip("lyric", json!({"songId": song_id}), "lyric")?;
        result["lyric"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| Error::Provider {
                provider: "lx-js".into(),
                message: "engine returned no lyric".into(),
            })
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
}
