//! MCP stdio protocol e2e: `br serve` speaks JSON-RPC over newline-delimited
//! JSON exactly as an MCP client would drive it.
//!
//! The shutdown test proves `serve` starts and stops cleanly; this one proves
//! the advertised surface works over the stateless 2026-07-28 era (every
//! frame carries the protocol version in `_meta`): `server/discover`, list
//! tools/resources/prompts, call a read tool and the mutating tools, read a
//! resource, and then check outside MCP that the mutations reached the
//! workspace (CLI `show`, the audit actor, and the auto-flushed `issues.jsonl`).
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
const REPLY_TIMEOUT: Duration = Duration::from_secs(90);
/// The MCP protocol era `br serve` negotiates on stdio; fastmcp requires every
/// frame to carry it under `params._meta`.
const PROTOCOL_VERSION: &str = "2026-07-28";
const PROTOCOL_VERSION_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";

/// Attach the protocol-era metadata fastmcp expects on every request and
/// notification.
fn with_era(mut params: Value) -> Value {
    if !params.is_object() {
        params = json!({});
    }
    params["_meta"][PROTOCOL_VERSION_META_KEY] = json!(PROTOCOL_VERSION);
    params["_meta"]["io.modelcontextprotocol/clientCapabilities"] = json!({});
    params
}

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
    // Pin discovery to this workspace: on hosts whose TMPDIR sits inside a
    // checkout (RCH workers), `br init` would otherwise walk up to that
    // repository's own tracker and refuse.
    command.env("BEADS_DIR", root.join(".beads"));
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
            // Debug logging goes to stderr (never stdout), so it does not
            // disturb the JSON-RPC stream and is shown when a step fails.
            .env("RUST_LOG", "beads_rust=debug")
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

    /// Send a request and wait for the response with the same id, skipping
    /// notifications and anything that is not JSON.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": with_era(params)}),
        );
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut other_frames: Vec<String> = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(line) = self.lines.recv_timeout(remaining) else {
                // Kill the server so its stderr reaches EOF and can be shown.
                let _ = self.child.kill();
                let stderr = self
                    .stderr
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap_or_default();
                panic!(
                    "no reply to {method} (id {id}) within {REPLY_TIMEOUT:?}\nother frames seen:\n{}\nserve stderr:\n{stderr}",
                    other_frames.join("\n")
                );
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                other_frames.push(line);
                continue;
            };
            if message.get("id") != Some(&json!(id)) {
                other_frames.push(line);
                continue;
            }
            if let Some(error) = message.get("error") {
                let _ = self.child.kill();
                let stderr = self
                    .stderr
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap_or_default();
                panic!(
                    "{method} failed: {error}\nother frames seen:\n{}\nserve stderr:\n{stderr}",
                    other_frames.join("\n")
                );
            }
            return message["result"].clone();
        }
    }

    /// Call a tool and return its JSON payload (structured content when the
    /// server provides it, otherwise the parsed text content).
    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let mut params = serde_json::Map::new();
        params.insert("name".to_string(), Value::String(name.to_string()));
        params.insert("arguments".to_string(), arguments);
        let result = self.request("tools/call", Value::Object(params));
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

/// The first record of a CLI `--json` payload (some commands wrap a single
/// record in an array).
fn first_record(payload: Value) -> Value {
    payload
        .as_array()
        .and_then(|entries| entries.first())
        .cloned()
        .unwrap_or(payload)
}

/// Discovery over the stateless 2026-07-28 era: no `initialize` handshake,
/// every frame carries the protocol version in `_meta`, `server/discover` is
/// the discovery request, and every documented tool, resource, and prompt is
/// listed.
fn assert_discovery_surface(client: &mut McpClient) {
    let discovered = client.request("server/discover", json!({}));
    assert!(
        discovered.is_object(),
        "server/discover result: {discovered}"
    );
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
}

/// In-place acceptance checklist edits over MCP (GitHub #477): append, tick
/// by text, and read the structured items back through `show_issue` and the
/// CLI.
fn exercise_acceptance_over_mcp(client: &mut McpClient, root: &Path, id: &str) {
    let appended = client.call_tool(
        "update_issue",
        json!({"id": id, "add_acceptance": ["first criterion", "second criterion"]}),
    );
    assert_eq!(
        appended["acceptance_criteria"]["total"], 2,
        "add_acceptance: {appended}"
    );
    let ticked = client.call_tool(
        "update_issue",
        json!({"id": id, "check_acceptance": ["second"]}),
    );
    assert_eq!(
        ticked["acceptance_criteria"]["remaining"], 1,
        "check_acceptance: {ticked}"
    );
    let shown = client.call_tool("show_issue", json!({"id": id}));
    assert_eq!(
        shown["acceptance_items"][1]["checked"], true,
        "show_issue: {shown}"
    );
    let record = first_record(cli_json(root, &["show", id]));
    assert_eq!(
        record["acceptance_items"][0]["checked"], false,
        "CLI show: {record}"
    );
    assert_eq!(
        record["acceptance_items"][1]["checked"], true,
        "CLI show: {record}"
    );
    assert_eq!(
        record["acceptance_criteria"], "- [ ] first criterion\n- [x] second criterion",
        "CLI show: {record}"
    );
}

/// Outside MCP: the CLI, the audit actor, and the auto-flushed JSONL all
/// reflect the mutations made over the protocol.
fn assert_mutations_reached_workspace(root: &Path, new_id: &str) {
    let record = first_record(cli_json(root, &["show", new_id]));
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
        jsonl.contains(new_id),
        "issues.jsonl should carry the MCP-created issue after auto-flush"
    );
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
    assert_discovery_surface(&mut client);

    // While serve is up, the CLI must still read the workspace promptly; if
    // this blocks, serve holds a workspace lock it should not.
    let cli_probe_started = Instant::now();
    let cli_list = cli_json(root, &["list"]);
    let cli_probe = cli_probe_started.elapsed();
    assert!(
        cli_probe < Duration::from_secs(10),
        "br list --json took {cli_probe:?} while serve was running: {cli_list}"
    );

    // A read tool sees the issue created by the CLI before serve started.
    let tool_started = Instant::now();
    let listed = client.call_tool("list_issues", json!({}));
    eprintln!(
        "[mcp] list_issues took {:?} (cli probe {cli_probe:?})",
        tool_started.elapsed()
    );
    let mut listed_ids = BTreeSet::new();
    ids_in(&listed, &mut listed_ids);
    assert!(listed_ids.contains(&seeded_id), "list_issues: {listed}");

    // A CLI write must also succeed while serve is idle; if it does and the
    // MCP create below still blocks, the block is inside serve's own
    // mutation path rather than a workspace lock held elsewhere.
    let cli_write_started = Instant::now();
    let cli_created = cli_json(root, &["create", "CLI write while serve is idle"]);
    eprintln!(
        "[mcp] cli create took {:?}: {}",
        cli_write_started.elapsed(),
        first_id(&cli_created)
    );

    // Mutating tools: create, label, close.
    let create_started = Instant::now();
    let created = client.call_tool(
        "create_issue",
        json!({"title": "Created over MCP", "type": "task", "priority": "1"}),
    );
    eprintln!("[mcp] create_issue took {:?}", create_started.elapsed());
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
    exercise_acceptance_over_mcp(&mut client, root, &new_id);

    client.call_tool(
        "close_issue",
        json!({"id": new_id, "reason": "closed over MCP"}),
    );

    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "serve exited with {status} after stdin close; stderr:\n{stderr}"
    );

    assert_mutations_reached_workspace(root, &new_id);
}
