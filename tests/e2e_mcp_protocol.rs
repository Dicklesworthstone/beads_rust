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
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::Barrier;
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
const FIXED_RESOURCE_URIS: &[&str] = &[
    "beads://project/info",
    "beads://schema",
    "beads://labels",
    "beads://issues/ready",
    "beads://issues/blocked",
    "beads://issues/in_progress",
    "beads://coordination/status",
    "beads://events/recent",
    "beads://issues/deferred",
    "beads://graph/health",
    "beads://issues/bottlenecks",
];

struct ProtocolWorkspace(Option<TempDir>);

impl ProtocolWorkspace {
    fn new() -> std::io::Result<Self> {
        TempDir::new().map(|temp| Self(Some(temp)))
    }

    fn path(&self) -> &Path {
        self.0.as_ref().expect("live workspace").path()
    }
}

impl Drop for ProtocolWorkspace {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(temp) = self.0.take()
        {
            eprintln!("MCP failure workspace retained: {}", temp.keep().display());
        }
    }
}

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
                | "BR_SESSION"
                | "BR_AGENT_NAME"
                | "BR_HARNESS"
                | "BR_MODEL"
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
    eprintln!(
        "{}",
        json!({"kind": "cli", "workspace": root, "binary": env!("CARGO_BIN_EXE_br"),
            "args": args, "json_flag": true, "exit": output.status.code(),
            "stdout": stdout, "stderr": String::from_utf8_lossy(&output.stderr)})
    );
    assert!(
        output.status.success(),
        "br {} failed with {}\nstdout:\n{stdout}\nstderr:\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(stdout.trim())
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
    root: PathBuf,
    trace: Vec<Value>,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Receiver<String>,
    next_id: u64,
}

impl McpClient {
    fn spawn(root: &Path) -> Self {
        Self::spawn_with_session(root, None)
    }

    fn spawn_with_session(root: &Path, session: Option<&str>) -> Self {
        let mut child = br_command(root)
            .args(["serve", "--actor", ACTOR])
            // Debug logging goes to stderr (never stdout), so it does not
            // disturb the JSON-RPC stream and is shown when a step fails.
            .env("RUST_LOG", "beads_rust=debug")
            .env("BR_MCP_READ_SNAPSHOT", "1")
            .env("BR_SESSION", session.unwrap_or_default())
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
            root: root.to_path_buf(),
            trace: Vec::new(),
            child,
            stdin: Some(stdin),
            lines,
            stderr,
            next_id: 1,
        }
    }

    fn send(&mut self, message: &Value) {
        eprintln!(
            "{}",
            json!({"kind": "mcp_sent", "workspace": self.root, "pid": self.child.id(),
                "at": chrono::Utc::now(), "frame": message})
        );
        self.trace.push(json!({"sent": message}));
        let stdin = self.stdin.as_mut().expect("serve stdin still open");
        writeln!(stdin, "{message}").expect("write to serve stdin");
        stdin.flush().expect("flush serve stdin");
    }

    /// Send a request and retain the complete success or error response.
    /// Notifications may precede it, but stdout must remain valid JSON-RPC.
    fn request_frame(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": with_era(params)}),
        );
        self.receive_response(id, method)
    }

    fn receive_response(&mut self, id: u64, method: &str) -> Value {
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
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|err| panic!("non-JSON stdout during {method}: {err}: {line}"));
            eprintln!(
                "{}",
                json!({"kind": "mcp_received", "workspace": self.root, "pid": self.child.id(),
                    "at": chrono::Utc::now(), "frame": message})
            );
            self.trace.push(json!({"received": message}));
            assert_eq!(message["jsonrpc"], "2.0", "invalid frame: {message}");
            if message.get("id") != Some(&json!(id)) {
                other_frames.push(line);
                continue;
            }
            assert_ne!(
                message.get("result").is_some(),
                message.get("error").is_some()
            );
            return message;
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let response = self.request_frame(method, params);
        assert!(
            response.get("error").is_none(),
            "{method} failed: {response}"
        );
        response["result"].clone()
    }

    fn read_resource(&mut self, uri: &str) -> Value {
        let response = self.request("resources/read", json!({"uri": uri}));
        let contents = response["contents"].as_array().expect("resource contents");
        assert_eq!(contents.len(), 1, "{uri}: {response}");
        assert_eq!(contents[0]["uri"], uri);
        assert_eq!(contents[0]["mimeType"], "application/json");
        let text = contents[0]["text"].as_str().expect("JSON resource text");
        serde_json::from_str(text).unwrap_or_else(|err| panic!("{uri}: {err}: {text}"))
    }

    fn tool_error(&mut self, name: &str, arguments: Value) -> Value {
        let mut params = serde_json::Map::new();
        params.insert("name".to_string(), Value::String(name.to_string()));
        params.insert("arguments".to_string(), arguments);
        let response = self.request_frame("tools/call", Value::Object(params));
        assert!(
            response.get("error").is_none(),
            "expected a structured tool refusal, got a protocol error: {response}"
        );
        assert_eq!(
            response["result"]["isError"], true,
            "expected {name} refusal: {response}"
        );
        let error = &response["result"]["structuredContent"];
        assert!(error.is_object(), "missing refusal detail: {response}");
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("error text");
        assert_eq!(
            serde_json::from_str::<Value>(text).expect("error JSON"),
            *error
        );
        error.clone()
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
        serde_json::from_str(text)
            .unwrap_or_else(|err| panic!("{name} returned invalid JSON: {err}: {text}"))
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
        eprintln!(
            "{}",
            json!({"kind": "mcp_exit", "workspace": self.root, "pid": self.child.id(),
                "binary": env!("CARGO_BIN_EXE_br"), "source": option_env!("VERGEN_GIT_SHA"),
                "engine": env!("BR_FSQLITE_VERSION"), "mcp": cfg!(feature = "mcp"),
                "exit": status.code(), "stderr": stderr})
        );
        (status, stderr)
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Ok(stderr) = self.stderr.recv_timeout(Duration::from_secs(2)) {
                eprintln!("serve stderr after failed assertion:\n{stderr}");
                let artifact = json!({"engine": env!("BR_FSQLITE_VERSION"), "source": option_env!("VERGEN_GIT_SHA"), "mcp": cfg!(feature = "mcp"), "trace": self.trace, "stderr": stderr});
                if let Err(error) =
                    std::fs::write(self.root.join("mcp-failure.json"), artifact.to_string())
                {
                    eprintln!("Could not preserve MCP trace: {error}");
                }
            }
        }
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
    for expected in FIXED_RESOURCE_URIS {
        assert!(
            uris.contains(expected),
            "missing resource {expected}: {uris:?}"
        );
    }
    assert_eq!(
        uris.len(),
        FIXED_RESOURCE_URIS.len(),
        "resources: {resources}"
    );
    let templates = client.request("resources/templates/list", json!({}));
    assert_eq!(
        templates["resourceTemplates"][0]["uriTemplate"],
        "beads://issue/{id}"
    );
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
    let temp = ProtocolWorkspace::new().expect("tempdir");
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
        json!({"uri": format!("beads://issue/{new_id}")}),
    );
    assert!(
        contains_text(&resource, &new_id),
        "resources/read beads://issue/{new_id}: {resource}"
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

#[test]
fn mcp_fixed_resources_and_issue_template_are_all_reachable() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "route"]);
    let mut client = McpClient::spawn(root);
    assert_discovery_surface(&mut client);
    for uri in FIXED_RESOURCE_URIS {
        let value = client.read_resource(uri);
        assert!(value.is_object(), "empty {uri}: {value}");
        if uri.starts_with("beads://issues/") && *uri != "beads://issues/bottlenecks" {
            assert_eq!(value["count"], 0, "empty {uri}: {value}");
            assert_eq!(value["issues"], json!([]));
        }
    }
    let ready = first_id(&cli_json(root, &["create", "Ready prerequisite"]));
    let blocked = first_id(&cli_json(root, &["create", "Blocked dependent"]));
    cli_json(root, &["dep", "add", &blocked, &ready]);
    let progress = first_id(&cli_json(root, &["create", "Working issue"]));
    cli_json(root, &["update", &progress, "--status", "in_progress"]);
    let deferred = first_id(&cli_json(root, &["create", "Deferred issue"]));
    cli_json(root, &["update", &deferred, "--status", "deferred"]);
    cli_json(root, &["label", "add", &ready, "route-proof"]);
    for uri in FIXED_RESOURCE_URIS {
        let value = client.read_resource(uri);
        assert!(value.is_object(), "populated {uri}: {value}");
    }
    for (uri, expected) in [
        ("beads://issues/ready", &ready),
        ("beads://issues/blocked", &blocked),
        ("beads://issues/in_progress", &progress),
        ("beads://issues/deferred", &deferred),
        ("beads://issues/bottlenecks", &ready),
    ] {
        let value = client.read_resource(uri);
        assert_eq!(value["count"], 1, "{uri}: {value}");
        assert_eq!(value["issues"][0]["id"], *expected, "{uri}: {value}");
    }
    let health = client.read_resource("beads://graph/health");
    assert_eq!(health["dependency_edge_count"], 1, "{health}");
    assert_eq!(health["max_chain_depth"], 1);
    assert_eq!(health["cycle_detected"], false);
    let project = client.read_resource("beads://project/info");
    assert_eq!(project["issue_prefix"], "route");
    assert_eq!(project["actor"], ACTOR);
    let schema = client.read_resource("beads://schema");
    assert!(
        schema["statuses"]["values"]
            .as_array()
            .expect("statuses")
            .contains(&json!("open"))
    );
    let labels = client.read_resource("beads://labels");
    assert!(contains_text(&labels, "route-proof"), "{labels}");
    let events = client.read_resource("beads://events/recent");
    assert!(
        events["events"]
            .as_array()
            .expect("events")
            .iter()
            .any(|event| event["issue_id"] == ready)
    );
    let coordination = client.read_resource("beads://coordination/status");
    assert_eq!(
        coordination["schema_version"], "br.coordination.v1",
        "{coordination}"
    );
    let issue = client.read_resource(&format!("beads://issue/{ready}"));
    assert!(contains_text(&issue, "Ready prerequisite"), "{issue}");
    let missing = client.request_frame(
        "resources/read",
        json!({"uri": "beads://issue/route-missing"}),
    );
    assert_eq!(
        missing["error"]["data"]["error_type"], "ISSUE_NOT_FOUND",
        "{missing}"
    );
    // An error must not poison subsequent transport requests.
    assert_eq!(client.read_resource("beads://issues/ready")["count"], 1);
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
    assert!(!stderr.contains("Failed to register resource"), "{stderr}");
}

fn assert_policy_refusal_unchanged(
    client: &mut McpClient,
    root: &Path,
    tool: &str,
    arguments: Value,
    expected_error: &str,
) -> Value {
    let before = cli_json(root, &["list", "--all"]);
    let events = client.read_resource("beads://events/recent");
    let jsonl = std::fs::read(root.join(".beads/issues.jsonl")).expect("JSONL before");
    eprintln!(
        "{}",
        json!({"kind": "jsonl_before_refusal", "workspace": root,
        "jsonl": String::from_utf8_lossy(&jsonl)})
    );
    let bookkeeping = policy_bookkeeping(root);
    let error = client.tool_error(tool, arguments);
    assert_eq!(error["data"]["error_type"], expected_error, "{error}");
    assert_eq!(cli_json(root, &["list", "--all"]), before);
    assert_eq!(client.read_resource("beads://events/recent"), events);
    let jsonl_after = std::fs::read(root.join(".beads/issues.jsonl")).expect("JSONL after");
    eprintln!(
        "{}",
        json!({"kind": "jsonl_after_refusal", "workspace": root,
        "jsonl": String::from_utf8_lossy(&jsonl_after)})
    );
    assert_eq!(jsonl_after, jsonl);
    assert_eq!(policy_bookkeeping(root), bookkeeping);
    error
}

fn assert_cli_policy_refusal_unchanged(root: &Path, args: &[&str], expected: &str) {
    let before = policy_bookkeeping(root);
    let jsonl = std::fs::read(root.join(".beads/issues.jsonl")).expect("JSONL before CLI refusal");
    eprintln!(
        "{}",
        json!({"kind": "jsonl_before_cli_refusal", "workspace": root,
        "jsonl": String::from_utf8_lossy(&jsonl)})
    );
    let output = br_command(root)
        .args(args)
        .arg("--json")
        .output()
        .expect("CLI refusal");
    eprintln!(
        "{}",
        json!({"kind": "cli_refusal", "workspace": root,
        "args": args, "json_flag": true, "exit": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr)})
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stdout).expect("whole CLI refusal JSON");
    assert_eq!(error["error"]["code"], expected, "{error}");
    assert_eq!(policy_bookkeeping(root), before);
    let jsonl_after =
        std::fs::read(root.join(".beads/issues.jsonl")).expect("JSONL after CLI refusal");
    eprintln!(
        "{}",
        json!({"kind": "jsonl_after_cli_refusal", "workspace": root,
        "jsonl": String::from_utf8_lossy(&jsonl_after)})
    );
    assert_eq!(jsonl_after, jsonl);
}

fn read_only_db(root: &Path) -> beads_rust::franken_sync::Connection {
    beads_rust::franken_sync::compat::open_with_flags(
        &root.join(".beads/beads.db").to_string_lossy(),
        beads_rust::franken_sync::compat::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read-only database observer")
}

fn policy_bookkeeping(root: &Path) -> Value {
    let connection = read_only_db(root);
    // Read the stored rows directly without opening a mutable storage facade.
    // Debug preserves the SQL value types and every column in this comparison.
    let result = json!({
        "issues": format!("{:?}", connection.query("SELECT * FROM issues ORDER BY id").expect("all issue fields")),
        "comments": format!("{:?}", connection.query("SELECT * FROM comments ORDER BY id").expect("all comments")),
        "events": format!("{:?}", connection.query("SELECT * FROM events ORDER BY id").expect("events including status revisions")),
        "gates": format!("{:?}", connection.query("SELECT * FROM gate_result_history ORDER BY id").expect("gate history")),
        "legacy_gates": format!("{:?}", connection.query("SELECT * FROM gate_results ORDER BY issue_id, gate, provider").expect("legacy gates")),
        "dirty": format!("{:?}", connection.query("SELECT * FROM dirty_issues ORDER BY issue_id").expect("dirty metadata")),
        "occupancy": format!("{:?}", connection.query("SELECT * FROM capacity_occupancy ORDER BY issue_id").expect("capacity occupancy")),
    });
    connection.close().expect("close observer");
    eprintln!(
        "{}",
        json!({"kind": "policy_bookkeeping", "workspace": root, "rows": result})
    );
    result
}

#[test]
fn mcp_refreshes_workflow_capacity_and_close_policy() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "policy"]);
    let first = first_id(&cli_json(root, &["create", "First admission"]));
    let second = first_id(&cli_json(root, &["create", "Second admission"]));
    let rework = first_id(&cli_json(root, &["create", "Custom ready work"]));
    let policy_path = root.join(".beads/policy.yaml");
    std::fs::write(&policy_path, "workflow:\n  strict: true\n  statuses: [open, in_progress, rework, closed]\n  status_groups:\n    ready: [rework]\n  capacity:\n    statuses:\n      in_progress:\n        hard: 1\n").expect("policy");
    let mut client = McpClient::spawn(root);
    client.call_tool("update_issue", json!({"id": rework, "status": "rework"}));
    let cli_ready = cli_json(root, &["ready"]);
    let mut expected = BTreeSet::new();
    ids_in(&cli_ready, &mut expected);
    assert_eq!(expected, BTreeSet::from([rework.clone()]));
    let resource = client.read_resource("beads://issues/ready");
    assert_eq!(resource["count"], 1);
    assert_eq!(resource["issues"][0]["id"], rework);
    let overview = client.call_tool("project_overview", json!({}));
    assert_eq!(overview["counts"]["ready"], 1, "{overview}");
    for name in ["triage", "plan_next_work"] {
        let prompt = client.request("prompts/get", json!({"name": name, "arguments": {}}));
        assert!(contains_text(&prompt, &rework), "{name}: {prompt}");
    }
    let report = client.request(
        "prompts/get",
        json!({"name": "status_report", "arguments": {}}),
    );
    let text = report["messages"][0]["content"]["text"]
        .as_str()
        .expect("status report text");
    let data: Value = serde_json::from_str(
        text.strip_prefix("Here is the current project data:\n\n")
            .expect("status report context"),
    )
    .expect("status report JSON");
    assert_eq!(data["counts"]["ready"], 1);
    client.call_tool(
        "update_issue",
        json!({"id": first, "status": "in_progress"}),
    );
    let rejected = br_command(root)
        .args(["update", &second, "--status", "in_progress", "--json"])
        .output()
        .expect("CLI capacity refusal");
    assert_eq!(rejected.status.code(), Some(4));
    let error: Value = serde_json::from_slice(&rejected.stdout).expect("CLI error JSON");
    assert_eq!(error["error"]["code"], "WORKFLOW_CAPACITY_EXCEEDED");
    assert_policy_refusal_unchanged(
        &mut client,
        root,
        "update_issue",
        json!({"id": second, "status": "in_progress"}),
        "WORKFLOW_CAPACITY_EXCEEDED",
    );

    // Editing policy without restarting serve must change both admissions and readiness.
    std::fs::write(&policy_path, "workflow:\n  strict: true\n  statuses: [open, in_progress, rework, closed]\n  status_groups:\n    ready: [open]\n  capacity:\n    statuses:\n      in_progress:\n        hard: 2\n").expect("refresh policy");
    assert_eq!(
        client.read_resource("beads://issues/ready")["issues"][0]["id"],
        second
    );
    client.call_tool(
        "update_issue",
        json!({"id": second, "status": "in_progress"}),
    );
    assert_eq!(
        cli_json(root, &["list", "--status", "in_progress"])["issues"]
            .as_array()
            .expect("issues")
            .len(),
        2
    );

    exercise_close_policy(&mut client, root, &first, &second, &rework);
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

fn exercise_close_policy(
    client: &mut McpClient,
    root: &Path,
    first: &str,
    second: &str,
    rework: &str,
) {
    let policy_path = root.join(".beads/policy.yaml");
    std::fs::write(&policy_path, "close_policy:\n  require_close_reason:\n    enabled: true\n    min_length: 1\n  require_acceptance_criteria_satisfied:\n    enabled: true\n  attribution:\n    tier: capture\n").expect("close policy");
    client.call_tool(
        "update_issue",
        json!({"id": first, "add_acceptance": ["Verified behavior"]}),
    );
    assert_policy_refusal_unchanged(
        client,
        root,
        "close_issue",
        json!({"id": first}),
        "POLICY_VIOLATION",
    );
    let acceptance_error = assert_policy_refusal_unchanged(
        client,
        root,
        "close_issue",
        json!({"id": first, "reason": "completed"}),
        "POLICY_VIOLATION",
    );
    assert!(
        contains_text(&acceptance_error, "acceptance_criteria_unchecked"),
        "{acceptance_error}"
    );
    client.call_tool(
        "update_issue",
        json!({"id": first, "check_acceptance": ["1"]}),
    );
    client.call_tool(
        "manage_dependencies",
        json!({"action": "add", "id": first, "depends_on": second}),
    );
    assert_policy_refusal_unchanged(
        client,
        root,
        "close_issue",
        json!({"id": first, "reason": "completed"}),
        "ISSUE_BLOCKED",
    );
    client.call_tool(
        "close_issue",
        json!({"id": second, "reason": "prerequisite complete"}),
    );
    let closed = client.call_tool("close_issue", json!({"id": first, "reason": "completed", "agent_name": "policy-proof", "harness": "stdio", "model": "test"}));
    assert_eq!(closed["status"], "closed");
    let events = client.read_resource("beads://events/recent");
    assert!(
        events["events"]
            .as_array()
            .expect("events")
            .iter()
            .any(|event| event["issue_id"] == first && event["agent_name"] == "policy-proof"),
        "{events}"
    );
    std::fs::write(&policy_path, "workflow: [broken").expect("malformed policy");
    let error = client.tool_error(
        "update_issue",
        json!({"id": rework, "title": "must not persist"}),
    );
    assert!(contains_text(&error, "policy"), "{error}");
    std::fs::write(&policy_path, "{}").expect("restore valid policy");
    assert!(contains_text(
        &client.read_resource(&format!("beads://issue/{rework}")),
        "Custom ready work"
    ));
}

#[test]
fn mcp_honors_required_fields_fresh_gates_and_exact_custom_status_names() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "gates"]);
    let id = first_id(&cli_json(root, &["create", "Gated work"]));
    std::fs::write(
        root.join(".beads/policy.yaml"),
        r#"workflow:
  strict: true
  statuses: [open, active, in_review, closed]
  transitions:
    open: [active]
    active: [in_review, open]
    in_review: [active, closed]
    closed: [active]
  required_fields:
    "active -> in_review": [acceptance_criteria, transition_comment]
  gates:
    "in_review -> closed":
      require_all: [ci_green]
"#,
    )
    .expect("gate policy");
    let mut client = McpClient::spawn(root);
    assert_required_fields_and_custom_statuses(&mut client, root, &id);
    assert_fresh_close_gates(&mut client, root, &id);
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

#[test]
fn mcp_prerequisites_and_acceptance_round_trip_through_prospective_policy() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "prereq"]);
    let mut client = McpClient::spawn(root);
    let pending = "Preparation:\r\n- [ ] accès approved\r\n";
    let complete = "Preparation:\r\n- [x] accès approved\r\n";
    let acceptance = "Delivery:\n- [ ] implement feature\n";
    let cli_pending = "- [ ] Review the CLI interface\n";
    let cli_acceptance = "- [ ] Ship the CLI behavior\n";
    let created = client.call_tool(
        "create_issue",
        json!({"title": "Independent checklists", "prerequisites": pending,
            "acceptance_criteria": acceptance}),
    );
    let id = created["id"].as_str().expect("created ID");
    assert_checklist_fields(&mut client, root, id, pending, acceptance, None);
    let cli_id = first_id(&cli_json(
        root,
        &[
            "create",
            "CLI checklist source",
            "--prerequisites",
            cli_pending,
            "--acceptance-criteria",
            cli_acceptance,
        ],
    ));
    write_prerequisite_policy(root, "in_planning", "acceptance_criteria_present");
    let changed = client.call_tool(
        "update_issue",
        json!({"id": id, "status": "in_planning", "prerequisites": complete,
            "acceptance_criteria": acceptance, "transition_comment": "Ready to plan"}),
    );
    assert_eq!(changed["status"], "in_planning");
    assert_checklist_fields(&mut client, root, id, complete, acceptance, None);
    let shown = client.call_tool("show_issue", json!({"id": id}));
    assert_eq!(shown["comments"][0]["text"], "Ready to plan", "{shown}");
    cli_json(
        root,
        &[
            "comments",
            "add",
            &cli_id,
            "--message",
            "CLI source discussion: preserve accès and context",
            "--author",
            "cli-reviewer",
        ],
    );
    cli_json(root, &["dep", "add", &cli_id, id, "--type", "related"]);
    assert_mcp_presence_handoffs(&mut client, root);
    assert_imported_checklists(
        &mut client,
        root,
        &[
            (id, complete, acceptance, None),
            (&cli_id, cli_pending, cli_acceptance, Some(id)),
        ],
    );

    // The same running server must observe the newly strengthened target rule.
    write_prerequisite_policy(root, "in_implementation", "acceptance_criteria");
    assert_policy_refusal_unchanged(
        &mut client,
        root,
        "update_issue",
        json!({"id": id, "status": "in_implementation", "force": true,
            "transition_comment": "Completion now required"}),
        "POLICY_VIOLATION",
    );
    client.call_tool(
        "update_issue",
        json!({"id": id, "status": "in_implementation", "check_acceptance": ["1"],
            "transition_comment": "Implementation complete"}),
    );
    assert_checklist_fields(
        &mut client,
        root,
        id,
        complete,
        "Delivery:\n- [x] implement feature\n",
        None,
    );
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

fn write_mcp_class_policy(root: &Path, enabled: bool, open_capacity: usize) {
    let class_rules = if enabled {
        "  class_transitions:\n    - {issue_type: bug, from: draft, to: open}\n"
    } else {
        ""
    };
    std::fs::write(
        root.join(".beads/policy.yaml"),
        format!(
            r#"workflow:
  strict: true
  statuses: [draft, planned, open, in_progress, review, closed]
  transitions:
    initial: [draft]
    draft: [planned]
    planned: [open]
    open: [in_progress, draft]
    in_progress: [review]
    review: [closed]
    closed: [draft]
{class_rules}  required_fields:
    "draft -> open": [acceptance_criteria_present, transition_comment]
  gates:
    "draft -> open":
      require_all: [ci_green]
      require_if:
        - label: security
          gate: security_review
  capacity:
    statuses:
      open: {{hard: {open_capacity}}}
close_policy:
  require_acceptance_criteria_satisfied:
    enabled: true
"#
        ),
    )
    .expect("class transition policy");
}

fn mcp_class_draft(root: &Path, title: &str, kind: &str, criteria: &str) -> String {
    first_id(&cli_json(
        root,
        &[
            "create",
            title,
            "--status",
            "draft",
            "--type",
            kind,
            "--acceptance-criteria",
            criteria,
            "--labels",
            "security",
            "--prerequisites",
            "- [x] Retain preparation context",
        ],
    ))
}

fn mcp_class_gate(root: &Path, id: &str, gate: &str, status: &str) {
    cli_json(
        root,
        &[
            "gate",
            "report",
            id,
            "--gate",
            gate,
            "--provider",
            "ci",
            "--status",
            status,
            "--to",
            "open",
        ],
    );
}

fn prepare_mcp_class_gates(root: &Path, id: &str) {
    for gate in ["ci_green", "security_review"] {
        mcp_class_gate(root, id, gate, "pass");
    }
}

#[test]
fn mcp_class_transitions_refresh_prospective_types_and_preserve_imported_data() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "class"]);
    write_mcp_class_policy(root, true, 2);
    let bug = mcp_class_draft(root, "Class bug context", "bug", "- [x] Reviewed");
    let task = mcp_class_draft(root, "Class task context", "task", "- [ ] Deliver task");
    let unknown = mcp_class_draft(
        root,
        "Custom class context",
        "undone_work",
        "- [x] Reviewed",
    );
    for id in [&bug, &task, &unknown] {
        prepare_mcp_class_gates(root, id);
    }
    let mut client = McpClient::spawn(root);
    assert_mcp_class_scope_refusals(&mut client, root, &bug, &task, &unknown);
    write_mcp_class_policy(root, false, 2);
    assert_policy_refusal_unchanged(
        &mut client,
        root,
        "update_issue",
        json!({"id": bug, "status": "open", "transition_comment": "Removed class edge"}),
        "VALIDATION_FAILED",
    );
    write_mcp_class_policy(root, true, 2);
    let admitted = client.call_tool(
        "update_issue",
        json!({"id": bug, "status": "open", "transition_comment": "Refreshed class edge"}),
    );
    assert_eq!(admitted["status"], "open");
    let prospective = client.call_tool(
        "update_issue",
        json!({"id": task, "type": "bug", "status": "open", "transition_comment": "Prospective class edge"}),
    );
    assert_eq!(prospective["status"], "open");
    for id in [&bug, &task] {
        let shown = first_record(cli_json(root, &["show", id]));
        assert_eq!(shown["issue_type"], "bug");
        assert_eq!(shown["status"], "open");
        assert_mcp_class_transition_event(root, id);
    }
    cli_json(root, &["dep", "add", &task, &bug, "--type", "related"]);
    assert_imported_checklists(
        &mut client,
        root,
        &[
            (
                &bug,
                "- [x] Retain preparation context",
                "- [x] Reviewed",
                None,
            ),
            (
                &task,
                "- [x] Retain preparation context",
                "- [ ] Deliver task",
                Some(&bug),
            ),
            (
                &unknown,
                "- [x] Retain preparation context",
                "- [x] Reviewed",
                None,
            ),
        ],
    );
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

fn assert_mcp_class_scope_refusals(
    client: &mut McpClient,
    root: &Path,
    bug: &str,
    task: &str,
    unknown: &str,
) {
    // MCP create has no status input: its open default still obeys initial admission.
    assert_policy_refusal_unchanged(
        client,
        root,
        "create_issue",
        json!({"title": "Cannot skip draft", "type": "bug", "acceptance_criteria": "- [x] Ready"}),
        "VALIDATION_FAILED",
    );
    for id in [task, unknown] {
        assert_policy_refusal_unchanged(
            client,
            root,
            "update_issue",
            json!({"id": id, "status": "open", "transition_comment": "Wrong class", "force": true}),
            "VALIDATION_FAILED",
        );
    }
    assert_policy_refusal_unchanged(
        client,
        root,
        "update_issue",
        json!({"id": bug, "type": "task", "status": "open", "title": "Must not persist",
            "transition_comment": "Old class cannot authorize"}),
        "VALIDATION_FAILED",
    );
    for status in ["review", "unlisted_status"] {
        assert_policy_refusal_unchanged(
            client,
            root,
            "update_issue",
            json!({"id": bug, "status": status, "transition_comment": "Only the named edge", "force": true}),
            "VALIDATION_FAILED",
        );
    }
    assert_policy_refusal_unchanged(
        client,
        root,
        "close_issue",
        json!({"id": bug, "reason": "Omitted edge"}),
        "VALIDATION_FAILED",
    );
}

fn assert_mcp_class_transition_event(root: &Path, id: &str) {
    let connection = read_only_db(root);
    let events = connection.query_with_params(
        "SELECT old_value, new_value, actor, id FROM events WHERE issue_id = ? AND event_type = 'status_changed' ORDER BY id",
        &[id.into()],
    ).expect("class transition events");
    eprintln!(
        "{}",
        json!({"kind": "class_transition_events", "workspace": root,
        "id": id, "rows": format!("{events:?}")})
    );
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].get(0), Some(&"draft".into()));
    assert_eq!(events[0].get(1), Some(&"open".into()));
    assert_eq!(events[0].get(2), Some(&ACTOR.into()));
    connection.close().expect("close class event observer");
}

#[test]
fn mcp_class_transitions_keep_required_fields_gates_capacity_and_close_guards() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "guard"]);
    write_mcp_class_policy(root, true, 1);
    let bug = mcp_class_draft(root, "Guarded class bug", "bug", "");
    let occupant = mcp_class_draft(root, "Other class bug", "bug", "- [x] Ready");
    for id in [&bug, &occupant] {
        prepare_mcp_class_gates(root, id);
    }
    let mut client = McpClient::spawn(root);
    assert_mcp_class_entry_guards(&mut client, root, &bug);
    client.call_tool(
        "update_issue",
        json!({"id": occupant, "status": "open", "transition_comment": "First admission"}),
    );
    assert_policy_refusal_unchanged(
        &mut client,
        root,
        "update_issue",
        json!({"id": bug, "status": "open", "transition_comment": "Capacity applies", "force": true}),
        "WORKFLOW_CAPACITY_EXCEEDED",
    );
    client.call_tool("update_issue", json!({"id": occupant, "status": "draft"}));
    client.call_tool(
        "update_issue",
        json!({"id": bug, "status": "open", "transition_comment": "All guards satisfied"}),
    );
    cli_json(root, &["dep", "add", &bug, &occupant]);
    assert_policy_refusal_unchanged(
        &mut client,
        root,
        "update_issue",
        json!({"id": bug, "status": "in_progress"}),
        "VALIDATION_FAILED",
    );
    cli_json(root, &["dep", "remove", &bug, &occupant]);
    client.call_tool("update_issue", json!({"id": bug, "status": "in_progress"}));
    client.call_tool("update_issue", json!({"id": bug, "status": "review"}));
    let error = assert_policy_refusal_unchanged(
        &mut client,
        root,
        "close_issue",
        json!({"id": bug}),
        "POLICY_VIOLATION",
    );
    assert!(
        contains_text(&error, "acceptance_criteria_unchecked"),
        "{error}"
    );
    client.call_tool(
        "update_issue",
        json!({"id": bug, "check_acceptance": ["1"]}),
    );
    assert_eq!(
        client.call_tool("close_issue", json!({"id": bug}))["status"],
        "closed"
    );
    assert_eq!(
        first_record(cli_json(root, &["show", &bug]))["status"],
        "closed"
    );
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

fn assert_mcp_class_entry_guards(client: &mut McpClient, root: &Path, bug: &str) {
    let error = assert_policy_refusal_unchanged(
        client,
        root,
        "update_issue",
        json!({"id": bug, "status": "open", "transition_comment": "Still needs criteria"}),
        "POLICY_VIOLATION",
    );
    assert!(
        contains_text(&error, "transition_acceptance_criteria_missing"),
        "{error}"
    );
    client.call_tool(
        "update_issue",
        json!({"id": bug, "acceptance_criteria": "- [ ] Deliver fix"}),
    );
    cli_json(
        root,
        &[
            "comments",
            "add",
            bug,
            "--message",
            "Earlier discussion cannot authorize this transition",
        ],
    );
    let error = assert_policy_refusal_unchanged(
        client,
        root,
        "update_issue",
        json!({"id": bug, "status": "open"}),
        "POLICY_VIOLATION",
    );
    assert!(
        contains_text(&error, "transition_comment_missing"),
        "{error}"
    );
    for gate in ["ci_green", "security_review"] {
        mcp_class_gate(root, bug, gate, "fail");
        let error = assert_policy_refusal_unchanged(
            client,
            root,
            "update_issue",
            json!({"id": bug, "status": "open", "transition_comment": "Gate must pass", "force": true}),
            "POLICY_VIOLATION",
        );
        assert!(contains_text(&error, &format!("gate_{gate}")), "{error}");
        mcp_class_gate(root, bug, gate, "pass");
    }
}

#[test]
fn mcp_class_batch_retains_ordered_partial_results_without_loser_mutations() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "partialclass"]);
    write_mcp_class_policy(root, true, 1);
    let bug = mcp_class_draft(root, "Allowed batch member", "bug", "- [ ] Deliver bug");
    let task = mcp_class_draft(root, "Refused batch member", "task", "- [ ] Deliver task");
    for id in [&bug, &task] {
        prepare_mcp_class_gates(root, id);
    }
    let mut client = McpClient::spawn(root);
    let before = capacity_contender_state(root, &task);
    let batch = client.call_tool("update_issue", json!({"updates": [
        {"id": bug, "status": "open", "transition_comment": "First item commits"},
        {"id": task, "status": "open", "title": "Must not persist", "transition_comment": "Second item refuses"}
    ]}));
    assert_eq!(batch["count"], 2);
    assert_eq!(batch["ok_count"], 1);
    assert_eq!(batch["error_count"], 1);
    assert_eq!(batch["items"][0]["id"], bug);
    assert_eq!(batch["items"][0]["ok"], true);
    assert_eq!(batch["items"][1]["id"], task);
    assert_eq!(batch["items"][1]["ok"], false);
    assert_eq!(
        batch["items"][1]["error"]["data"]["error_type"],
        "VALIDATION_FAILED"
    );
    assert_eq!(capacity_contender_state(root, &task), before);
    assert_eq!(
        first_record(cli_json(root, &["show", &bug]))["status"],
        "open"
    );
    assert_mcp_class_transition_event(root, &bug);
    let comments = cli_json(root, &["comments", "list", &bug]);
    assert_eq!(comments.as_array().expect("winner comments").len(), 1);
    assert_eq!(comments[0]["text"], "First item commits");
    assert_eq!(comments[0]["author"], ACTOR);
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

#[test]
fn cli_and_mcp_class_edges_compete_for_one_capacity_slot() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "classrace"]);
    std::fs::write(
        root.join(".beads/policy.yaml"),
        r"workflow:
  strict: true
  statuses: [draft, open, in_progress, closed]
  transitions:
    initial: [draft]
    draft: [open]
    in_progress: [open]
  class_transitions:
    - {issue_type: bug, from: draft, to: in_progress}
  capacity:
    statuses:
      in_progress: {hard: 1}
",
    )
    .expect("class capacity race policy");
    let cli_id = mcp_class_draft(root, "CLI class contender", "bug", "- [ ] Deliver CLI");
    let mcp_id = mcp_class_draft(root, "MCP class contender", "bug", "- [ ] Deliver MCP");
    let mut client = McpClient::spawn(root);
    assert_capacity_race(&mut client, root, &cli_id, &mcp_id, 0, false);
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

fn assert_mcp_presence_handoffs(client: &mut McpClient, root: &Path) {
    for criteria in ["Planning prose", "- [x] Planning review complete"] {
        let id = first_id(&client.call_tool(
            "create_issue",
            json!({"title": format!("Presence handoff: {criteria}"),
                "prerequisites": "- [ ] Review preparation"}),
        ));
        let changed = client.call_tool(
            "update_issue",
            json!({"id": id, "status": "in_planning",
                "prerequisites": "- [x] Review preparation",
                "acceptance_criteria": criteria, "transition_comment": "Presence is sufficient"}),
        );
        assert_eq!(changed["status"], "in_planning", "{changed}");
        assert_checklist_fields(
            client,
            root,
            &id,
            "- [x] Review preparation",
            criteria,
            None,
        );
        let comments = cli_json(root, &["comments", "list", &id]);
        assert_eq!(comments.as_array().expect("handoff comments").len(), 1);
        assert_eq!(comments[0]["text"], "Presence is sufficient");
    }
}

fn write_prerequisite_policy(root: &Path, target: &str, acceptance_requirement: &str) {
    std::fs::write(
        root.join(".beads/policy.yaml"),
        format!(
            "workflow:\n  strict: true\n  statuses: [open, in_planning, in_implementation, closed]\n  required_fields:\n    {target}: [prerequisites_complete, {acceptance_requirement}, transition_comment]\n"
        ),
    )
    .expect("prerequisite policy");
}

fn assert_imported_checklists(
    source_client: &mut McpClient,
    source: &Path,
    issues: &[(&str, &str, &str, Option<&str>)],
) {
    let before: Vec<_> = issues
        .iter()
        .map(|(id, _, _, _)| {
            (
                first_record(cli_json(source, &["show", id])),
                source_client.call_tool("show_issue", json!({"id": id})),
            )
        })
        .collect();
    let imported = ProtocolWorkspace::new().expect("second independent workspace");
    let root = imported.path();
    cli_json(root, &["init", "--prefix", "imported"]);
    std::fs::copy(
        source.join(".beads/issues.jsonl"),
        root.join(".beads/issues.jsonl"),
    )
    .expect("copy actual source export into import workspace");
    cli_json(root, &["sync", "--import-only"]);
    let mut client = McpClient::spawn(root);
    for ((id, prerequisites, acceptance, dependency), (source_cli, source_mcp)) in
        issues.iter().zip(before)
    {
        assert_checklist_fields(
            &mut client,
            root,
            id,
            prerequisites,
            acceptance,
            *dependency,
        );
        for (original, imported) in [
            (source_cli, first_record(cli_json(root, &["show", id]))),
            (
                source_mcp,
                client.call_tool("show_issue", json!({"id": id})),
            ),
        ] {
            for field in [
                "id",
                "title",
                "description",
                "status",
                "priority",
                "issue_type",
                "labels",
                "comments",
                "dependencies",
                "dependents",
            ] {
                assert_eq!(
                    imported.get(field),
                    original.get(field),
                    "import changed {id}.{field}: {imported}"
                );
            }
        }
    }
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

fn assert_checklist_fields(
    client: &mut McpClient,
    root: &Path,
    id: &str,
    prerequisites: &str,
    acceptance: &str,
    expected_dependency: Option<&str>,
) {
    let jsonl = std::fs::read_to_string(root.join(".beads/issues.jsonl")).expect("JSONL");
    let exported = jsonl
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("exported issue"))
        .find(|issue| issue["id"] == id)
        .expect("created issue exported");
    for (shown, dependency_id_key, dependency_type_key) in [
        (
            client.call_tool("show_issue", json!({"id": id})),
            "id",
            "dep_type",
        ),
        (
            first_record(cli_json(root, &["show", id])),
            "id",
            "dependency_type",
        ),
        (exported, "depends_on_id", "type"),
    ] {
        assert_eq!(shown["prerequisites"], prerequisites, "{shown}");
        assert_eq!(shown["acceptance_criteria"], acceptance, "{shown}");
        if let Some(expected_id) = expected_dependency {
            let dependencies = shown["dependencies"]
                .as_array()
                .expect("explicit dependency");
            assert_eq!(dependencies.len(), 1, "{shown}");
            let dependency = &dependencies[0];
            assert_eq!(
                dependency.get(dependency_id_key),
                Some(&json!(expected_id)),
                "{shown}"
            );
            assert_eq!(
                dependency.get(dependency_type_key),
                Some(&json!("related")),
                "{shown}"
            );
        } else {
            assert!(
                shown
                    .get("dependencies")
                    .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty)),
                "prerequisite text must not create edges: {shown}"
            );
        }
    }
}

#[test]
fn mcp_prerequisite_refusals_use_new_values_and_ignore_force() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "prereq"]);
    let mut client = McpClient::spawn(root);
    let absent = first_id(&client.call_tool(
        "create_issue",
        json!({"title": "Absent prerequisites", "acceptance_criteria": "- [ ] deliver"}),
    ));
    let complete = first_id(&client.call_tool(
        "create_issue",
        json!({"title": "Previously complete prerequisites", "prerequisites": "- [x] access",
            "acceptance_criteria": "- [ ] deliver"}),
    ));
    write_prerequisite_policy(root, "in_planning", "acceptance_criteria_present");
    for force in [false, true] {
        assert_policy_refusal_unchanged(
            &mut client,
            root,
            "update_issue",
            json!({"id": absent, "status": "in_planning", "force": force,
                "transition_comment": "Cannot authorize absent prerequisites"}),
            "POLICY_VIOLATION",
        );
    }
    for id in [&absent, &complete] {
        for replacement in [
            Value::Null,
            json!(""),
            json!(" \t\n"),
            json!("All done"),
            json!("- [ ] access"),
            json!("- [x] access\n- [ ] Review pending"),
        ] {
            assert_policy_refusal_unchanged(
                &mut client,
                root,
                "update_issue",
                json!({"id": id, "status": "in_planning", "prerequisites": replacement,
                    "title": "Must roll back", "force": true,
                    "transition_comment": "Invalid prospective checklist", "comment": "Must not append"}),
                "POLICY_VIOLATION",
            );
        }
    }
    assert_mcp_presence_refusals(&mut client, root, &absent);
    assert_policy_refusal_unchanged(
        &mut client,
        root,
        "update_issue",
        json!({"id": absent, "status": "in_planning", "prerequisites": "- [x] access"}),
        "POLICY_VIOLATION",
    );
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

fn assert_mcp_presence_refusals(client: &mut McpClient, root: &Path, populated: &str) {
    let missing = first_id(&client.call_tool(
        "create_issue",
        json!({"title": "Missing acceptance criteria", "prerequisites": "- [x] access"}),
    ));
    assert_policy_refusal_unchanged(
        client,
        root,
        "update_issue",
        json!({"id": missing, "status": "in_planning",
            "transition_comment": "Omitted criteria must remain absent", "comment": "Must not append"}),
        "POLICY_VIOLATION",
    );
    for id in [populated, missing.as_str()] {
        for criteria in [Value::Null, json!(""), json!(" "), json!(" \t\n")] {
            let error = assert_policy_refusal_unchanged(
                client,
                root,
                "update_issue",
                json!({"id": id, "status": "in_planning", "prerequisites": "- [x] access",
                    "acceptance_criteria": criteria, "title": "Must roll back", "force": true,
                    "transition_comment": "Criteria still required", "comment": "Must not append"}),
                "POLICY_VIOLATION",
            );
            assert!(
                contains_text(&error, "acceptance_criteria_present"),
                "{error}"
            );
        }
    }
}

#[test]
fn mcp_whole_checklist_replacements_are_guarded_and_conflict_with_item_edits() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "prereq"]);
    let mut client = McpClient::spawn(root);
    let created = client.call_tool(
        "create_issue",
        json!({"title": "Guarded checklists", "prerequisites": "- [x] access approved",
            "acceptance_criteria": "- [ ] feature delivered"}),
    );
    let id = created["id"].as_str().expect("created ID");
    for field in ["prerequisites", "acceptance_criteria"] {
        for replacement in [Value::Null, json!(""), json!("x")] {
            let mut args = json!({"id": id, "comment": "Must not append"});
            args[field] = replacement;
            assert_policy_refusal_unchanged(
                &mut client,
                root,
                "update_issue",
                args,
                "VALIDATION_FAILED",
            );
        }
    }
    for edit in ["check_acceptance", "uncheck_acceptance", "add_acceptance"] {
        let before = policy_bookkeeping(root);
        let mut args = json!({"id": id, "acceptance_criteria": "- [ ] replacement", "force": true});
        args[edit] = json!(["1"]);
        let error = client.tool_error("update_issue", args);
        assert!(contains_text(&error, "cannot be combined"), "{error}");
        assert_eq!(policy_bookkeeping(root), before);
    }
    client.call_tool(
        "update_issue",
        json!({"id": id, "prerequisites": null, "acceptance_criteria": null, "force": true}),
    );
    let shown = client.call_tool("show_issue", json!({"id": id}));
    assert!(
        shown.get("prerequisites").is_none_or(Value::is_null),
        "{shown}"
    );
    assert!(
        shown.get("acceptance_criteria").is_none_or(Value::is_null),
        "{shown}"
    );
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

fn assert_required_fields_and_custom_statuses(client: &mut McpClient, root: &Path, id: &str) {
    let changed = client.call_tool("update_issue", json!({"id": id, "status": "active"}));
    assert_eq!(changed["status"], "active");
    let listed = client.call_tool("list_issues", json!({"status": "active"}));
    assert!(contains_text(&listed, id), "{listed}");
    assert_cli_policy_refusal_unchanged(
        root,
        &["update", id, "--status", "undeclared"],
        "VALIDATION_FAILED",
    );
    assert_policy_refusal_unchanged(
        client,
        root,
        "update_issue",
        json!({"id": id, "status": "undeclared"}),
        "VALIDATION_FAILED",
    );
    assert_cli_policy_refusal_unchanged(
        root,
        &["update", id, "--status", "in_review"],
        "POLICY_VIOLATION",
    );
    assert_policy_refusal_unchanged(
        client,
        root,
        "update_issue",
        json!({"id": id, "status": "in_review"}),
        "POLICY_VIOLATION",
    );
    let unchecked = assert_policy_refusal_unchanged(
        client,
        root,
        "update_issue",
        json!({"id": id, "status": "in_review",
            "add_acceptance": ["Review evidence"], "transition_comment": "Ready for review"}),
        "POLICY_VIOLATION",
    );
    assert!(
        contains_text(&unchecked, "transition_acceptance_criteria_unchecked"),
        "{unchecked}"
    );
    client.call_tool(
        "update_issue",
        json!({"id": id, "add_acceptance": ["Review evidence"]}),
    );
    client.call_tool(
        "update_issue",
        json!({"id": id, "status": "in_review",
            "check_acceptance": ["1"], "transition_comment": "Ready for review"}),
    );
}

fn assert_fresh_close_gates(client: &mut McpClient, root: &Path, id: &str) {
    assert_cli_policy_refusal_unchanged(root, &["close", id], "POLICY_VIOLATION");
    assert_policy_refusal_unchanged(
        client,
        root,
        "close_issue",
        json!({"id": id}),
        "POLICY_VIOLATION",
    );
    cli_json(
        root,
        &[
            "gate",
            "report",
            id,
            "--gate",
            "ci_green",
            "--provider",
            "ci",
            "--status",
            "pass",
            "--to",
            "closed",
        ],
    );
    client.call_tool("update_issue", json!({"id": id, "status": "active"}));
    client.call_tool(
        "update_issue",
        json!({"id": id, "status": "in_review", "transition_comment": "Reviewed again"}),
    );
    assert_cli_policy_refusal_unchanged(root, &["close", id], "POLICY_VIOLATION");
    let stale = assert_policy_refusal_unchanged(
        client,
        root,
        "close_issue",
        json!({"id": id}),
        "POLICY_VIOLATION",
    );
    assert!(contains_text(&stale, "stale_status_revision"), "{stale}");
    cli_json(
        root,
        &[
            "gate",
            "report",
            id,
            "--gate",
            "ci_green",
            "--provider",
            "ci",
            "--status",
            "pass",
            "--to",
            "closed",
        ],
    );
    assert_eq!(
        client.call_tool("close_issue", json!({"id": id}))["status"],
        "closed"
    );
}

#[test]
fn cli_and_mcp_compete_for_one_capacity_slot_without_loser_side_effects() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "race"]);
    let cli_id = first_id(&cli_json(root, &["create", "CLI contender"]));
    let mcp_id = first_id(&cli_json(root, &["create", "MCP contender"]));
    std::fs::write(
        root.join(".beads/policy.yaml"),
        "workflow:\n  statuses: [open, in_progress, closed]\n  capacity:\n    statuses:\n      in_progress:\n        hard: 1\n",
    )
    .expect("capacity policy");
    let mut client = McpClient::spawn(root);
    assert_discovery_surface(&mut client);
    for round in 0..4 {
        assert_capacity_race(&mut client, root, &cli_id, &mcp_id, round, false);
    }
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
    cli_json(root, &["update", &cli_id, "--status", "in_progress"]);
}

#[test]
fn cli_and_mcp_prerequisite_handoffs_compete_without_loser_field_or_comment_changes() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "handoff"]);
    let cli_id = first_id(&cli_json(
        root,
        &[
            "create",
            "CLI handoff",
            "--acceptance-criteria",
            "- [ ] Deliver CLI",
        ],
    ));
    let mcp_id = first_id(&cli_json(
        root,
        &[
            "create",
            "MCP handoff",
            "--acceptance-criteria",
            "- [ ] Deliver MCP",
        ],
    ));
    std::fs::write(
        root.join(".beads/policy.yaml"),
        "workflow:\n  statuses: [open, in_progress, closed]\n  required_fields:\n    in_progress: [prerequisites_complete, acceptance_criteria_present, transition_comment]\n  capacity:\n    statuses:\n      in_progress:\n        hard: 1\n",
    )
    .expect("prerequisite admission policy");
    let mut client = McpClient::spawn(root);
    for round in 0..4 {
        for (id, pending) in [
            (&cli_id, "- [ ] CLI input ready"),
            (&mcp_id, "- [ ] MCP input ready"),
        ] {
            cli_json(root, &["update", id, "--prerequisites", pending, "--force"]);
        }
        assert_capacity_race(&mut client, root, &cli_id, &mcp_id, round, true);
    }
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

fn assert_capacity_race(
    client: &mut McpClient,
    root: &Path,
    cli_id: &str,
    mcp_id: &str,
    round: usize,
    prerequisites: bool,
) {
    let before = cli_json(root, &["list", "--all"]);
    let events = client.read_resource("beads://events/recent");
    let comments_before = [cli_id, mcp_id].map(|id| cli_json(root, &["comments", "list", id]));
    let stored_before = [cli_id, mcp_id].map(|id| capacity_contender_state(root, id));
    let (cli, response) = run_capacity_contenders(client, root, cli_id, mcp_id, prerequisites);
    assert!(response.get("error").is_none(), "round {round}: {response}");
    let mcp_ok = response["result"]["isError"] != true;
    assert_ne!(
        cli.status.success(),
        mcp_ok,
        "round {round}: CLI={cli:?}, MCP={response}"
    );
    let loser = if mcp_ok {
        assert_eq!(cli.status.code(), Some(4), "{cli:?}");
        let refusal: Value = serde_json::from_slice(&cli.stdout).expect("CLI refusal JSON");
        assert_eq!(
            refusal["error"]["code"], "WORKFLOW_CAPACITY_EXCEEDED",
            "{refusal}"
        );
        cli_id
    } else {
        assert_eq!(
            response["result"]["structuredContent"]["data"]["error_type"],
            "WORKFLOW_CAPACITY_EXCEEDED",
            "{response}"
        );
        mcp_id
    };
    assert_eq!(
        capacity_contender_state(root, loser),
        stored_before[usize::from(loser == mcp_id)]
    );
    let after = cli_json(root, &["list", "--all"]);
    let records = after["issues"].as_array().expect("issues");
    assert_eq!(
        records
            .iter()
            .filter(|issue| issue["status"] == "in_progress")
            .count(),
        1
    );
    assert_eq!(
        records.iter().find(|issue| issue["id"] == loser),
        before["issues"]
            .as_array()
            .expect("before issues")
            .iter()
            .find(|issue| issue["id"] == loser)
    );
    for (index, id) in [cli_id, mcp_id].iter().enumerate() {
        let comments = cli_json(root, &["comments", "list", id]);
        if *id == loser || !prerequisites {
            assert_eq!(comments, comments_before[index]);
        } else {
            assert_prerequisite_race_winner(root, id, index, &comments, &comments_before[index]);
        }
    }
    let after_events = client.read_resource("beads://events/recent");
    let loser_events = |value: &Value| {
        value["events"]
            .as_array()
            .expect("events")
            .iter()
            .filter(|event| event["issue_id"] == loser)
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(loser_events(&after_events), loser_events(&events));
    let connection = read_only_db(root);
    assert!(
        connection
            .query("SELECT issue_id FROM dirty_issues")
            .expect("dirty count")
            .is_empty()
    );
    connection.close().expect("close observer");
    cli_json(root, &["update", cli_id, "--status", "open"]);
    cli_json(root, &["update", mcp_id, "--status", "open"]);
}

fn capacity_contender_state(root: &Path, id: &str) -> Value {
    let connection = read_only_db(root);
    let mut snapshot = serde_json::Map::new();
    for (name, sql) in [
        ("issue", "SELECT * FROM issues WHERE id = ?"),
        (
            "comments",
            "SELECT * FROM comments WHERE issue_id = ? ORDER BY id",
        ),
        (
            "events",
            "SELECT * FROM events WHERE issue_id = ? ORDER BY id",
        ),
        (
            "occupancy",
            "SELECT * FROM capacity_occupancy WHERE issue_id = ?",
        ),
        ("dirty", "SELECT * FROM dirty_issues WHERE issue_id = ?"),
        (
            "export_hash",
            "SELECT content_hash FROM export_hashes WHERE issue_id = ?",
        ),
    ] {
        let rows = connection
            .query_with_params(sql, &[id.into()])
            .expect("raw contender rows including stored content hash");
        snapshot.insert(name.to_owned(), json!(format!("{rows:?}")));
    }
    connection.close().expect("close contender observer");
    let jsonl =
        std::fs::read_to_string(root.join(".beads/issues.jsonl")).expect("contender export");
    let row = jsonl
        .lines()
        .find(|line| serde_json::from_str::<Value>(line).expect("exported issue")["id"] == id)
        .expect("contender exported");
    snapshot.insert("jsonl_row".to_owned(), json!(row));
    eprintln!(
        "{}",
        json!({"kind": "contender_state", "workspace": root, "id": id, "rows": snapshot})
    );
    Value::Object(snapshot)
}

fn assert_prerequisite_race_winner(
    root: &Path,
    id: &str,
    index: usize,
    comments: &Value,
    before: &Value,
) {
    let shown = first_record(cli_json(root, &["show", id]));
    let name = ["CLI", "MCP"][index];
    assert_eq!(shown["prerequisites"], format!("- [x] {name} input ready"));
    assert_eq!(
        shown["acceptance_criteria"],
        format!("- [ ] Deliver {name}")
    );
    let comments = comments.as_array().expect("comments");
    let before = before.as_array().expect("before comments");
    assert_eq!(comments.len(), before.len() + 1);
    assert_eq!(&comments[..before.len()], before);
    assert_eq!(comments.last().unwrap()["text"], format!("{name} prepared"));
}

fn run_capacity_contenders(
    client: &mut McpClient,
    root: &Path,
    cli_id: &str,
    mcp_id: &str,
    prerequisites: bool,
) -> (std::process::Output, Value) {
    let mut cli_args = vec![
        "update",
        cli_id,
        "--status",
        "in_progress",
        "--actor",
        "cli-racer",
        "--json",
    ];
    let mut mcp_args = json!({"id": mcp_id, "status": "in_progress"});
    if prerequisites {
        cli_args.extend([
            "--prerequisites",
            "- [x] CLI input ready",
            "--transition-comment",
            "CLI prepared",
        ]);
        mcp_args["prerequisites"] = json!("- [x] MCP input ready");
        mcp_args["transition_comment"] = json!("MCP prepared");
    }
    let authority =
        beads_rust::sync::blocking_write_lock(&root.join(".beads")).expect("hold admission lock");
    let barrier = Barrier::new(3);
    thread::scope(|scope| {
        let cli_thread = scope.spawn(|| {
            let invoked_at = chrono::Utc::now();
            let child = br_command(root)
                .args(&cli_args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start CLI contender");
            barrier.wait();
            let output = child.wait_with_output().expect("CLI contender outcome");
            eprintln!(
                "{}",
                json!({"kind": "cli_contender", "workspace": root,
                "args": cli_args, "invoked_at": invoked_at, "returned_at": chrono::Utc::now(),
                "exit": output.status.code(), "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr)})
            );
            output
        });
        let mcp_thread = scope.spawn(|| {
                let request_id = client.next_id;
                client.next_id += 1;
                client.send(&json!({"jsonrpc": "2.0", "id": request_id, "method": "tools/call", "params": with_era(json!({"name": "update_issue", "arguments": mcp_args}))}));
                barrier.wait();
                client.receive_response(request_id, "tools/call")
            });
        // Both real clients are in flight while the shared admission lock
        // is held. Release it only after both invocation barriers arrive.
        barrier.wait();
        drop(authority);
        (
            cli_thread.join().expect("CLI thread"),
            mcp_thread.join().expect("MCP thread"),
        )
    })
}

#[test]
fn mcp_export_failure_reports_committed_state_and_can_be_reconciled() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "export"]);
    let id = first_id(&cli_json(root, &["create", "Before export failure"]));
    let mut client = McpClient::spawn(root);
    assert_discovery_surface(&mut client);
    let jsonl = root.join(".beads/issues.jsonl");
    let conflict = "<<<<<<< unresolved\n=======\n>>>>>>> incoming\n";
    std::fs::write(&jsonl, conflict).expect("controlled conflicting export destination");
    let error = client.tool_error(
        "update_issue",
        json!({"id": id, "title": "Committed before publication failed"}),
    );
    assert_eq!(error["data"]["error_type"], "AUTO_FLUSH_FAILED", "{error}");
    assert_eq!(error["data"]["mutation_committed"], true);
    assert_eq!(error["data"]["sync_pending"], true);
    assert_eq!(error["data"]["retry_mutation"], false);
    assert_eq!(error["data"]["previous_sync_pending"], false);
    assert_eq!(error["data"]["request_result"]["id"], id);
    assert_eq!(
        client.read_resource(&format!("beads://issue/{id}"))["title"],
        "Committed before publication failed"
    );
    assert_eq!(
        std::fs::read_to_string(&jsonl).expect("conflict retained"),
        conflict
    );
    let connection = read_only_db(root);
    assert_eq!(
        connection
            .query("SELECT issue_id FROM dirty_issues")
            .expect("dirty")
            .len(),
        1
    );
    connection.close().expect("close observer");
    let before = policy_bookkeeping(root);
    let rejected_batch = client.tool_error(
        "update_issue",
        json!({"updates": [{"id": id, "status": "closed"}]}),
    );
    let detail = &rejected_batch["data"];
    assert_eq!(detail["error_type"], "AUTO_FLUSH_FAILED");
    assert_eq!(detail["mutation_committed"], false);
    assert_eq!(detail["previous_sync_pending"], true);
    assert_eq!(detail["sync_pending"], true);
    assert_eq!(detail["retry_mutation"], false);
    let batch = &detail["request_result"];
    assert_eq!(batch["count"], 1);
    assert_eq!(batch["ok_count"], 0);
    assert_eq!(batch["error_count"], 1);
    assert_eq!(batch["items"][0]["ok"], false);
    assert_eq!(batch["items"][0]["id"], id);
    assert!(
        batch["items"][0]["error"]["message"]
            .as_str()
            .expect("refusal")
            .contains("close_issue")
    );
    assert_eq!(
        policy_bookkeeping(root),
        before,
        "a rejected batch must not inherit an earlier commit"
    );
    assert_eq!(
        std::fs::read_to_string(&jsonl).expect("conflict retained"),
        conflict
    );
    std::fs::rename(&jsonl, root.join(".beads/preserved-conflict.jsonl"))
        .expect("retain conflict fixture");
    cli_json(root, &["sync", "--flush-only"]);
    let exported: Vec<Value> = std::fs::read_to_string(&jsonl)
        .expect("published JSONL")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSONL record"))
        .collect();
    assert_eq!(exported.len(), 1);
    assert_eq!(exported[0]["title"], "Committed before publication failed");
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

#[test]
fn mcp_batch_reports_a_committed_item_when_a_later_label_operation_fails() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "partial"]);
    let mut client = McpClient::spawn(root);
    // The documented per-issue label limit is a real storage refusal, not an
    // injected handler error. The title update precedes this label operation.
    let mut labels: Vec<String> = (0..64).map(|index| format!("label-{index}")).collect();
    labels.sort();
    let created = client.call_tool(
        "create_issue",
        json!({"title": "Before partial batch", "labels": labels}),
    );
    let id = created["id"].as_str().expect("created id");
    let batch = client.call_tool(
        "update_issue",
        json!({"updates": [
            {"id": id, "title": "Title committed before label refusal", "labels_add": ["overflow"]},
            {"id": id, "status": "closed"}
        ]}),
    );
    assert_eq!(batch["count"], 2);
    assert_eq!(batch["ok_count"], 0);
    assert_eq!(batch["error_count"], 2);
    let partial = &batch["items"][0];
    assert_eq!(partial["ok"], false);
    assert_eq!(partial["error"]["data"]["error_type"], "VALIDATION_FAILED");
    assert_eq!(partial["error"]["data"]["mutation_committed"], true);
    assert_eq!(partial["error"]["data"]["retry_mutation"], false);
    assert_eq!(
        partial["error"]["data"]["publication"],
        "see_request_outcome"
    );
    let refused = &batch["items"][1];
    assert_eq!(refused["ok"], false);
    assert!(
        refused["error"]["data"].get("mutation_committed").is_none(),
        "clean refusal inherited earlier item's commit: {batch}"
    );
    let issue = client.read_resource(&format!("beads://issue/{id}"));
    assert_eq!(issue["title"], "Title committed before label refusal");
    assert_eq!(issue["status"], "open");
    assert_eq!(issue["labels"], json!(labels));
    let connection = read_only_db(root);
    assert!(
        connection
            .query("SELECT issue_id FROM dirty_issues")
            .expect("dirty")
            .is_empty()
    );
    connection.close().expect("close observer");
    let exported: Vec<Value> = std::fs::read_to_string(root.join(".beads/issues.jsonl"))
        .expect("published JSONL")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSONL record"))
        .collect();
    assert_eq!(exported.len(), 1);
    assert_eq!(exported[0]["title"], "Title committed before label refusal");
    assert_eq!(exported[0]["labels"], json!(labels));
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

#[test]
fn mcp_capacity_scopes_use_actor_harness_and_session_attribution() {
    for scope in ["actor", "harness", "session"] {
        let workspace = ProtocolWorkspace::new().expect("workspace");
        let root = workspace.path();
        cli_json(root, &["init", "--prefix", "scope"]);
        let first = first_id(&cli_json(root, &["create", "First scoped claim"]));
        let second = first_id(&cli_json(root, &["create", "Same scope claim"]));
        std::fs::write(root.join(".beads/policy.yaml"), format!("workflow:\n  statuses: [open, in_progress, closed]\n  capacity:\n    scopes:\n      {scope}:\n        statuses:\n          in_progress:\n            hard: 1\n")).expect("scoped policy");
        let mut client = McpClient::spawn_with_session(root, Some("session-one"));
        client.call_tool("update_issue", json!({"id": first, "status": "in_progress", "agent_name": "scope-proof", "harness": "harness-one", "model": "model-proof"}));
        let error = assert_policy_refusal_unchanged(
            &mut client,
            root,
            "update_issue",
            json!({"id": second, "status": "in_progress", "harness": "harness-one"}),
            "WORKFLOW_CAPACITY_EXCEEDED",
        );
        assert!(contains_text(&error, scope), "{error}");
        let cli = br_command(root)
            .env("BR_SESSION", "session-one")
            .args([
                "update",
                &second,
                "--status",
                "in_progress",
                "--actor",
                ACTOR,
                "--harness",
                "harness-one",
                "--json",
            ])
            .output()
            .expect("paired scoped CLI");
        assert_eq!(cli.status.code(), Some(4), "{scope}: {cli:?}");
        let refusal: Value = serde_json::from_slice(&cli.stdout).expect("scoped refusal JSON");
        assert_eq!(refusal["error"]["code"], "WORKFLOW_CAPACITY_EXCEEDED");
        let connection = read_only_db(root);
        let occupancy = connection
            .query_row_with_params(
                "SELECT actor, harness, session FROM capacity_occupancy WHERE issue_id = ?",
                &[first.clone().into()],
            )
            .expect("scoped occupancy");
        for (index, expected) in [ACTOR, "harness-one", "session-one"].iter().enumerate() {
            assert_eq!(
                occupancy
                    .get(index)
                    .and_then(beads_rust::franken_sync::SqliteValue::as_text),
                Some(*expected)
            );
        }
        let event = connection.query_row_with_params("SELECT agent_name, harness, model FROM events WHERE issue_id = ? AND event_type = 'status_changed' ORDER BY id DESC LIMIT 1", &[first.clone().into()]).expect("attributed event");
        for (index, expected) in ["scope-proof", "harness-one", "model-proof"]
            .iter()
            .enumerate()
        {
            assert_eq!(
                event
                    .get(index)
                    .and_then(beads_rust::franken_sync::SqliteValue::as_text),
                Some(*expected)
            );
        }
        connection.close().expect("close observer");
        let other = br_command(root)
            .env("BR_SESSION", "session-two")
            .args([
                "update",
                &second,
                "--status",
                "in_progress",
                "--actor",
                "another-actor",
                "--harness",
                "harness-two",
                "--json",
            ])
            .output()
            .expect("independent scope admission");
        assert!(other.status.success(), "{scope}: {other:?}");
        assert_eq!(
            cli_json(root, &["list", "--status", "in_progress"])["issues"]
                .as_array()
                .expect("active issues")
                .len(),
            2
        );
        let (status, stderr) = client.finish();
        assert!(
            status.success() || status.code() == Some(130),
            "{status}: {stderr}"
        );
    }
}

#[test]
fn running_mcp_server_observes_pending_merge_and_preserves_read_access() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "pending"]);
    let id = first_id(&cli_json(root, &["create", "Pending merge guard"]));
    let mut client = McpClient::spawn(root);
    assert_discovery_surface(&mut client);
    let authority = beads_rust::sync::blocking_write_lock(&root.join(".beads"))
        .expect("fixture writer authority");
    let mut storage = beads_rust::storage::SqliteStorage::open(&root.join(".beads/beads.db"))
        .expect("fixture writer");
    storage
        .set_metadata("sync_merge_pending_v1", "retained-legacy-receipt")
        .expect("persist real pending merge metadata");
    drop(storage);
    drop(authority);
    let before = policy_bookkeeping(root);
    let jsonl = std::fs::read(root.join(".beads/issues.jsonl")).expect("JSONL before");
    let error = client.tool_error(
        "update_issue",
        json!({"id": id, "title": "must not be written"}),
    );
    assert_eq!(error["data"]["error_type"], "SYNC_MERGE_PENDING", "{error}");
    assert_eq!(error["data"]["condition"], "legacy");
    assert_eq!(
        client.read_resource(&format!("beads://issue/{id}"))["title"],
        "Pending merge guard"
    );
    assert_eq!(policy_bookkeeping(root), before);
    assert_eq!(
        std::fs::read(root.join(".beads/issues.jsonl")).expect("JSONL after"),
        jsonl
    );
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}

#[test]
fn mcp_honors_cli_capacity_exemption_and_ordered_partial_batch_results() {
    let workspace = ProtocolWorkspace::new().expect("workspace");
    let root = workspace.path();
    cli_json(root, &["init", "--prefix", "exempt"]);
    let first = first_id(&cli_json(root, &["create", "Ordinary admission"]));
    let exempt = first_id(&cli_json(root, &["create", "Authorized exemption"]));
    let denied = first_id(&cli_json(root, &["create", "Full capacity refusal"]));
    std::fs::write(root.join(".beads/policy.yaml"), "workflow:\n  statuses: [open, in_progress, closed]\n  capacity:\n    statuses:\n      in_progress:\n        hard: 1\n    exemptions:\n      providers: [operator]\n").expect("exemption policy");
    let mut client = McpClient::spawn(root);
    assert_discovery_surface(&mut client);
    cli_json(
        root,
        &[
            "capacity",
            "exempt",
            &exempt,
            "--status",
            "in_progress",
            "--provider",
            "operator",
            "--reason",
            "Externally required exception",
            "--expires",
            "+7d",
        ],
    );
    let refused_before = client.read_resource(&format!("beads://issue/{denied}"));
    let result = client.call_tool(
        "update_issue",
        json!({"updates": [
            {"id": first, "status": "in_progress"},
            {"id": exempt, "status": "in_progress"},
            {"id": denied, "status": "in_progress"}
        ]}),
    );
    assert_eq!(result["count"], 3);
    assert_eq!(result["ok_count"], 2);
    assert_eq!(result["error_count"], 1);
    for (index, id) in [&first, &exempt, &denied].iter().enumerate() {
        assert_eq!(result["items"][index]["id"], **id);
        assert_eq!(result["items"][index]["index"], index);
    }
    assert_eq!(
        result["items"][2]["error"]["data"]["error_type"],
        "WORKFLOW_CAPACITY_EXCEEDED"
    );
    assert_ne!(
        result["items"][2]["error"]["data"]["mutation_committed"],
        true
    );
    assert_eq!(
        client.read_resource(&format!("beads://issue/{denied}")),
        refused_before
    );
    assert_eq!(
        cli_json(root, &["list", "--status", "in_progress"])["issues"]
            .as_array()
            .expect("admitted issues")
            .len(),
        2
    );
    let connection = read_only_db(root);
    assert!(
        connection
            .query("SELECT issue_id FROM dirty_issues")
            .expect("dirty metadata")
            .is_empty()
    );
    connection.close().expect("close observer");
    let (status, stderr) = client.finish();
    assert!(
        status.success() || status.code() == Some(130),
        "{status}: {stderr}"
    );
}
