//! `cormex mcp` — a stdio ↔ HTTP bridge for hosts that only speak stdio.
//!
//! Design: docs/plan/session-archive-plan.md P3-6.
//!
//! # Why a bridge rather than a second server
//!
//! The memory lives in cortexd; a stdio host (Codex, and anything else that
//! spawns a child process) cannot reach it directly. Writing a second MCP server
//! inside the CLI would mean two implementations of every tool, and the one
//! that is not being tested is the one that rots.
//!
//! So this is a **transport adapter**: it reads JSON-RPC lines from stdin,
//! forwards them to `/mcp`, and writes the answers back. It knows nothing about
//! tools.
//!
//! # The two headers the 2026-07-28 protocol requires
//!
//! `Mcp-Method` and `Mcp-Name`. A stateless server routes on them without
//! parsing the body, and a request without them is rejected — with an error
//! that names the header rather than saying "this endpoint is not for you".
//!
//! # Sessions, for the old protocol
//!
//! A server on the legacy protocol answers `initialize` with an
//! `Mcp-Session-Id` header, and every later request has to carry it back. The
//! bridge remembers the one it saw and sends it; on the stateless protocol the
//! header never appears and nothing changes.

use std::io::{BufRead as _, Write as _};

use anyhow::{Context as _, Result};

use crate::client::ApiClient;

/// One JSON-RPC message, as far as the bridge needs to understand it.
///
/// **Only `method` and `params.name` are read.** Everything else is forwarded
/// verbatim: the bridge is a transport, and a field it does not understand is
/// still the server's business.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Option<String>,
    pub name: Option<String>,
}

/// Pull the two header values out of a request body.
///
/// Never fails: a body that is not JSON is forwarded without the headers, and
/// the server answers with its own error — which is more informative than
/// anything the bridge could invent.
#[must_use]
pub fn headers_for(body: &str) -> Vec<(&'static str, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
        out.push(("mcp-method", method.to_owned()));
    }
    if let Some(name) = v
        .get("params")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    {
        out.push(("mcp-name", name.to_owned()));
    }
    out
}

/// Extract the JSON-RPC payloads from a response.
///
/// The server may answer either a plain JSON body or an SSE stream whose
/// `data:` frames carry the payloads. A bridge that only understood one would
/// work against one protocol version and silently produce nothing against the
/// other — the host would see a request with no reply.
#[must_use]
pub fn payloads(content_type: &str, body: &str) -> Vec<String> {
    if content_type.contains("text/event-stream") {
        body.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| d.trim().to_owned())
            .filter(|d| !d.is_empty())
            .collect()
    } else if body.trim().is_empty() {
        Vec::new()
    } else {
        vec![body.trim().to_owned()]
    }
}

/// Run the bridge until stdin closes.
///
/// # Errors
/// The server is unreachable, or stdout cannot be written.
pub async fn run(client: ApiClient) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut session: Option<String> = None;

    for line in stdin.lock().lines() {
        let line = line.context("could not read stdin")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut headers = headers_for(trimmed);
        if let Some(id) = &session {
            headers.push(("mcp-session-id", id.clone()));
        }
        headers.push(("content-type", "application/json".to_owned()));
        headers.push(("accept", "application/json, text/event-stream".to_owned()));

        let (content_type, body, session_id) = client
            .post_raw("/mcp", &headers, trimmed.as_bytes().to_vec())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // The legacy protocol hands back a session id on `initialize`, and
        // every later request has to carry it
        if let Some(id) = session_id {
            session = Some(id);
        }

        for payload in payloads(&content_type, &body) {
            writeln!(stdout, "{payload}").context("could not write stdout")?;
        }
        stdout.flush().context("could not flush stdout")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two headers come out of the body.
    #[test]
    fn the_method_and_name_headers_are_extracted() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_search","arguments":{}}}"#;
        let headers = headers_for(body);
        assert!(headers.contains(&("mcp-method", "tools/call".to_owned())));
        assert!(headers.contains(&("mcp-name", "memory_search".to_owned())));
    }

    /// A request without `params.name` gets only the method header.
    #[test]
    fn a_request_without_a_name_gets_no_name_header() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let headers = headers_for(body);
        assert_eq!(headers, vec![("mcp-method", "tools/list".to_owned())]);
    }

    /// A body that is not JSON is forwarded without headers, not rejected here.
    ///
    /// The server's error is more informative than anything the bridge could
    /// invent, and the bridge is a transport.
    #[test]
    fn a_non_json_body_yields_no_headers() {
        assert!(headers_for("not json").is_empty());
    }

    /// A plain JSON response is one payload.
    #[test]
    fn a_json_response_is_one_payload() {
        let payloads = payloads(
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        );
        assert_eq!(payloads.len(), 1);
    }

    /// An SSE response's `data:` frames are the payloads.
    ///
    /// A bridge that only understood JSON would produce nothing against the
    /// streaming transport, and the host would see a request with no reply.
    #[test]
    fn an_sse_response_yields_its_data_frames() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let payloads = payloads("text/event-stream", body);
        assert_eq!(payloads.len(), 1);
        assert!(payloads[0].contains("\"result\""));
    }

    /// An empty body yields nothing, rather than an empty line on stdout.
    #[test]
    fn an_empty_body_yields_nothing() {
        assert!(payloads("application/json", "   ").is_empty());
        assert!(payloads("text/event-stream", "").is_empty());
    }
}
