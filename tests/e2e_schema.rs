//! E2E tests for the schema command.
//!
//! Validates that `br schema` works without an initialized workspace and
//! produces machine-parseable output in both JSON and TOON modes.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

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

    let decoded = toon_rust::try_decode(toon, None).expect("valid TOON");
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
