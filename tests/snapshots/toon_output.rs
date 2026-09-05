use super::common::cli::{BrRun, run_br};
use super::init_workspace;
use insta::assert_snapshot;
use regex::Regex;
use serde_json::Value;
use std::fs;
use std::sync::LazyLock;

fn strict_toon(run: &BrRun) -> Value {
    assert!(run.status.success(), "TOON command failed: {run:?}");
    assert!(!run.stdout.contains('\u{1b}'), "ANSI in TOON: {run:?}");
    Value::from(
        toon_rust::try_decode(&run.stdout, None)
            .unwrap_or_else(|error| panic!("whole TOON stdout must decode: {error}; {run:?}")),
    )
}

const TOON_JSONL_FIXTURE: &str = r#"{"id":"bd-blocker","title":"00 Blocking Root","description":"Unblocks dependent work","status":"open","priority":0,"issue_type":"task","created_at":"2026-02-01T00:00:00Z","created_by":"fixture","updated_at":"2026-02-01T00:00:00Z","source_repo":".","labels":["core"],"compaction_level":0,"original_size":0}
{"id":"bd-ready-p0","title":"01 Ready Critical Unassigned","status":"open","priority":0,"issue_type":"bug","created_at":"2026-02-02T00:00:00Z","created_by":"fixture","updated_at":"2026-02-02T00:00:00Z","source_repo":".","labels":["ops","agent"],"compaction_level":0,"original_size":0}
{"id":"bd-ready-p1-assigned","title":"02 Ready Assigned Feature","status":"open","priority":1,"issue_type":"feature","assignee":"alice","owner":"owner@example.com","created_at":"2026-02-03T00:00:00Z","created_by":"fixture","updated_at":"2026-02-03T00:00:00Z","source_repo":".","labels":["frontend"],"compaction_level":0,"original_size":0}
{"id":"bd-blocked","title":"03 Blocked By Root","status":"open","priority":1,"issue_type":"task","created_at":"2026-02-05T00:00:00Z","created_by":"fixture","updated_at":"2026-02-05T00:00:00Z","source_repo":".","labels":["blocked"],"dependencies":[{"issue_id":"bd-blocked","depends_on_id":"bd-blocker","type":"blocks","created_at":"2026-02-05T00:00:00Z","created_by":"fixture","metadata":"{}","thread_id":""}],"compaction_level":0,"original_size":0}
{"id":"bd-closed","title":"04 Closed Done","status":"closed","priority":2,"issue_type":"task","created_at":"2026-02-08T00:00:00Z","created_by":"fixture","updated_at":"2026-02-08T00:00:00Z","closed_at":"2026-02-08T01:00:00Z","close_reason":"done","source_repo":".","labels":["done"],"compaction_level":0,"original_size":0}
"#;

static TOON_GENERATED_AT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^generated_at:\s*.+$").expect("toon generated_at regex"));

fn init_toon_workspace() -> super::common::cli::BrWorkspace {
    let workspace = init_workspace();
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    fs::write(jsonl_path, TOON_JSONL_FIXTURE).expect("write TOON JSONL fixture");

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "toon_golden_import",
    );
    assert!(
        import.status.success(),
        "fixture import failed:\nstdout:\n{}\nstderr:\n{}",
        import.stdout,
        import.stderr
    );

    workspace
}

fn normalize_toon_output(raw: &str) -> String {
    let trimmed = raw.trim_end();
    TOON_GENERATED_AT_RE
        .replace_all(trimmed, "generated_at: GENERATED_AT")
        .to_string()
}

#[test]
fn toon_golden_list_output() {
    let workspace = init_toon_workspace();

    let output = run_br(
        &workspace,
        ["list", "--all", "--sort", "title", "--format", "toon"],
        "toon_golden_list",
    );
    assert!(
        output.status.success(),
        "list --format toon failed: {}",
        output.stderr
    );
    let decoded = strict_toon(&output);
    assert_eq!(decoded["issues"].as_array().expect("list issues").len(), 5);
    // The first fixture record sorts first by title; its imported ID must round-trip.
    let first_fixture_issue: Value = serde_json::from_str(
        TOON_JSONL_FIXTURE
            .lines()
            .next()
            .expect("first fixture issue"),
    )
    .expect("valid fixture issue");
    assert_eq!(decoded["issues"][0]["id"], first_fixture_issue["id"]);

    let normalized = normalize_toon_output(&output.stdout);
    assert_snapshot!("toon_list_output", normalized);
}

#[test]
fn toon_golden_show_output() {
    let workspace = init_toon_workspace();

    let output = run_br(
        &workspace,
        ["show", "bd-ready-p0", "--format", "toon"],
        "toon_golden_show",
    );
    assert!(
        output.status.success(),
        "show --format toon failed: {}",
        output.stderr
    );
    let decoded = strict_toon(&output);
    assert_eq!(decoded.as_array().expect("show issues").len(), 1);
    assert_eq!(decoded[0]["id"], "bd-ready-p0");

    let normalized = normalize_toon_output(&output.stdout);
    assert_snapshot!("toon_show_output", normalized);
}

#[test]
fn toon_golden_ready_output() {
    let workspace = init_toon_workspace();

    let output = run_br(
        &workspace,
        [
            "ready", "--sort", "priority", "--limit", "0", "--format", "toon",
        ],
        "toon_golden_ready",
    );
    assert!(
        output.status.success(),
        "ready --format toon failed: {}",
        output.stderr
    );
    let decoded = strict_toon(&output);
    let ids: Vec<_> = decoded
        .as_array()
        .expect("ready issues")
        .iter()
        .map(|issue| issue["id"].as_str().expect("ready id"))
        .collect();
    assert_eq!(ids, ["bd-blocker", "bd-ready-p0", "bd-ready-p1-assigned"]);

    let normalized = normalize_toon_output(&output.stdout);
    assert_snapshot!("toon_ready_output", normalized);
}
