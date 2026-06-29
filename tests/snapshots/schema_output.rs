use super::common::cli::{BrWorkspace, run_br};
use insta::assert_snapshot;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::LazyLock;

// Full schema-document goldens for agent integration surfaces.
//
// Golden update workflow:
// INSTA_UPDATE=always rch exec -- cargo test --test snapshots schema_document_golden
//
// Review the JSON and TOON snapshots together. These tests normalize only the
// top-level generated_at value; schema names, key order, field definitions,
// descriptions, and TOON structure are intentionally frozen for review.
const EXPECTED_SCHEMA_NAMES: &[&str] = &[
    "BlockedIssue",
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
    "TreeNode",
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
    let result = toon_rust::try_decode(raw, None);
    let error = result.as_ref().err().map(ToString::to_string);
    assert_eq!(None, error, "{context} did not emit valid TOON\n\n{raw}");
    let decoded = result.expect("valid TOON after assertion");
    Value::from(decoded)
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
