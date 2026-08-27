use super::common::cli::{BrWorkspace, run_br};
use super::{create_issue, init_workspace, normalize_json};
use insta::assert_snapshot;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::LazyLock;
use toon_rust::options::{DecodeOptions, ExpandPathsMode};

// Full schema-document goldens for agent integration surfaces.
//
// Golden update workflow:
// INSTA_UPDATE=always rch exec -- cargo test --test snapshots schema_document_golden
//
// Review the JSON and TOON snapshots together. These tests normalize only the
// top-level generated_at value; schema names, key order, field definitions,
// descriptions, and TOON structure are intentionally frozen for review.
const EXPECTED_SCHEMA_NAMES: &[&str] = &[
    "AdditiveReconcileReceipt",
    "BlockedIssue",
    "BlockedPage",
    "CoordinationClaimRow",
    "CoordinationStatusOutput",
    "CountGroup",
    "ErrorEnvelope",
    "Issue",
    "IssueDetails",
    "IssueWithCounts",
    "ReadyIssue",
    "StaleIssue",
    "Statistics",
    "SyncReconcileReceipt",
    "TreeNode",
    "VcsExportStatus",
];

static JSON_GENERATED_AT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""generated_at"\s*:\s*"[^"]+""#).expect("generated_at regex"));
static TOON_GENERATED_AT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^generated_at:\s*.+$").expect("toon generated_at regex"));

fn normalize_schema_json_output(raw: &str) -> String {
    let trimmed = raw.trim_end();
    let normalized = JSON_GENERATED_AT_RE
        .replace(trimmed, r#""generated_at": "GENERATED_AT""#)
        .to_string();
    assert_ne!(
        trimmed, normalized,
        "schema JSON output did not contain generated_at"
    );
    normalized
}

fn normalize_schema_toon_output(raw: &str) -> String {
    let trimmed = raw.trim_end();
    let normalized = TOON_GENERATED_AT_RE
        .replace(trimmed, r#"generated_at: "GENERATED_AT""#)
        .to_string();
    assert_ne!(
        trimmed, normalized,
        "schema TOON output did not contain generated_at"
    );
    normalized
}

fn parse_json(raw: &str, context: &str) -> Value {
    let result = serde_json::from_str(raw);
    let error = result.as_ref().err().map(ToString::to_string);
    assert_eq!(None, error, "{context} did not emit valid JSON\n\n{raw}");
    result.expect("valid JSON after assertion")
}

fn parse_toon(raw: &str, context: &str) -> Value {
    // ubs:ignore - this decodes TOON snapshot text, not JWTs or credentials.
    let result = toon_rust::try_decode(raw, Some(safe_toon_decode_options()));
    let error = result.as_ref().err().map(ToString::to_string);
    assert_eq!(None, error, "{context} did not emit valid TOON\n\n{raw}");
    let decoded = result.expect("valid TOON after assertion");
    Value::from(decoded)
}

fn safe_toon_decode_options() -> DecodeOptions {
    DecodeOptions {
        indent: Some(2),
        strict: Some(true),
        expand_paths: Some(ExpandPathsMode::Safe),
    }
}

fn schema_value<'a>(document: &'a Value, schema_name: &str) -> Option<&'a Value> {
    document
        .get("schemas")
        .and_then(|schemas| schemas.get(schema_name))
        .or_else(|| document.get(format!("schemas.{schema_name}")))
}

fn command_shape_value<'a>(document: &'a Value, command_name: &str) -> Option<&'a Value> {
    document
        .get("commands")
        .and_then(|commands| commands.get(command_name))
        .or_else(|| document.get(format!("commands.{command_name}")))
}

fn schema_names(document: &Value) -> BTreeSet<String> {
    let mut names = BTreeSet::new();

    if let Some(schemas) = document.get("schemas").and_then(Value::as_object) {
        names.extend(schemas.keys().cloned());
    }

    if let Some(object) = document.as_object() {
        for key in object.keys() {
            if let Some(rest) = key.strip_prefix("schemas.")
                && let Some(name) = rest.split('.').next()
            {
                names.insert(name.to_string());
            }
        }
    }

    names
}

fn expected_schema_names() -> BTreeSet<String> {
    EXPECTED_SCHEMA_NAMES
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn assert_command_shape_schema_ref(
    expected_names: &BTreeSet<String>,
    command_name: &str,
    shape: &Value,
    context: &str,
) {
    let Some(item_schema) = shape.get("item_schema").and_then(Value::as_str) else {
        return;
    };

    assert!(
        expected_names.contains(item_schema),
        "{context} command {command_name:?} references missing item_schema {item_schema:?}"
    );
}

fn assert_command_item_schemas_resolve(document: &Value, context: &str) {
    let expected_names = schema_names(document);
    let mut command_count = 0usize;

    if let Some(commands) = document.get("commands").and_then(Value::as_object) {
        for (command_name, shape) in commands {
            command_count += 1;
            assert_command_shape_schema_ref(&expected_names, command_name, shape, context);
        }
    }

    if let Some(object) = document.as_object() {
        for (key, shape) in object {
            if let Some(command_name) = key.strip_prefix("commands.")
                && shape.is_object()
            {
                command_count += 1;
                assert_command_shape_schema_ref(&expected_names, command_name, shape, context);
            }
        }
    }

    assert!(command_count > 0, "{context} should include command shapes");
}

fn assert_coordination_contract_coverage(document: &Value, context: &str) {
    for schema_name in ["CoordinationStatusOutput", "CoordinationClaimRow"] {
        assert!(
            schema_value(document, schema_name).is_some(),
            "{context} missing required coordination schema {schema_name}"
        );
    }

    let coordination = command_shape_value(document, "coordination status");
    assert!(
        coordination.is_some(),
        "{context} missing coordination status command shape"
    );
    let coordination = coordination.expect("coordination status command shape asserted present");
    assert_eq!(
        coordination.get("jq_filter").and_then(Value::as_str),
        Some(".claims[]"),
        "{context} coordination status jq_filter drifted"
    );
    assert_eq!(
        coordination.get("items_at").and_then(Value::as_str),
        Some(".claims"),
        "{context} coordination status items_at drifted"
    );
    assert_eq!(
        coordination.get("item_schema").and_then(Value::as_str),
        Some("CoordinationClaimRow"),
        "{context} coordination status item_schema must stay linked to claims rows"
    );
}

fn assert_schema_document_shape(document: &Value, context: &str) {
    assert_eq!(document["tool"], "br", "{context} should identify br");
    assert_eq!(
        document["generated_at"], "GENERATED_AT",
        "{context} should have normalized generated_at"
    );
    assert_eq!(
        schema_names(document),
        expected_schema_names(),
        "{context} schema target set changed"
    );
    assert_command_item_schemas_resolve(document, context);
    assert_coordination_contract_coverage(document, context);
}

fn assert_toon_matches_json_schema_metadata(json: &Value, toon: &Value) {
    for schema_name in EXPECTED_SCHEMA_NAMES {
        let json_schema = schema_value(json, schema_name);
        assert!(
            json_schema.is_some(),
            "JSON output missing {schema_name} schema"
        );
        let json_schema = json_schema.expect("JSON schema present after assertion");

        let toon_schema = schema_value(toon, schema_name);
        assert!(
            toon_schema.is_some(),
            "TOON output missing {schema_name} schema"
        );
        let toon_schema = toon_schema.expect("TOON schema present after assertion");

        for key in ["$schema", "title", "type"] {
            assert_eq!(
                json_schema.get(key),
                toon_schema.get(key),
                "TOON {schema_name}.{key} metadata diverged from JSON"
            );
        }
    }
}

fn normalize_number_for_semantic_comparison(number: &serde_json::Number) -> Value {
    let text = number.to_string();
    let normalized = text.strip_suffix(".0").unwrap_or(&text);
    let mut marker = serde_json::Map::new();
    marker.insert(
        "__semantic_number".to_string(),
        Value::String(normalized.to_string()),
    );
    Value::Object(marker)
}

fn normalize_semantic_output(value: &Value) -> Value {
    let normalized = normalize_json(value);
    normalize_semantic_output_inner(&normalized)
}

fn normalize_semantic_output_inner(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut normalized = serde_json::Map::new();
            for (key, value) in map {
                let normalized_value = match key.as_str() {
                    "generated_at" => Value::String("GENERATED_AT".to_string()),
                    _ => normalize_semantic_output_inner(value),
                };
                normalized.insert(key.clone(), normalized_value);
            }
            Value::Object(normalized)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(normalize_semantic_output_inner).collect())
        }
        Value::Number(number) => normalize_number_for_semantic_comparison(number),
        other => other.clone(),
    }
}

struct LiveCommandCase<'a> {
    name: &'a str,
    args: Vec<&'a str>,
    require_extracted_items: bool,
}

struct SemanticParityCase<'a> {
    name: &'a str,
    json_args: Vec<&'a str>,
    toon_args: Vec<&'a str>,
}

struct LiveCommandFixture {
    workspace: BrWorkspace,
    ready_id: String,
    blocked_id: String,
    blocker_id: String,
    in_progress_id: String,
    closed_id: String,
}

fn run_success(workspace: &BrWorkspace, args: &[&str], label: &str) {
    let output = run_br(workspace, args, label);
    assert!(
        output.status.success(),
        "{label} failed for args {args:?}\nstdout:\n{}\nstderr:\n{}",
        output.stdout,
        output.stderr
    );
}

fn create_live_command_fixture() -> LiveCommandFixture {
    let workspace = init_workspace();

    let ready_id = create_issue(
        &workspace,
        "Ready contract fixture for schema filters",
        "schema_contract_create_ready",
    );
    let blocked_id = create_issue(
        &workspace,
        "Blocked contract fixture for schema filters",
        "schema_contract_create_blocked",
    );
    let blocker_id = create_issue(
        &workspace,
        "Blocker contract fixture for schema filters",
        "schema_contract_create_blocker",
    );
    let in_progress_id = create_issue(
        &workspace,
        "In progress contract fixture for schema filters",
        "schema_contract_create_in_progress",
    );
    let closed_id = create_issue(
        &workspace,
        "Closed contract fixture for schema filters",
        "schema_contract_create_closed",
    );

    run_success(
        &workspace,
        &[
            "update",
            &ready_id,
            "--add-label",
            "schema-contract",
            "--json",
        ],
        "schema_contract_label_ready",
    );
    run_success(
        &workspace,
        &[
            "update",
            &blocked_id,
            "--add-label",
            "schema-blocked",
            "--json",
        ],
        "schema_contract_label_blocked",
    );
    run_success(
        &workspace,
        &[
            "comments",
            "add",
            &ready_id,
            "--author",
            "schema-contract-test",
            "--message",
            "contract comment",
            "--json",
        ],
        "schema_contract_comment_ready",
    );
    run_success(
        &workspace,
        &["dep", "add", &blocked_id, &blocker_id, "--json"],
        "schema_contract_dep_add",
    );
    run_success(
        &workspace,
        &[
            "update",
            &in_progress_id,
            "--status",
            "in_progress",
            "--assignee",
            "schema-contract-agent",
            "--json",
        ],
        "schema_contract_update_in_progress",
    );
    run_success(
        &workspace,
        &[
            "close",
            &closed_id,
            "--reason",
            "schema contract fixture",
            "--json",
        ],
        "schema_contract_close_issue",
    );

    LiveCommandFixture {
        workspace,
        ready_id,
        blocked_id,
        blocker_id,
        in_progress_id,
        closed_id,
    }
}

fn live_command_cases(fixture: &LiveCommandFixture) -> Vec<LiveCommandCase<'_>> {
    vec![
        LiveCommandCase {
            name: "blocked",
            args: vec!["blocked", "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "capabilities",
            args: vec!["capabilities", "--json"],
            require_extracted_items: false,
        },
        LiveCommandCase {
            name: "comments list",
            args: vec!["comments", "list", &fixture.ready_id, "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "coordination status",
            args: vec!["coordination", "status", "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "count",
            args: vec!["count", "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "count --by",
            args: vec!["count", "--by", "status", "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "dep list",
            args: vec!["dep", "list", &fixture.blocked_id, "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "dep tree",
            args: vec!["dep", "tree", &fixture.blocked_id, "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "info",
            args: vec!["info", "--json"],
            require_extracted_items: false,
        },
        LiveCommandCase {
            name: "label list",
            args: vec!["label", "list", "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "list",
            args: vec!["list", "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "ready",
            args: vec!["ready", "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "robot-docs guide",
            args: vec!["robot-docs", "guide", "--json"],
            require_extracted_items: false,
        },
        LiveCommandCase {
            name: "search",
            // The search envelope is stable whether or not this fixture hides
            // closed matches; `--all` also exercises the zero-count case.
            args: vec!["search", "contract fixture", "--all", "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "show",
            args: vec!["show", &fixture.ready_id, "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "stale",
            args: vec!["stale", "--days", "0", "--json"],
            require_extracted_items: true,
        },
        LiveCommandCase {
            name: "stats",
            args: vec!["stats", "--json"],
            require_extracted_items: false,
        },
        LiveCommandCase {
            name: "status",
            args: vec!["status", "--json"],
            require_extracted_items: false,
        },
    ]
}

fn semantic_parity_cases(fixture: &LiveCommandFixture) -> Vec<SemanticParityCase<'_>> {
    vec![
        SemanticParityCase {
            name: "list",
            json_args: vec!["list", "--json"],
            toon_args: vec!["list", "--format", "toon"],
        },
        SemanticParityCase {
            name: "show",
            json_args: vec!["show", &fixture.ready_id, "--json"],
            toon_args: vec!["show", &fixture.ready_id, "--format", "toon"],
        },
        SemanticParityCase {
            name: "ready",
            json_args: vec!["ready", "--json"],
            toon_args: vec!["ready", "--format", "toon"],
        },
        SemanticParityCase {
            name: "coordination status",
            json_args: vec!["coordination", "status", "--json"],
            toon_args: vec!["coordination", "status", "--format", "toon"],
        },
        SemanticParityCase {
            name: "schema all",
            json_args: vec!["schema", "all", "--format", "json"],
            toon_args: vec!["schema", "all", "--format", "toon"],
        },
    ]
}

fn assert_json_toon_semantic_parity(
    workspace: &BrWorkspace,
    case: &SemanticParityCase<'_>,
) -> (Value, Value) {
    let label = case.name.replace(' ', "_");
    let json_output = run_br(
        workspace,
        case.json_args.iter().copied(),
        &format!("semantic_parity_{label}_json"),
    );
    assert!(
        json_output.status.success(),
        "JSON command {:?} failed for args {:?}\nstdout:\n{}\nstderr:\n{}",
        case.name,
        case.json_args,
        json_output.stdout,
        json_output.stderr
    );
    let json = parse_json(&json_output.stdout, case.name);

    let toon_output = run_br(
        workspace,
        case.toon_args.iter().copied(),
        &format!("semantic_parity_{label}_toon"),
    );
    assert!(
        toon_output.status.success(),
        "TOON command {:?} failed for args {:?}\nstdout:\n{}\nstderr:\n{}",
        case.name,
        case.toon_args,
        toon_output.stdout,
        toon_output.stderr
    );
    let toon = parse_toon(&toon_output.stdout, case.name);

    assert_eq!(
        normalize_semantic_output(&json),
        normalize_semantic_output(&toon),
        "command {:?} JSON and TOON semantic trees diverged\nJSON stdout:\n{}\nTOON stdout:\n{}",
        case.name,
        json_output.stdout,
        toon_output.stdout
    );

    (json, toon)
}

fn assert_live_output_shape(command_name: &str, shape: &Value, output: &Value) {
    let expected_shape = shape
        .get("shape")
        .and_then(Value::as_str)
        .unwrap_or("<missing>");

    let matched = match expected_shape {
        "array" => output.is_array(),
        "object" => output.is_object(),
        "scalar" => !(output.is_array() || output.is_object()),
        _ => false,
    };

    assert!(
        matched,
        "command {command_name:?} expected top-level shape {expected_shape:?}; observed output: {output}"
    );
}

fn evaluate_schema_path<'a>(
    root: &'a Value,
    path: &str,
    command_name: &str,
    path_kind: &str,
) -> Result<Vec<&'a Value>, String> {
    let Some(mut remaining) = path.strip_prefix('.') else {
        return Err(format!(
            "command {command_name:?} {path_kind} {path:?} must start with `.`; observed output: {root}"
        ));
    };
    let mut current = vec![root];

    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix("[]") {
            let mut expanded = Vec::new();
            for value in current {
                let Some(items) = value.as_array() else {
                    return Err(format!(
                        "command {command_name:?} {path_kind} {path:?} expected array before [] segment; observed output: {root}"
                    ));
                };
                expanded.extend(items);
            }
            current = expanded;
            remaining = rest;
            continue;
        }

        if let Some(rest) = remaining.strip_prefix("[0]") {
            let mut indexed = Vec::new();
            for value in current {
                let Some(first) = value.get(0) else {
                    return Err(format!(
                        "command {command_name:?} {path_kind} {path:?} expected index 0; observed output: {root}"
                    ));
                };
                indexed.push(first);
            }
            current = indexed;
            remaining = rest;
            continue;
        }

        let field_start = remaining.strip_prefix('.').unwrap_or(remaining);
        let field_end = field_start.find(['.', '[']).unwrap_or(field_start.len());
        let Some(field) = field_start.get(..field_end) else {
            return Err(format!(
                "command {command_name:?} {path_kind} {path:?} could not slice field segment; observed output: {root}"
            ));
        };
        if field.is_empty() {
            return Err(format!(
                "command {command_name:?} {path_kind} {path:?} has an empty field segment; observed output: {root}"
            ));
        }
        let mut field_values = Vec::new();
        for value in current {
            let Some(field_value) = value.get(field) else {
                return Err(format!(
                    "command {command_name:?} {path_kind} {path:?} missing field {field:?}; observed output: {root}"
                ));
            };
            field_values.push(field_value);
        }
        current = field_values;
        let Some(rest) = field_start.get(field_end..) else {
            return Err(format!(
                "command {command_name:?} {path_kind} {path:?} could not slice remaining path; observed output: {root}"
            ));
        };
        remaining = rest;
    }

    Ok(current)
}

fn assert_items_path_matches_jq_filter(
    command_name: &str,
    shape: &Value,
    output: &Value,
    jq_values: &[&Value],
) {
    let Some(items_at) = shape.get("items_at").and_then(Value::as_str) else {
        return;
    };

    let item_containers = match evaluate_schema_path(output, items_at, command_name, "items_at") {
        Ok(values) => values,
        Err(message) => {
            panic!("{message}");
        }
    };
    assert_eq!(
        item_containers.len(),
        1,
        "command {command_name:?} items_at {items_at:?} should resolve to one container; observed output: {output}"
    );
    let Some(container) = item_containers.first() else {
        panic!(
            "command {command_name:?} items_at {items_at:?} should resolve to one container; observed output: {output}"
        );
    };
    let Some(items) = container.as_array() else {
        panic!(
            "command {command_name:?} items_at {items_at:?} should resolve to an array; observed output: {output}",
        );
    };

    assert_eq!(
        items.len(),
        jq_values.len(),
        "command {command_name:?} items_at {items_at:?} disagrees with jq_filter {:?}; observed output: {output}",
        shape
            .get("jq_filter")
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
    );
}

fn required_schema_fields(schema: &Value) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|required| required.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn assert_extracted_items_match_schema(
    command_name: &str,
    shape: &Value,
    schema_document: &Value,
    jq_values: &[&Value],
    output: &Value,
) {
    let Some(schema_name) = shape.get("item_schema").and_then(Value::as_str) else {
        return;
    };
    let Some(schema) = schema_value(schema_document, schema_name) else {
        panic!("command {command_name:?} references missing schema {schema_name:?}");
    };
    let required_fields = required_schema_fields(schema);

    for item in jq_values {
        let Some(object) = item.as_object() else {
            panic!(
                "command {command_name:?} expected extracted {schema_name} item to be an object; jq_filter {:?}; observed output: {output}",
                shape
                    .get("jq_filter")
                    .and_then(Value::as_str)
                    .unwrap_or("<missing>")
            );
        };

        for field in &required_fields {
            assert!(
                object.contains_key(*field),
                "command {command_name:?} extracted item is missing required {schema_name}.{field}; jq_filter {:?}; observed output: {output}",
                shape
                    .get("jq_filter")
                    .and_then(Value::as_str)
                    .unwrap_or("<missing>")
            );
        }
    }
}

fn assert_live_command_matches_schema(
    case: &LiveCommandCase<'_>,
    shape: &Value,
    schema_document: &Value,
    output: &Value,
) {
    assert_live_output_shape(case.name, shape, output);

    let jq_filter = shape
        .get("jq_filter")
        .and_then(Value::as_str)
        .unwrap_or("<missing>");
    let jq_values = match evaluate_schema_path(output, jq_filter, case.name, "jq_filter") {
        Ok(values) => values,
        Err(message) => {
            panic!("{message}");
        }
    };
    assert!(
        !case.require_extracted_items || !jq_values.is_empty(),
        "command {:?} jq_filter {jq_filter:?} extracted no items; items_at {:?}; observed output: {output}",
        case.name,
        shape.get("items_at").and_then(Value::as_str)
    );
    assert_items_path_matches_jq_filter(case.name, shape, output, &jq_values);
    assert_extracted_items_match_schema(case.name, shape, schema_document, &jq_values, output);
}

#[test]
fn schema_document_golden_json_all() {
    let workspace = BrWorkspace::new();

    let output = run_br(
        &workspace,
        ["schema", "all", "--format", "json"],
        "schema_all_json_golden",
    );
    assert!(
        output.status.success(),
        "schema all --format json failed: {}",
        output.stderr
    );

    let normalized = normalize_schema_json_output(&output.stdout);
    let json = parse_json(&normalized, "schema all --format json");
    assert_schema_document_shape(&json, "schema all JSON");
    assert_snapshot!("schema_all_json_output", normalized);
}

#[test]
fn schema_document_golden_toon_all() {
    let workspace = BrWorkspace::new();

    let json_output = run_br(
        &workspace,
        ["schema", "all", "--format", "json"],
        "schema_all_json_for_toon_golden",
    );
    assert!(
        json_output.status.success(),
        "schema all --format json failed: {}",
        json_output.stderr
    );
    let normalized_json = normalize_schema_json_output(&json_output.stdout);
    let json = parse_json(&normalized_json, "schema all --format json");

    let toon_output = run_br(
        &workspace,
        ["schema", "all", "--format", "toon"],
        "schema_all_toon_golden",
    );
    assert!(
        toon_output.status.success(),
        "schema all --format toon failed: {}",
        toon_output.stderr
    );

    let normalized_toon = normalize_schema_toon_output(&toon_output.stdout);
    let toon = parse_toon(&normalized_toon, "schema all --format toon");
    assert_schema_document_shape(&toon, "schema all TOON");
    assert_toon_matches_json_schema_metadata(&json, &toon);
    assert_snapshot!("schema_all_toon_output", normalized_toon);
}

#[test]
fn schema_command_shapes_match_live_json_outputs() {
    let fixture = create_live_command_fixture();
    let schema_output = run_br(
        &fixture.workspace,
        ["schema", "all", "--format", "json"],
        "schema_contract_schema_all",
    );
    assert!(
        schema_output.status.success(),
        "schema all --format json failed: {}",
        schema_output.stderr
    );
    let schema_document = parse_json(&schema_output.stdout, "schema all --format json");

    for case in live_command_cases(&fixture) {
        let output = run_br(&fixture.workspace, case.args.iter().copied(), case.name);
        assert!(
            output.status.success(),
            "command {:?} failed for args {:?}\nstdout:\n{}\nstderr:\n{}",
            case.name,
            case.args,
            output.stdout,
            output.stderr
        );

        let json = parse_json(&output.stdout, case.name);
        let Some(shape) = command_shape_value(&schema_document, case.name) else {
            panic!("schema all missing command shape for {:?}", case.name);
        };
        assert_live_command_matches_schema(&case, shape, &schema_document, &json);
    }

    for required_fixture_id in [
        &fixture.ready_id,
        &fixture.blocked_id,
        &fixture.blocker_id,
        &fixture.in_progress_id,
        &fixture.closed_id,
    ] {
        assert!(
            !required_fixture_id.is_empty(),
            "fixture id should be populated"
        );
    }
}

#[test]
fn agent_json_and_toon_outputs_match_semantically() {
    // Update workflow: when an agent-facing JSON or TOON envelope changes,
    // update the command implementation first, run this semantic parity test,
    // then refresh visual snapshots only after this decoded-tree check is
    // green. TOON must decode with safe path expansion because br emits safe
    // folded keys for token efficiency.
    let fixture = create_live_command_fixture();

    for case in semantic_parity_cases(&fixture) {
        let (json, toon) = assert_json_toon_semantic_parity(&fixture.workspace, &case);

        if case.name == "coordination status" {
            assert_eq!(
                json.pointer("/claims/0/assessment/reservation/state")
                    .and_then(Value::as_str),
                Some("no_snapshot"),
                "coordination JSON fixture should exercise reservation.state"
            );
            assert_eq!(
                toon.pointer("/claims/0/assessment/reservation/state")
                    .and_then(Value::as_str),
                Some("no_snapshot"),
                "coordination TOON must expand assessment.reservation.state safely"
            );
        }
    }
}
