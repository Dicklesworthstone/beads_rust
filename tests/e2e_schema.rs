//! E2E tests for the schema command.
//!
//! Validates that `br schema` works without an initialized workspace and
//! produces machine-parseable output in both JSON and TOON modes.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, parse_created_id, run_br, run_br_with_env};
use serde_json::Value;
#[cfg(feature = "self_update")]
use std::{fs, path::PathBuf};
#[cfg(feature = "self_update")]
use toon_rust::options::KeyFoldingMode;
use toon_rust::try_decode as parse_toon;
#[cfg(feature = "self_update")]
use toon_rust::{EncodeOptions, JsonValue};

#[cfg(feature = "self_update")]
const UPDATE_AGENT_BASELINE_ENV: &str = "UPDATE_AGENT_BASELINE";

#[test]
fn e2e_schema_vcs_shape_matches_live_success_and_error_streams() {
    let _log = common::test_log("e2e_schema_vcs_shape_matches_live_success_and_error_streams");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "schema_vcs_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let schemas = run_br(
        &workspace,
        ["schema", "commands", "--format", "json"],
        "schema_vcs_commands",
    );
    assert!(schemas.status.success(), "{}", schemas.stderr);
    let schemas: Value =
        serde_json::from_str(&extract_json_payload(&schemas.stdout)).expect("schema commands JSON");
    let shape = &schemas["commands"]["vcs-status"];
    assert_eq!(shape["shape"], "object", "{shape}");
    assert_eq!(shape["jq_filter"], ".", "{shape}");
    assert_eq!(shape["item_schema"], "VcsExportStatus", "{shape}");
    assert_eq!(
        shape["error_envelope_on_stderr"], false,
        "shape must describe the top-level CLI error stream: {shape}"
    );

    let success = run_br(
        &workspace,
        ["vcs-status", "--json"],
        "schema_vcs_live_success",
    );
    assert!(success.status.success(), "{}", success.stderr);
    let success: Value =
        serde_json::from_str(&extract_json_payload(&success.stdout)).expect("VCS success JSON");
    assert_eq!(success["schema"], "br.vcs-export-status.v2", "{success}");

    let error = run_br(
        &workspace,
        ["vcs-status", "--timeout-ms", "1", "--json"],
        "schema_vcs_live_error",
    );
    assert!(!error.status.success(), "invalid timeout must fail");
    let error: Value = serde_json::from_str(&extract_json_payload(&error.stdout))
        .expect("VCS error JSON on stdout");
    assert!(error.get("error").is_some(), "{error}");
}

#[test]
fn e2e_schema_json_issue() {
    let _log = common::test_log("e2e_schema_json_issue");
    let workspace = BrWorkspace::new();

    let run = run_br(
        &workspace,
        ["schema", "issue", "--format", "json"],
        "schema_issue_json",
    );
    assert!(
        run.status.success(),
        "schema issue json failed: {}",
        run.stderr
    );

    let payload = extract_json_payload(&run.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON output");

    assert_eq!(json["tool"], "br");
    assert!(json.get("generated_at").is_some(), "missing generated_at");
    assert!(json.get("schemas").is_some(), "missing schemas");
    assert!(
        json["schemas"].get("Issue").is_some(),
        "schemas should include Issue"
    );
}

#[test]
fn e2e_schema_all_includes_grouped_count_contract() {
    let _log = common::test_log("e2e_schema_all_includes_grouped_count_contract");
    let workspace = BrWorkspace::new();

    let run = run_br(
        &workspace,
        ["schema", "all", "--format", "json"],
        "schema_all_grouped_count_contract",
    );
    assert!(
        run.status.success(),
        "schema all json failed: {}",
        run.stderr
    );

    let payload = extract_json_payload(&run.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON output");

    assert!(
        json["schemas"].get("CountGroup").is_some(),
        "schemas should include CountGroup rows for grouped count output"
    );
    assert_eq!(
        json["commands"]["count"]["jq_filter"], ".count",
        "ungrouped count must keep the scalar .count contract"
    );
    assert_eq!(
        json["commands"]["count --by"]["jq_filter"], ".groups[]",
        "grouped count must expose the iterable groups row path"
    );
    assert_eq!(
        json["commands"]["count --by"]["items_at"], ".groups",
        "grouped count must document where rows live"
    );
    assert_eq!(
        json["commands"]["count --by"]["item_schema"], "CountGroup",
        "grouped count rows must reference the CountGroup schema"
    );
}

#[test]
fn e2e_schema_toon_decodes() {
    let _log = common::test_log("e2e_schema_toon_decodes");
    let workspace = BrWorkspace::new();

    let run = run_br(
        &workspace,
        ["schema", "issue-details", "--format", "toon"],
        "schema_issue_details_toon",
    );
    assert!(
        run.status.success(),
        "schema issue-details toon failed: {}",
        run.stderr
    );

    let toon = run.stdout.trim();
    assert!(!toon.is_empty(), "TOON output should be non-empty");

    let decoded = parse_toon(toon, None).expect("valid TOON");
    let json = Value::from(decoded);

    assert_eq!(json["tool"], "br");
    assert!(json.get("generated_at").is_some(), "missing generated_at");
    // TOON output uses key folding, so nested map keys may appear as dotted keys.
    let has_nested = json
        .get("schemas")
        .and_then(|schemas| schemas.get("IssueDetails"))
        .is_some();
    let has_folded = json.get("schemas.IssueDetails").is_some();
    assert!(
        has_nested || has_folded,
        "expected IssueDetails schema (nested or folded), got keys: {:?}",
        json.as_object().map(|o| o.keys().collect::<Vec<_>>())
    );
}

#[test]
fn e2e_capabilities_json_no_workspace() {
    let _log = common::test_log("e2e_capabilities_json_no_workspace");
    let workspace = BrWorkspace::new();

    let run = run_br(
        &workspace,
        ["capabilities", "--format", "json"],
        "capabilities_json",
    );
    assert!(
        run.status.success(),
        "capabilities json failed: {}",
        run.stderr
    );

    let payload = extract_json_payload(&run.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON output");

    assert_eq!(json["tool"], "br");
    assert_eq!(json["contract_version"], "br.capabilities.v1");
    assert!(
        json["features"].as_array().is_some_and(|features| {
            features
                .iter()
                .any(|feature| feature["name"] == "agent_machine_output")
        }),
        "missing agent_machine_output feature: {json}"
    );
    assert!(
        json["commands"].as_array().is_some_and(|commands| {
            commands
                .iter()
                .any(|command| command["name"] == "capabilities")
                && commands
                    .iter()
                    .any(|command| command["name"] == "robot-docs")
        }),
        "missing new agent commands: {json}"
    );
    assert!(
        json["exit_codes"].as_array().is_some_and(|codes| {
            codes
                .iter()
                .any(|code| code["code"] == 4 && code["category"] == "validation")
        }),
        "missing exit-code contract: {json}"
    );
}

#[test]
fn e2e_capabilities_command_detail_create_json() {
    let _log = common::test_log("e2e_capabilities_command_detail_create_json");
    let workspace = BrWorkspace::new();

    let run = run_br(
        &workspace,
        ["capabilities", "--format", "json", "--command", "create"],
        "capabilities_command_detail_create_json",
    );
    assert!(
        run.status.success(),
        "capabilities create detail failed: {}",
        run.stderr
    );

    let payload = extract_json_payload(&run.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON output");
    let detail = &json["command_detail"];

    assert_eq!(detail["path"], "create");
    assert_eq!(detail["operation"], "write");
    assert_eq!(detail["workspace"], "required");
    assert!(
        detail["arguments"].as_array().is_some_and(|arguments| {
            arguments
                .iter()
                .any(|argument| argument["long"] == "--slug")
                && arguments
                    .iter()
                    .any(|argument| argument["kind"] == "positional" && argument["id"] == "title")
        }),
        "missing create argument metadata: {detail}"
    );
}

#[test]
fn e2e_capabilities_command_detail_nested_alias_json() {
    let _log = common::test_log("e2e_capabilities_command_detail_nested_alias_json");
    let workspace = BrWorkspace::new();

    let run = run_br(
        &workspace,
        [
            "capabilities",
            "--format",
            "json",
            "--command",
            "comment add",
        ],
        "capabilities_command_detail_nested_alias_json",
    );
    assert!(
        run.status.success(),
        "capabilities nested detail failed: {}",
        run.stderr
    );

    let payload = extract_json_payload(&run.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON output");
    let detail = &json["command_detail"];

    assert_eq!(detail["path"], "comments add");
    assert_eq!(detail["operation"], "write");
    assert!(
        detail["examples"]
            .as_array()
            .is_some_and(|examples| examples.iter().any(|example| example
                .as_str()
                .is_some_and(|text| text.contains("br comments add")))),
        "missing nested command examples: {detail}"
    );
    assert!(
        detail["arguments"].as_array().is_some_and(|arguments| {
            arguments
                .iter()
                .any(|argument| argument["long"] == "--author")
                && arguments
                    .iter()
                    .any(|argument| argument["kind"] == "positional" && argument["id"] == "id")
        }),
        "missing nested command argument metadata: {detail}"
    );
}

#[test]
fn e2e_capabilities_command_detail_group_contracts_json() {
    let _log = common::test_log("e2e_capabilities_command_detail_group_contracts_json");
    let workspace = BrWorkspace::new();

    let dep_add = run_br(
        &workspace,
        ["capabilities", "--format", "json", "--command", "dep add"],
        "capabilities_command_detail_dep_add_json",
    );
    assert!(
        dep_add.status.success(),
        "capabilities dep add detail failed: {}",
        dep_add.stderr
    );
    let dep_add_payload = extract_json_payload(&dep_add.stdout);
    let dep_add_json: Value = serde_json::from_str(&dep_add_payload).expect("valid JSON output");
    let dep_add_detail = &dep_add_json["command_detail"];
    assert_eq!(dep_add_detail["operation"], "write");
    assert_eq!(dep_add_detail["workspace"], "required");
    assert!(
        dep_add_detail["examples"]
            .as_array()
            .is_some_and(|examples| examples.iter().any(|example| example
                .as_str()
                .is_some_and(|text| text.contains("br dep add")))),
        "missing dep add examples: {dep_add_detail}"
    );

    let query_run = run_br(
        &workspace,
        ["capabilities", "--format", "json", "--command", "query run"],
        "capabilities_command_detail_query_run_json",
    );
    assert!(
        query_run.status.success(),
        "capabilities query run detail failed: {}",
        query_run.stderr
    );
    let query_run_payload = extract_json_payload(&query_run.stdout);
    let query_run_json: Value =
        serde_json::from_str(&query_run_payload).expect("valid JSON output");
    let query_run_detail = &query_run_json["command_detail"];
    assert_eq!(query_run_detail["operation"], "read");
    assert!(
        query_run_detail["machine_output"]
            .as_array()
            .is_some_and(|formats| formats.iter().any(|format| format == "json")),
        "missing query run machine output formats: {query_run_detail}"
    );

    let dep_group = run_br(
        &workspace,
        ["capabilities", "--format", "json", "--command", "dep"],
        "capabilities_command_detail_dep_group_json",
    );
    assert!(
        dep_group.status.success(),
        "capabilities dep group detail failed: {}",
        dep_group.stderr
    );
    let dep_group_payload = extract_json_payload(&dep_group.stdout);
    let dep_group_json: Value =
        serde_json::from_str(&dep_group_payload).expect("valid JSON output");
    let dep_group_detail = &dep_group_json["command_detail"];
    assert_eq!(dep_group_detail["operation"], "mixed");
    assert!(
        dep_group_detail["examples"]
            .as_array()
            .is_some_and(|examples| examples.iter().any(|example| example
                .as_str()
                .is_some_and(|text| text.contains("br dep list")))),
        "missing dep parent examples: {dep_group_detail}"
    );
}

fn capabilities_command_detail_output(workspace: &BrWorkspace, command: &str) -> Value {
    let label = format!(
        "capabilities_command_detail_{}_json",
        command.replace(' ', "_")
    );
    let run = run_br(
        workspace,
        ["capabilities", "--format", "json", "--command", command],
        &label,
    );
    assert!(
        run.status.success(),
        "capabilities {command} detail failed: {}",
        run.stderr
    );
    let payload = extract_json_payload(&run.stdout);
    serde_json::from_str(&payload).expect("valid JSON output")
}

fn assert_array_text_contains(detail: &Value, field: &str, needle: &str, context: &str) {
    assert!(
        detail
            .get(field)
            .and_then(Value::as_array)
            .is_some_and(|values| values
                .iter()
                .any(|value| value.as_str().is_some_and(|text| text.contains(needle)))),
        "missing {context}: {detail}"
    );
}

fn assert_json_command_succeeds<const N: usize>(
    workspace: &BrWorkspace,
    args: [&str; N],
    label: &str,
) -> Value {
    let run = run_br(workspace, args, label);
    assert!(
        run.status.success(),
        "{label} JSON command failed: {}",
        run.stderr
    );
    let payload = extract_json_payload(&run.stdout);
    serde_json::from_str(&payload).expect("valid JSON output")
}

fn assert_toon_command_succeeds<const N: usize>(
    workspace: &BrWorkspace,
    args: [&str; N],
    label: &str,
) -> Value {
    let run = run_br_with_env(workspace, args, [("BR_OUTPUT_FORMAT", "toon")], label);
    assert!(
        run.status.success(),
        "{label} TOON command failed: {}",
        run.stderr
    );
    let toon = run.stdout.trim();
    assert!(!toon.is_empty(), "{label} TOON output should be non-empty");
    Value::from(parse_toon(toon, None).expect("valid TOON output"))
}

type CommandDetailCase = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

fn assert_command_detail_cases(workspace: &BrWorkspace, cases: &[CommandDetailCase]) {
    for (command, operation, field, needle, context) in cases {
        let output = capabilities_command_detail_output(workspace, command);
        let detail = output
            .get("command_detail")
            .expect("capabilities output should include command_detail");
        assert_eq!(
            detail.get("operation").and_then(Value::as_str),
            Some(*operation)
        );
        assert_array_text_contains(detail, field, needle, context);
    }
}

#[test]
fn e2e_capabilities_command_detail_high_traffic_safety_notes_json() {
    let _log = common::test_log("e2e_capabilities_command_detail_high_traffic_safety_notes_json");
    let workspace = BrWorkspace::new();

    let cases = [
        (
            "update",
            "write",
            "examples",
            "--add-label",
            "update task recipe examples",
        ),
        (
            "update",
            "write",
            "safety_notes",
            "last-touched",
            "update last-touched safety note",
        ),
        (
            "close",
            "write",
            "safety_notes",
            "--force",
            "close --force safety note",
        ),
        (
            "scheduler",
            "read",
            "examples",
            "--candidate-limit",
            "scheduler task recipe examples",
        ),
        (
            "scheduler",
            "read",
            "safety_notes",
            "does not claim",
            "scheduler read-only safety note",
        ),
        (
            "count",
            "read",
            "examples",
            "--by-label",
            "count grouped examples",
        ),
        (
            "search",
            "read",
            "safety_notes",
            "read-only",
            "search read-only safety note",
        ),
    ];

    assert_command_detail_cases(&workspace, &cases);
}

#[test]
fn e2e_capabilities_command_detail_machine_output_contracts_json() {
    let _log = common::test_log("e2e_capabilities_command_detail_machine_output_contracts_json");
    let workspace = BrWorkspace::new();

    let cases = [
        (
            "create",
            "write",
            "machine_output",
            "toon",
            "create TOON contract",
        ),
        (
            "q",
            "write",
            "machine_output",
            "json",
            "quick create JSON contract",
        ),
        (
            "comments add",
            "write",
            "machine_output",
            "toon",
            "comment add TOON contract",
        ),
        (
            "dep add",
            "write",
            "machine_output",
            "toon",
            "dependency add TOON contract",
        ),
        (
            "query save",
            "write",
            "machine_output",
            "toon",
            "query save TOON contract",
        ),
        (
            "query list",
            "read",
            "machine_output",
            "toon",
            "query list TOON contract",
        ),
        (
            "epic status",
            "read",
            "machine_output",
            "toon",
            "epic status TOON contract",
        ),
        (
            "count",
            "read",
            "machine_output",
            "toon",
            "count TOON contract",
        ),
        (
            "graph",
            "mixed",
            "machine_output",
            "toon",
            "graph TOON contract",
        ),
    ];

    assert_command_detail_cases(&workspace, &cases);

    let output = capabilities_command_detail_output(&workspace, "query save");
    let detail = output
        .get("command_detail")
        .expect("capabilities output should include command_detail");
    assert_array_text_excludes(detail, "machine_output", "csv", "query save");

    let output = capabilities_command_detail_output(&workspace, "config");
    let detail = output
        .get("command_detail")
        .expect("capabilities output should include command_detail");
    assert_array_text_contains(detail, "machine_output", "json", "config");
    assert_array_text_excludes(detail, "machine_output", "toon", "config");

    let output = capabilities_command_detail_output(&workspace, "upgrade");
    let detail = output
        .get("command_detail")
        .expect("capabilities output should include command_detail");
    assert_array_text_contains(detail, "machine_output", "json", "upgrade");
    assert_array_text_excludes(detail, "machine_output", "toon", "upgrade");
}

#[test]
fn e2e_capabilities_machine_output_contracts_execute_representative_modes() {
    let _log =
        common::test_log("e2e_capabilities_machine_output_contracts_execute_representative_modes");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init", "--prefix", "br"], "cap_runtime_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create_issue = run_br(
        &workspace,
        ["create", "Runtime contract issue"],
        "cap_runtime_create_issue",
    );
    assert!(
        create_issue.status.success(),
        "create issue failed: {}",
        create_issue.stderr
    );
    let issue_id = parse_created_id(&create_issue.stdout);
    assert!(!issue_id.is_empty(), "created issue id missing");

    let create_blocker = run_br(
        &workspace,
        ["create", "Runtime contract blocker"],
        "cap_runtime_create_blocker",
    );
    assert!(
        create_blocker.status.success(),
        "create blocker failed: {}",
        create_blocker.stderr
    );
    let blocker_id = parse_created_id(&create_blocker.stdout);
    assert!(!blocker_id.is_empty(), "created blocker id missing");

    assert_json_command_succeeds(
        &workspace,
        ["--json", "q", "Runtime contract quick issue"],
        "cap_runtime_q_json",
    );
    assert_toon_command_succeeds(
        &workspace,
        [
            "comments",
            "add",
            issue_id.as_str(),
            "--message",
            "Runtime note",
        ],
        "cap_runtime_comments_add_toon",
    );
    assert_toon_command_succeeds(
        &workspace,
        [
            "dep",
            "add",
            issue_id.as_str(),
            blocker_id.as_str(),
            "--type",
            "blocks",
        ],
        "cap_runtime_dep_add_toon",
    );
    assert_toon_command_succeeds(
        &workspace,
        [
            "label",
            "add",
            issue_id.as_str(),
            "--label",
            "runtime-contract",
        ],
        "cap_runtime_label_add_toon",
    );
    assert_toon_command_succeeds(
        &workspace,
        ["query", "save", "open-runtime", "--status", "open"],
        "cap_runtime_query_save_toon",
    );
    assert_toon_command_succeeds(&workspace, ["query", "list"], "cap_runtime_query_list_toon");
    assert_toon_command_succeeds(
        &workspace,
        ["epic", "status"],
        "cap_runtime_epic_status_toon",
    );
    assert_toon_command_succeeds(
        &workspace,
        ["count", "--by", "status"],
        "cap_runtime_count_toon",
    );
    assert_toon_command_succeeds(&workspace, ["graph", "--all"], "cap_runtime_graph_toon");
}

#[test]
fn e2e_capabilities_command_detail_dependency_safety_notes_json() {
    let _log = common::test_log("e2e_capabilities_command_detail_dependency_safety_notes_json");
    let workspace = BrWorkspace::new();

    let cases = [
        (
            "dep add",
            "write",
            "safety_notes",
            "waits on",
            "dependency argument-order note",
        ),
        (
            "dep list",
            "read",
            "examples",
            "--direction both",
            "dependency list direction recipe",
        ),
        (
            "dep cycles",
            "read",
            "safety_notes",
            "--blocking-only",
            "dependency cycles planning note",
        ),
        (
            "dep cycles",
            "read",
            "examples",
            "--json",
            "dependency cycles machine-output recipe",
        ),
        (
            "dep cycles",
            "read",
            "examples",
            "BR_OUTPUT_FORMAT=toon",
            "dependency cycles TOON env recipe",
        ),
        (
            "dep tree",
            "read",
            "examples",
            "--json",
            "dependency tree machine-output recipe",
        ),
        (
            "dep tree",
            "read",
            "examples",
            "BR_OUTPUT_FORMAT=toon",
            "dependency tree TOON env recipe",
        ),
        (
            "dep tree",
            "read",
            "safety_notes",
            "local `--format` selects text or mermaid",
            "dependency tree local format note",
        ),
    ];

    assert_command_detail_cases(&workspace, &cases);

    for command in ["dep cycles", "dep tree"] {
        let output = capabilities_command_detail_output(&workspace, command);
        let detail = output
            .get("command_detail")
            .expect("capabilities output should include command_detail");
        assert_array_text_excludes(detail, "examples", "--format json", command);
        assert_array_text_excludes(detail, "examples", "--format toon", command);
    }
}

#[test]
fn e2e_capabilities_command_detail_workflow_safety_notes_json() {
    let _log = common::test_log("e2e_capabilities_command_detail_workflow_safety_notes_json");
    let workspace = BrWorkspace::new();

    let cases = [
        (
            "create",
            "write",
            "examples",
            "--slug",
            "create slug task recipe",
        ),
        (
            "create",
            "write",
            "safety_notes",
            "--file",
            "create import safety note",
        ),
        (
            "comments add",
            "write",
            "safety_notes",
            "--message",
            "comment text-source safety note",
        ),
        (
            "comments list",
            "read",
            "safety_notes",
            "read-only",
            "comment list read-only note",
        ),
        (
            "query save",
            "write",
            "safety_notes",
            "already exists",
            "query save replacement note",
        ),
        (
            "query run",
            "read",
            "examples",
            "--status open",
            "query run override recipe",
        ),
        (
            "query delete",
            "write",
            "safety_notes",
            "never deletes issues",
            "query delete scope note",
        ),
    ];

    assert_command_detail_cases(&workspace, &cases);
}

fn assert_array_text_excludes(detail: &Value, field: &str, needle: &str, context: &str) {
    assert!(
        detail
            .get(field)
            .and_then(Value::as_array)
            .is_some_and(|values| values
                .iter()
                .all(|value| value.as_str().is_none_or(|text| !text.contains(needle)))),
        "unexpected {context} {field} entry containing {needle}: {detail}"
    );
}

#[test]
fn e2e_robot_docs_guide_text_is_concise() {
    let _log = common::test_log("e2e_robot_docs_guide_text_is_concise");
    let workspace = BrWorkspace::new();

    let run = run_br(&workspace, ["robot-docs", "guide"], "robot_docs_guide_text");
    assert!(
        run.status.success(),
        "robot-docs guide failed: {}",
        run.stderr
    );

    let lines = run.stdout.lines().count();
    assert!(lines <= 80, "guide should stay concise, got {lines} lines");
    assert!(run.stdout.contains("br capabilities --format json"));
    assert!(run.stdout.contains("br ready --json"));
    assert!(run.stdout.contains("br never runs git"));
}

#[test]
fn e2e_robot_docs_guide_json_no_workspace() {
    let _log = common::test_log("e2e_robot_docs_guide_json_no_workspace");
    let workspace = BrWorkspace::new();

    let run = run_br(
        &workspace,
        ["robot-docs", "guide", "--format", "json"],
        "robot_docs_guide_json",
    );
    assert!(
        run.status.success(),
        "robot-docs guide json failed: {}",
        run.stderr
    );

    let payload = extract_json_payload(&run.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON output");

    assert_eq!(json["tool"], "br");
    assert_eq!(json["contract_version"], "br.robot_docs.v1");
    assert!(
        json["line_count"].as_u64().is_some_and(|count| count <= 80),
        "guide should report <=80 lines: {json}"
    );
    assert!(
        json["canonical_commands"]
            .as_array()
            .is_some_and(|commands| {
                commands
                    .iter()
                    .any(|command| command["command"] == "br coordination status --json")
            }),
        "missing canonical coordination command: {json}"
    );
}

#[cfg(feature = "self_update")]
#[test]
fn agent_baseline_snapshots_match_current_binary() {
    let _log = common::test_log("agent_baseline_snapshots_match_current_binary");
    let workspace = BrWorkspace::new();

    compare_agent_baseline_help(&workspace);
    compare_agent_baseline_schemas(&workspace);
    let id_two = seed_agent_baseline_workspace(&workspace);
    compare_agent_baseline_examples(&workspace, &id_two);
    compare_agent_baseline_error(&workspace);
}

#[cfg(feature = "self_update")]
fn compare_agent_baseline_help(workspace: &BrWorkspace) {
    compare_text_baseline(
        "help/br_help.txt",
        &run_success(workspace, ["--help"], "baseline_help"),
    );
    compare_text_baseline(
        "help/br_list_help.txt",
        &run_success(workspace, ["list", "--help"], "baseline_list_help"),
    );
    compare_text_baseline(
        "help/br_schema_help.txt",
        &run_success(workspace, ["schema", "--help"], "baseline_schema_help"),
    );
}

#[cfg(feature = "self_update")]
fn compare_agent_baseline_schemas(workspace: &BrWorkspace) {
    compare_json_baseline(
        "schemas/schema_all.json",
        &run_success(
            workspace,
            ["schema", "all", "--format", "json"],
            "baseline_schema_all",
        ),
        normalize_schema_snapshot,
    );
    compare_json_baseline(
        "schemas/schema_error.json",
        &run_success(
            workspace,
            ["schema", "error", "--format", "json"],
            "baseline_schema_error",
        ),
        normalize_schema_snapshot,
    );
    compare_json_baseline(
        "schemas/schema_issue_details.json",
        &run_success(
            workspace,
            ["schema", "issue-details", "--format", "json"],
            "baseline_schema_issue_details",
        ),
        normalize_schema_snapshot,
    );
}

#[cfg(feature = "self_update")]
fn seed_agent_baseline_workspace(workspace: &BrWorkspace) -> String {
    let init = run_br(workspace, ["init", "--prefix", "bd"], "baseline_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create_one = run_br(
        workspace,
        [
            "create",
            "One",
            "--type",
            "task",
            "--priority",
            "2",
            "--description",
            "Short desc",
        ],
        "baseline_create_one",
    );
    assert!(
        create_one.status.success(),
        "create one failed: {}",
        create_one.stderr
    );
    let create_two = run_br(
        workspace,
        ["create", "Two", "--type", "bug", "--priority", "0"],
        "baseline_create_two",
    );
    assert!(
        create_two.status.success(),
        "create two failed: {}",
        create_two.stderr
    );
    let id_two = parse_created_id(&create_two.stdout);
    let create_three = run_br(
        workspace,
        ["create", "Three", "--type", "feature", "--priority", "1"],
        "baseline_create_three",
    );
    assert!(
        create_three.status.success(),
        "create three failed: {}",
        create_three.stderr
    );
    id_two
}

#[cfg(feature = "self_update")]
fn compare_agent_baseline_examples(workspace: &BrWorkspace, id_two: &str) {
    compare_json_baseline(
        "examples/list_limit3.json",
        &run_success(
            workspace,
            ["list", "--format", "json", "--limit", "3"],
            "baseline_list_limit3_json",
        ),
        normalize_issue_example_snapshot,
    );
    compare_toon_baseline(
        "examples/list_limit3.toon",
        &run_success(
            workspace,
            ["list", "--format", "toon", "--limit", "3"],
            "baseline_list_limit3_toon",
        ),
    );
    compare_json_baseline(
        "examples/ready.json",
        &run_success(
            workspace,
            ["ready", "--format", "json"],
            "baseline_ready_json",
        ),
        normalize_issue_example_snapshot,
    );
    compare_toon_baseline(
        "examples/ready.toon",
        &run_success(
            workspace,
            ["ready", "--format", "toon"],
            "baseline_ready_toon",
        ),
    );
    compare_json_baseline(
        "examples/show_one.json",
        &run_success(
            workspace,
            ["show", id_two, "--format", "json"],
            "baseline_show_one_json",
        ),
        normalize_issue_example_snapshot,
    );
    compare_toon_baseline(
        "examples/show_one.toon",
        &run_success(
            workspace,
            ["show", id_two, "--format", "toon"],
            "baseline_show_one_toon",
        ),
    );
    compare_json_baseline(
        "examples/version.json",
        &run_success(workspace, ["version", "--json"], "baseline_version_json"),
        normalize_version_snapshot,
    );
}

#[cfg(feature = "self_update")]
fn compare_agent_baseline_error(workspace: &BrWorkspace) {
    let missing = run_br(
        workspace,
        ["show", "bd-NOTEXIST", "--json"],
        "baseline_show_not_found",
    );
    assert_eq!(
        missing.status.code(),
        Some(3),
        "unexpected status: {missing:?}"
    );
    compare_json_baseline(
        "errors/show_not_found.json",
        &missing.stdout,
        normalize_noop,
    );
}

#[cfg(feature = "self_update")]
fn run_success<const N: usize>(workspace: &BrWorkspace, args: [&str; N], label: &str) -> String {
    let run = run_br(workspace, args, label);
    assert!(
        run.status.success(),
        "{label} failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    run.stdout
}

#[cfg(feature = "self_update")]
fn compare_text_baseline(relative_path: &str, actual: &str) {
    let path = baseline_path(relative_path);
    let actual = normalize_text_snapshot(actual);
    if should_update_agent_baseline() {
        fs::write(&path, actual).expect("update agent baseline text snapshot");
        return;
    }

    let expected = fs::read_to_string(&path).expect("read agent baseline text snapshot");
    let expected = normalize_text_snapshot(&expected);
    assert_eq!(
        expected, actual,
        "agent_baseline/{relative_path} is stale; rerun with {UPDATE_AGENT_BASELINE_ENV}=1"
    );
}

#[cfg(feature = "self_update")]
fn compare_json_baseline(relative_path: &str, actual: &str, normalize: fn(&mut Value)) {
    let path = baseline_path(relative_path);
    let actual_payload = extract_json_payload(actual);
    let mut actual: Value =
        serde_json::from_str(&actual_payload).expect("valid generated JSON for agent baseline");
    normalize(&mut actual);

    if should_update_agent_baseline() {
        let pretty = serde_json::to_string_pretty(&actual)
            .expect("serialize normalized agent baseline JSON snapshot");
        fs::write(&path, with_trailing_newline(&pretty))
            .expect("update agent baseline JSON snapshot");
        return;
    }

    let expected_raw = fs::read_to_string(&path).expect("read agent baseline JSON snapshot");
    let mut expected: Value =
        serde_json::from_str(&expected_raw).expect("valid agent baseline JSON snapshot");
    normalize(&mut expected);

    assert_eq!(
        expected, actual,
        "agent_baseline/{relative_path} is stale; rerun with {UPDATE_AGENT_BASELINE_ENV}=1"
    );
}

#[cfg(feature = "self_update")]
fn compare_toon_baseline(relative_path: &str, actual: &str) {
    let path = baseline_path(relative_path);
    let actual = actual.trim_end();
    if should_update_agent_baseline() {
        let mut normalized =
            Value::from(parse_toon(actual, None).expect("valid generated TOON for agent baseline"));
        normalize_issue_example_snapshot(&mut normalized);
        let toon_value: JsonValue = normalized.into();
        let encoded = toon_rust::encode(toon_value, Some(agent_baseline_toon_encode_options()));
        fs::write(&path, with_trailing_newline(encoded.trim_end()))
            .expect("update agent baseline TOON snapshot");
        return;
    }

    let expected_raw = fs::read_to_string(&path).expect("read agent baseline TOON snapshot");
    let mut expected =
        Value::from(parse_toon(&expected_raw, None).expect("valid agent baseline TOON snapshot"));
    let mut actual =
        Value::from(parse_toon(actual, None).expect("valid generated TOON for agent baseline"));
    normalize_issue_example_snapshot(&mut expected);
    normalize_issue_example_snapshot(&mut actual);

    assert_eq!(
        expected, actual,
        "agent_baseline/{relative_path} is stale; rerun with {UPDATE_AGENT_BASELINE_ENV}=1"
    );
}

#[cfg(feature = "self_update")]
#[must_use]
fn agent_baseline_toon_encode_options() -> EncodeOptions {
    EncodeOptions {
        indent: Some(2),
        delimiter: None,
        key_folding: Some(KeyFoldingMode::Safe),
        flatten_depth: None,
        replacer: None,
    }
}

#[cfg(feature = "self_update")]
fn baseline_path(relative_path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("agent_baseline")
        .join(relative_path)
}

#[cfg(feature = "self_update")]
fn should_update_agent_baseline() -> bool {
    std::env::var_os(UPDATE_AGENT_BASELINE_ENV).is_some()
}

#[cfg(feature = "self_update")]
fn with_trailing_newline(text: &str) -> String {
    format!("{text}\n")
}

#[cfg(feature = "self_update")]
fn normalize_text_snapshot(text: &str) -> String {
    let mut normalized = text
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    normalized.push('\n');
    normalized
}

#[cfg(feature = "self_update")]
fn normalize_schema_snapshot(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "generated_at".to_string(),
            Value::String("<GENERATED_AT>".to_string()),
        );
    }
}

#[cfg(feature = "self_update")]
fn normalize_version_snapshot(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("branch");
        object.remove("commit");
        for key in ["build", "rust_version", "target"] {
            if object.contains_key(key) {
                object.insert(key.to_string(), Value::String(format!("<{key}>")));
            }
        }
    }
}

#[cfg(feature = "self_update")]
fn normalize_issue_example_snapshot(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                normalize_issue_example_snapshot(item);
            }
        }
        Value::Object(object) => {
            for (key, child) in object {
                match key.as_str() {
                    "closed_at" | "created_at" | "updated_at" => {
                        *child = Value::String("<TIMESTAMP>".to_string());
                    }
                    "created_by" => {
                        *child = Value::String("<ACTOR>".to_string());
                    }
                    "source_repo" | "source_repo_path" => {
                        *child = Value::String("<SOURCE_REPO>".to_string());
                    }
                    "depends_on_id" | "id" | "issue_id" => {
                        *child = Value::String("<ISSUE_ID>".to_string());
                    }
                    _ => normalize_issue_example_snapshot(child),
                }
            }
        }
        Value::Bool(_) | Value::Null | Value::Number(_) | Value::String(_) => {}
    }
}

#[cfg(feature = "self_update")]
fn normalize_noop(_: &mut Value) {}
