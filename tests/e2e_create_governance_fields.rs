//! E2E coverage for issue #408: `br create` accepts `--acceptance-criteria`
//! (visible alias `--acceptance`) and `--agent-context` so a fully governed
//! issue is created in ONE atomic mutation instead of create + update.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, parse_created_id, run_br};
use serde_json::Value;
use std::fs;

fn show_issue(workspace: &BrWorkspace, id: &str) -> Value {
    let show = run_br(
        workspace,
        ["show", id, "--json", "--no-auto-flush", "--no-auto-import"],
        "show_issue",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    // `br show --json` emits an array of issues.
    let mut issues: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show JSON");
    assert_eq!(issues.len(), 1, "expected exactly one issue for {id}");
    issues.remove(0)
}

fn jsonl_record_for(workspace: &BrWorkspace, id: &str) -> Value {
    let jsonl = fs::read_to_string(workspace.root.join(".beads").join("issues.jsonl"))
        .expect("read issues.jsonl");
    let line = jsonl
        .lines()
        .find(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|record| record.get("id").and_then(Value::as_str).map(String::from))
                .as_deref()
                == Some(id)
        })
        .unwrap_or_else(|| panic!("no JSONL record for {id}; jsonl:\n{jsonl}"));
    serde_json::from_str(line).expect("jsonl record")
}

/// Both fields persist through the single create transaction, appear in the
/// command's JSON output, in `br show`, and in the first flushed JSONL record.
#[test]
fn e2e_create_with_acceptance_criteria_and_agent_context() {
    let _log = common::test_log("e2e_create_with_acceptance_criteria_and_agent_context");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        [
            "create",
            "Governed issue",
            "--acceptance-criteria",
            "- [ ] guard refuses\n- [ ] test passes",
            "--agent-context",
            r#"{"workflow":"tdd","reviewer":"agent"}"#,
            "--json",
            "--no-auto-import",
        ],
        "create_governed",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create JSON");
    let id = created["id"].as_str().expect("created id").to_string();
    assert_eq!(
        created["acceptance_criteria"].as_str(),
        Some("- [ ] guard refuses\n- [ ] test passes"),
        "create JSON output must carry acceptance criteria"
    );
    let context_out = created["agent_context"]
        .as_str()
        .expect("agent_context in create JSON");
    let context_json: Value = serde_json::from_str(context_out).expect("context is JSON");
    assert_eq!(context_json["workflow"].as_str(), Some("tdd"));

    // SQLite row via show.
    let shown = show_issue(&workspace, &id);
    assert_eq!(
        shown["acceptance_criteria"].as_str(),
        Some("- [ ] guard refuses\n- [ ] test passes")
    );
    let shown_context: Value =
        serde_json::from_str(shown["agent_context"].as_str().expect("context")).expect("JSON");
    assert_eq!(shown_context["reviewer"].as_str(), Some("agent"));

    // First flushed JSONL record (auto-flush ran on create).
    let record = jsonl_record_for(&workspace, &id);
    assert_eq!(
        record["acceptance_criteria"].as_str(),
        Some("- [ ] guard refuses\n- [ ] test passes")
    );
    assert!(
        record["agent_context"].as_str().is_some(),
        "flushed JSONL record must carry agent_context; record: {record}"
    );
}

/// `--acceptance` is a visible alias with identical behavior, matching update.
#[test]
fn e2e_create_acceptance_alias() {
    let _log = common::test_log("e2e_create_acceptance_alias");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        [
            "create",
            "Alias issue",
            "--acceptance",
            "- [ ] alias works",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "create_alias",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);
    let shown = show_issue(&workspace, &id);
    assert_eq!(
        shown["acceptance_criteria"].as_str(),
        Some("- [ ] alias works")
    );
}

/// `--agent-context @file.json` and `@file.yaml` normalize exactly like
/// `br update --agent-context`.
#[test]
fn e2e_create_agent_context_file_forms() {
    let _log = common::test_log("e2e_create_agent_context_file_forms");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let json_path = workspace.root.join("ctx.json");
    fs::write(&json_path, r#"{"workflow":"json-file","steps":[1,2]}"#).expect("write json");
    let yaml_path = workspace.root.join("ctx.yaml");
    fs::write(&yaml_path, "workflow: yaml-file\nsteps:\n  - a\n  - b\n").expect("write yaml");

    let create_json = run_br(
        &workspace,
        [
            "create",
            "From json file",
            "--agent-context",
            &format!("@{}", json_path.display()),
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "create_ctx_json",
    );
    assert!(
        create_json.status.success(),
        "create failed: {}",
        create_json.stderr
    );
    let id_json = parse_created_id(&create_json.stdout);
    let ctx_json: Value = serde_json::from_str(
        show_issue(&workspace, &id_json)["agent_context"]
            .as_str()
            .expect("context"),
    )
    .expect("JSON");
    assert_eq!(ctx_json["workflow"].as_str(), Some("json-file"));

    let create_yaml = run_br(
        &workspace,
        [
            "create",
            "From yaml file",
            "--agent-context",
            &format!("@{}", yaml_path.display()),
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "create_ctx_yaml",
    );
    assert!(
        create_yaml.status.success(),
        "create failed: {}",
        create_yaml.stderr
    );
    let id_yaml = parse_created_id(&create_yaml.stdout);
    let ctx_yaml: Value = serde_json::from_str(
        show_issue(&workspace, &id_yaml)["agent_context"]
            .as_str()
            .expect("context"),
    )
    .expect("JSON");
    assert_eq!(ctx_yaml["workflow"].as_str(), Some("yaml-file"));
    assert_eq!(ctx_yaml["steps"][1].as_str(), Some("b"));

    // Update with the same YAML file must produce the identical normalized
    // context (parser parity between create and update).
    let update = run_br(
        &workspace,
        [
            "update",
            &id_json,
            "--agent-context",
            &format!("@{}", yaml_path.display()),
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "update_ctx_yaml",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    assert_eq!(
        show_issue(&workspace, &id_json)["agent_context"].as_str(),
        show_issue(&workspace, &id_yaml)["agent_context"].as_str(),
        "create and update must normalize identical YAML identically"
    );
}

/// Invalid inline context fails BEFORE any mutation: nonzero exit, no issue,
/// no dirty marker, no JSONL record.
#[test]
fn e2e_create_invalid_agent_context_leaves_no_trace() {
    let _log = common::test_log("e2e_create_invalid_agent_context_leaves_no_trace");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        [
            "create",
            "Broken context",
            "--agent-context",
            "{not json",
            "--no-auto-import",
        ],
        "create_invalid_ctx",
    );
    assert!(
        !create.status.success(),
        "invalid agent context must fail; stdout: {}",
        create.stdout
    );
    assert!(
        create.stderr.contains("agent-context"),
        "error must name the offending flag; stderr: {}",
        create.stderr
    );

    let list = run_br(
        &workspace,
        [
            "list",
            "--all",
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "list_after_invalid",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);
    assert!(
        !list.stdout.contains("Broken context"),
        "no issue may exist after a rejected create; list: {}",
        list.stdout
    );

    let jsonl =
        fs::read_to_string(workspace.root.join(".beads").join("issues.jsonl")).unwrap_or_default();
    assert!(
        !jsonl.contains("Broken context"),
        "no JSONL record may exist after a rejected create"
    );

    let status = run_br(
        &workspace,
        [
            "sync",
            "--status",
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "sync_status_after_invalid",
    );
    assert!(status.status.success(), "status failed: {}", status.stderr);
    let status_json: Value =
        serde_json::from_str(&extract_json_payload(&status.stdout)).expect("status JSON");
    assert_eq!(
        status_json["dirty_issues"].as_u64().unwrap_or(0),
        0,
        "rejected create must not leave dirty markers; status: {status_json}"
    );
}

/// Plain create without the new flags keeps current behavior (both NULL).
#[test]
fn e2e_create_without_governance_flags_unchanged() {
    let _log = common::test_log("e2e_create_without_governance_flags_unchanged");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        [
            "create",
            "Plain issue",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "create_plain",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);
    let shown = show_issue(&workspace, &id);
    assert!(shown["acceptance_criteria"].is_null());
    assert!(shown["agent_context"].is_null());
}
