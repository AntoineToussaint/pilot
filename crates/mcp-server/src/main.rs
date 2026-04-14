//! pilot-mcp-server: MCP server for Claude Code integration.
//!
//! Speaks JSON-RPC over stdio (standard MCP transport).
//! Communicates with the pilot TUI via a Unix domain socket for confirmations.
//!
//! Flow:
//! 1. Claude calls a tool (e.g. pilot_push)
//! 2. MCP server sends request to pilot TUI via Unix socket
//! 3. Pilot shows confirmation modal (y/n)
//! 4. MCP server receives approval, executes the action
//! 5. Result returned to Claude

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const SOCKET_PATH_ENV: &str = "PILOT_SOCKET";

fn default_socket_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    format!("{home}/.pilot/pilot.sock")
}

#[tokio::main]
async fn main() {
    let log_file = std::fs::File::create("/tmp/pilot-mcp.log").ok();
    if let Some(f) = log_file {
        tracing_subscriber::fmt()
            .with_writer(f)
            .with_ansi(false)
            .init();
    }

    tracing::info!("pilot-mcp-server starting (pid={})", std::process::id());

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Invalid JSON: {e}");
                continue;
            }
        };

        let response = handle_request(&request).await;
        if let Some(resp) = response {
            let resp_str = serde_json::to_string(&resp).unwrap();
            let _ = writeln!(stdout, "{resp_str}");
            let _ = stdout.flush();
        }
    }

    tracing::info!("pilot-mcp-server exiting");
}

async fn handle_request(req: &Value) -> Option<Value> {
    let method = req.get("method")?.as_str()?;
    let id = req.get("id").cloned();

    tracing::info!("MCP request: {method}");

    let result = match method {
        "initialize" => handle_initialize(),
        "tools/list" => handle_tools_list(),
        "tools/call" => handle_tool_call(req).await,
        "notifications/initialized" => return None,
        _ => json!({
            "error": { "code": -32601, "message": format!("Method not found: {method}") }
        }),
    };

    Some(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    }))
}

fn handle_initialize() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "pilot-mcp-server", "version": "0.1.0" }
    })
}

fn handle_tools_list() -> Value {
    json!({ "tools": [
        tool("pilot_push", "Push commits to remote. Pilot shows confirmation before executing. Use instead of `git push`.", json!({
            "type": "object",
            "properties": { "message": { "type": "string", "description": "What you're pushing" } },
            "required": ["message"]
        })),
        tool("pilot_reply", "Reply to a PR comment. Pilot shows the reply for approval. Use instead of `gh pr comment`.", json!({
            "type": "object",
            "properties": {
                "comment_id": { "type": "integer", "description": "Comment ID to reply to" },
                "body": { "type": "string", "description": "Reply text (markdown)" }
            },
            "required": ["body"]
        })),
        tool("pilot_merge", "Merge a PR. Pilot shows confirmation. Use instead of `gh pr merge`.", json!({
            "type": "object",
            "properties": { "method": { "type": "string", "enum": ["merge", "squash", "rebase"] } }
        })),
        tool("pilot_approve", "Approve a PR. Pilot shows confirmation.", json!({
            "type": "object",
            "properties": { "body": { "type": "string", "description": "Approval message" } }
        })),
        tool("pilot_resolve_thread", "Resolve a review thread after addressing feedback.", json!({
            "type": "object",
            "properties": { "thread_id": { "type": "string", "description": "GraphQL thread node ID" } },
            "required": ["thread_id"]
        })),
        tool("pilot_request_changes", "Request changes on a PR review.", json!({
            "type": "object",
            "properties": { "body": { "type": "string", "description": "Review body" } },
            "required": ["body"]
        })),
        tool("pilot_get_context", "Get the static context summary that pilot fed to Claude (markdown).", json!({
            "type": "object", "properties": {}
        })),
        tool("pilot_get_pr_state", "Fetch live PR state from GitHub (CI status, reviews, comments). Use this to check if your push passed CI before commenting/merging.", json!({
            "type": "object", "properties": {}
        })),
    ]})
}

fn tool(name: &str, desc: &str, schema: Value) -> Value {
    json!({ "name": name, "description": desc, "inputSchema": schema })
}

async fn handle_tool_call(req: &Value) -> Value {
    let params = req.get("params").unwrap_or(&Value::Null);
    let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = params.get("arguments").unwrap_or(&Value::Null);

    tracing::info!("Tool call: {tool_name}");

    let repo = std::env::var("PILOT_REPO").unwrap_or_default();
    let pr_number = std::env::var("PILOT_PR_NUMBER").unwrap_or_default();

    // Read-only tools execute immediately, no confirmation needed.
    if is_readonly(tool_name) {
        let result = execute_action(tool_name, arguments, &repo, &pr_number);
        return json!({ "content": [{ "type": "text", "text": result }] });
    }

    // Build confirmation request for write tools.
    let action = ConfirmRequest {
        tool: tool_name.to_string(),
        arguments: arguments.clone(),
        session_id: std::env::var("PILOT_SESSION").unwrap_or_default(),
        pr_number: pr_number.clone(),
        repo: repo.clone(),
        display: format_action(tool_name, arguments),
    };

    match request_confirmation(&action).await {
        Ok(response) => {
            if response.approved {
                let result = execute_action(tool_name, arguments, &action.repo, &action.pr_number);
                json!({ "content": [{ "type": "text", "text": result }] })
            } else {
                let msg = response.message.unwrap_or_default();
                json!({ "content": [{ "type": "text", "text": format!("Rejected by user. {msg}") }] })
            }
        }
        Err(e) => {
            tracing::error!("Confirmation failed: {e}");
            // Fallback: execute with warning if socket unavailable.
            json!({
                "content": [{ "type": "text", "text": format!("Warning: pilot TUI not connected ({e}). Action not executed. Run pilot to enable confirmations.") }],
                "isError": true
            })
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ConfirmRequest {
    tool: String,
    arguments: Value,
    session_id: String,
    pr_number: String,
    repo: String,
    display: String,
}

#[derive(Serialize, Deserialize)]
struct ConfirmResponse {
    approved: bool,
    message: Option<String>,
}

/// Send confirmation request to pilot TUI via Unix socket.
async fn request_confirmation(req: &ConfirmRequest) -> Result<ConfirmResponse, String> {
    let socket_path =
        std::env::var(SOCKET_PATH_ENV).unwrap_or_else(|_| default_socket_path());

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .map_err(|e| format!("Cannot connect to {socket_path}: {e}"))?;

    // Send the request.
    let payload = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    let len = (payload.len() as u32).to_be_bytes();
    stream.write_all(&len).await.map_err(|e| e.to_string())?;
    stream.write_all(&payload).await.map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())?;

    // Wait for response (timeout 120s).
    let mut len_buf = [0u8; 4];
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        stream.read_exact(&mut len_buf),
    )
    .await
    .map_err(|_| "Timeout waiting for user confirmation".to_string())?
    .map_err(|e| e.to_string())?;

    let resp_len = u32::from_be_bytes(len_buf) as usize;
    let mut resp_buf = vec![0u8; resp_len];
    stream
        .read_exact(&mut resp_buf)
        .await
        .map_err(|e| e.to_string())?;

    serde_json::from_slice(&resp_buf).map_err(|e| e.to_string())
}

fn format_action(tool: &str, args: &Value) -> String {
    match tool {
        "pilot_push" => {
            let msg = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
            format!("Push: {msg}")
        }
        "pilot_reply" => {
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let preview: String = body.chars().take(80).collect();
            format!("Reply: \"{preview}\"")
        }
        "pilot_merge" => {
            let method = args.get("method").and_then(|v| v.as_str()).unwrap_or("squash");
            format!("Merge PR ({method})")
        }
        "pilot_approve" => "Approve PR".to_string(),
        "pilot_resolve_thread" => "Resolve review thread".to_string(),
        "pilot_request_changes" => {
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let preview: String = body.chars().take(60).collect();
            format!("Request changes: \"{preview}\"")
        }
        // Read-only tools don't need confirmation.
        "pilot_get_pr_state" | "pilot_get_context" => format!("[read-only] {tool}"),
        _ => format!("{tool}"),
    }
}

/// Tools that don't need user confirmation (read-only data access).
fn is_readonly(tool: &str) -> bool {
    matches!(tool, "pilot_get_pr_state" | "pilot_get_context")
}

fn execute_action(tool: &str, args: &Value, repo: &str, pr_number: &str) -> String {
    match tool {
        "pilot_push" => {
            let output = std::process::Command::new("git").args(["push"]).output();
            match output {
                Ok(o) if o.status.success() => "Pushed successfully.".into(),
                Ok(o) => format!("Push failed: {}", String::from_utf8_lossy(&o.stderr)),
                Err(e) => format!("Push error: {e}"),
            }
        }
        "pilot_reply" => {
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let output = std::process::Command::new("gh")
                .args(["pr", "comment", pr_number, "--body", body, "--repo", repo])
                .output();
            match output {
                Ok(o) if o.status.success() => "Comment posted.".into(),
                Ok(o) => format!("Comment failed: {}", String::from_utf8_lossy(&o.stderr)),
                Err(e) => format!("Comment error: {e}"),
            }
        }
        "pilot_merge" => {
            let method = args.get("method").and_then(|v| v.as_str()).unwrap_or("squash");
            let output = std::process::Command::new("gh")
                .args(["pr", "merge", pr_number, &format!("--{method}"), "--repo", repo])
                .output();
            match output {
                Ok(o) if o.status.success() => format!("Merged ({method})."),
                Ok(o) => format!("Merge failed: {}", String::from_utf8_lossy(&o.stderr)),
                Err(e) => format!("Merge error: {e}"),
            }
        }
        "pilot_approve" => {
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("LGTM");
            let output = std::process::Command::new("gh")
                .args(["pr", "review", pr_number, "--approve", "--body", body, "--repo", repo])
                .output();
            match output {
                Ok(o) if o.status.success() => "Approved.".into(),
                Ok(o) => format!("Approve failed: {}", String::from_utf8_lossy(&o.stderr)),
                Err(e) => format!("Approve error: {e}"),
            }
        }
        "pilot_get_context" => {
            let home = std::env::var("HOME").unwrap_or_default();
            let session_id = std::env::var("PILOT_SESSION").unwrap_or_default();
            let safe_id = session_id.replace(':', "_").replace('/', "_");
            let context_file = format!("{home}/.pilot/context/{safe_id}.md");
            std::fs::read_to_string(&context_file)
                .unwrap_or_else(|_| format!("PR: {repo}#{pr_number} — no detailed context available."))
        }
        "pilot_get_pr_state" => {
            // Live fetch via gh CLI — JSON with current state.
            let output = std::process::Command::new("gh")
                .args([
                    "pr", "view", pr_number,
                    "--repo", repo,
                    "--json", "number,title,state,isDraft,mergeable,reviewDecision,statusCheckRollup,reviews,comments",
                ])
                .output();
            match output {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
                Ok(o) => format!("{{\"error\": \"{}\"}}", String::from_utf8_lossy(&o.stderr).trim().replace('"', "'")),
                Err(e) => format!("{{\"error\": \"{e}\"}}"),
            }
        }
        "pilot_resolve_thread" => {
            let thread_id = args.get("thread_id").and_then(|v| v.as_str()).unwrap_or("");
            if thread_id.is_empty() {
                return "Error: thread_id required".to_string();
            }
            if !thread_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '=') {
                return "Error: invalid thread_id format".to_string();
            }
            // GraphQL mutation to resolve thread.
            let query = format!(
                r#"mutation {{ resolveReviewThread(input: {{threadId: "{thread_id}"}}) {{ thread {{ isResolved }} }} }}"#
            );
            let output = std::process::Command::new("gh")
                .args(["api", "graphql", "-f", &format!("query={query}")])
                .output();
            match output {
                Ok(o) if o.status.success() => "Thread resolved.".into(),
                Ok(o) => format!("Resolve failed: {}", String::from_utf8_lossy(&o.stderr)),
                Err(e) => format!("Resolve error: {e}"),
            }
        }
        "pilot_request_changes" => {
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let output = std::process::Command::new("gh")
                .args([
                    "pr", "review", pr_number,
                    "--request-changes", "--body", body,
                    "--repo", repo,
                ])
                .output();
            match output {
                Ok(o) if o.status.success() => "Changes requested.".into(),
                Ok(o) => format!("Request changes failed: {}", String::from_utf8_lossy(&o.stderr)),
                Err(e) => format!("Request changes error: {e}"),
            }
        }
        _ => format!("Unknown tool: {tool}"),
    }
}
