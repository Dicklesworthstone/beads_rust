//! MCP stdio protocol e2e: `br serve` speaks JSON-RPC over newline-delimited
//! JSON exactly as an MCP client would drive it.
//!
//! The shutdown test proves `serve` starts and stops cleanly; this one proves
//! the advertised surface works: initialize, list tools/resources/prompts,
//! call a read tool and the mutating tools, read a resource, and then check
//! outside MCP that the mutations reached the workspace (CLI `show`, the
//! audit actor, and the auto-flushed `issues.jsonl`).
//!
//! Only built with `--features mcp` (`scripts/test-shard.sh` and CI pass
//! `--all-features`).
#![cfg(all(unix, feature = "mcp"))]

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const ACTOR: &str = "mcp-protocol-test";
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

fn should_clear_inherited_br_env(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    key.starts_with("BD_")
        || key.starts_with("BEADS_")
        || matches!(
            key.as_ref(),
            "BR_DISABLE_READ_ONLY_FAST_OPEN"
                | "BR_OUTPUT_FORMAT"
                | "TOON_DEFAULT_FORMAT"
                | "TOON_STATS"
        )
}

fn br_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_br"));
    command.current_dir(root);
    for (key, _) in std::env::vars_os() {
        if should_clear_inherited_br_env(&key) {
            command.env_remove(key);
        }
    }
    command.env("HOME", root);
    command.env("NO_COLOR", "1");
    command.env("RUST_LOG", "error");
    command
}

/// Run a CLI command with `--json` and parse its output.
fn cli_json(root: &Path, args: &[&str]) -> Value {
    let output = br_command(root)
        .args(args)
        .arg("--json")
        .output()
        .expect("run br");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "br {} failed with {}\nstdout:\n{stdout}\nstderr:\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let start = stdout
        .find(['{', '['])
        .unwrap_or_else(|| panic!("br {} printed no JSON: {stdout}", args.join(" ")));
    serde_json::from_str(stdout[start..].trim())
        .unwrap_or_else(|err| panic!("br {} printed bad JSON: {err}\n{stdout}", args.join(" ")))
}

/// Every `id` string reachable from the value.
fn ids_in(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("id") {
                out.insert(id.clone());
            }
            map.values().for_each(|child| ids_in(child, out));
        }
        Value::Array(items) => items.iter().for_each(|item| ids_in(item, out)),
        _ => {}
    }
}

fn first_id(value: &Value) -> String {
    let mut ids = BTreeSet::new();
    ids_in(value, &mut ids);
    ids.into_iter()
        .next()
        .unwrap_or_else(|| panic!("no id in {value}"))
}

/// Whether any string anywhere in the value contains `needle`.
fn contains_text(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(text) => text.contains(needle),
        Value::Object(map) => map.values().any(|child| contains_text(child, needle)),
        Value::Array(items) => items.iter().any(|item| contains_text(item, needle)),
        _ => false,
    }
}

/// A minimal MCP client over the child's stdio pipes.
struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Receiver<String>,
    next_id: u64,
}

impl McpClient {
    fn spawn(root: &Path) -> Self {
        let mut child = br_command(root)
            .args(["serve", "--actor", ACTOR])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn br serve");
        let stdin = child.stdin.take().expect("serve stdin");
        let stdout = child.stdout.take().expect("serve stdout");
        let (line_tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut stderr_pipe = child.stderr.take().expect("serve stderr");
        let (stderr_tx, stderr) = mpsc::channel();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr_pipe.read_to_string(&mut text);
            let _ = stderr_tx.send(text);
        });
        Self {
            child,
            stdin: Some(stdin),
            lines,
            stderr,
            next_id: 1,
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("serve stdin still open");
        writeln!(stdin, "{message}").expect("write to serve stdin");
        stdin.flush().expect("flush serve stdin");
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Send a request and wait for the response with the same id, skipping
    /// notifications and anything that is not JSON.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(remaining).unwrap_or_else(|_| {
                panic!(
                    "no reply to {method} within {REPLY_TIMEOUT:?}; serve stderr:\n{}",
                    self.stderr.try_recv().unwrap_or_default()
                )
            });
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = message.get("error") {
                panic!("{method} failed: {error}");
            }
            return message["result"].clone();
        }
    }

    /// Call a tool and return its JSON payload (structured content when the
    /// server provides it, otherwise the parsed text content).
    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert_ne!(
            result.get("isError"),
            Some(&json!(true)),
            "{name} returned an error result: {result}"
        );
        if let Some(structured) = result.get("structuredContent") {
            return structured.clone();
        }
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} returned no text content: {result}"));
        serde_json::from_str(text).unwrap_or_else(|_| json!({ "text": text }))
    }

    /// Close stdin (EOF ends the stdio transport) and wait for the server.
    fn finish(mut self) -> (ExitStatus, String) {
        drop(self.stdin.take());
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll serve") {
                break status;
            }
            if start.elapsed() > Duration::from_secs(10) {
                let _ = Command::new("kill")
                    .args(["-TERM", &self.child.id().to_string()])
                    .status();
                break self.child.wait().expect("wait serve after SIGTERM");
            }
            thread::sleep(Duration::from_millis(20));
        };
        let stderr = self
            .stderr
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default();
        (status, stderr)
    }
}

#[test]
fn serve_speaks_mcp_over_stdio_and_mutations_reach_the_workspace() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();
    let init = br_command(root)
        .args(["init", "--prefix", "mcp"])
        .output()
        .expect("run br init");
    assert!(
        init.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let seeded_id = first_id(&cli_json(root, &["create", "Seeded before serve"]));

    let mut client = McpClient::spawn(root);

    // Handshake.
    let initialized = client.request(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "beads_rust e2e", "version": "0"}
        }),
    );
    assert!(
        initialized["protocolVersion"].is_string(),
        "initialize result: {initialized}"
    );
    assert!(
        initialized["capabilities"]["tools"].is_object(),
        "server must advertise tools: {initialized}"
    );
    client.notify("notifications/initialized", json!({}));

    // Discovery: every documented tool, resource, and prompt is listed.
    let tools = client.request("tools/list", json!({}));
    let tool_names: BTreeSet<&str> = tools["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for expected in [
        "list_issues",
        "show_issue",
        "create_issue",
        "update_issue",
        "close_issue",
        "manage_dependencies",
        "project_overview",
    ] {
        assert!(
            tool_names.contains(expected),
            "missing tool {expected}: {tool_names:?}"
        );
    }
    let resources = client.request("resources/list", json!({}));
    let uris: BTreeSet<&str> = resources["resources"]
        .as_array()
        .expect("resources array")
        .iter()
        .filter_map(|resource| resource["uri"].as_str())
        .collect();
    for expected in ["beads://project/info", "beads://schema", "beads://labels"] {
        assert!(
            uris.contains(expected),
            "missing resource {expected}: {uris:?}"
        );
    }
    let prompts = client.request("prompts/list", json!({}));
    let prompt_names: BTreeSet<&str> = prompts["prompts"]
        .as_array()
        .expect("prompts array")
        .iter()
        .filter_map(|prompt| prompt["name"].as_str())
        .collect();
    for expected in [
        "triage",
        "status_report",
        "plan_next_work",
        "polish_backlog",
    ] {
        assert!(
            prompt_names.contains(expected),
            "missing prompt {expected}: {prompt_names:?}"
        );
    }

    // A read tool sees the issue created by the CLI before serve started.
    let listed = client.call_tool("list_issues", json!({}));
    let mut listed_ids = BTreeSet::new();
    ids_in(&listed, &mut listed_ids);
    assert!(listed_ids.contains(&seeded_id), "list_issues: {listed}");

    // Mutating tools: create, label, close.
    let created = client.call_tool(
        "create_issue",
        json!({"title": "Created over MCP", "type": "task", "priority": "1"}),
    );
    let new_id = first_id(&created);
    assert!(
        new_id.starts_with("mcp-"),
        "created id {new_id} lacks the workspace prefix"
    );
    let shown = client.call_tool("show_issue", json!({"id": new_id}));
    assert!(
        contains_text(&shown, "Created over MCP"),
        "show_issue: {shown}"
    );
    client.call_tool(
        "update_issue",
        json!({"id": new_id, "labels_add": ["over-mcp"]}),
    );
    let overview = client.call_tool("project_overview", json!({}));
    assert!(!overview.is_null(), "project_overview returned nothing");
    let resource = client.request(
        "resources/read",
        json!({"uri": format!("beads://issues/{new_id}")}),
    );
    assert!(
        contains_text(&resource, &new_id),
        "resources/read beads://issues/{new_id}: {resource}"
    );
    client.call_tool(
        "close_issue",
        json!({"id": new_id, "reason": "closed over MCP"}),
    );

    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "serve exited with {status} after stdin close; stderr:\n{stderr}"
    );

    // Outside MCP: the CLI, the audit actor, and the auto-flushed JSONL all
    // reflect the mutations.
    let shown_cli = cli_json(root, &["show", &new_id]);
    let record = shown_cli
        .as_array()
        .and_then(|entries| entries.first())
        .cloned()
        .unwrap_or(shown_cli);
    assert_eq!(record["title"], "Created over MCP");
    assert_eq!(record["status"], "closed", "record: {record}");
    assert!(
        record["labels"]
            .as_array()
            .is_some_and(|labels| labels.iter().any(|label| label == "over-mcp")),
        "labels: {}",
        record["labels"]
    );
    assert_eq!(record["created_by"], ACTOR, "record: {record}");
    let jsonl = std::fs::read_to_string(root.join(".beads").join("issues.jsonl"))
        .expect("issues.jsonl after serve");
    assert!(
        jsonl.contains(&new_id),
        "issues.jsonl should carry the MCP-created issue after auto-flush"
    );
}
