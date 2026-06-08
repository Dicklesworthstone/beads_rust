use super::common::cli::{BrWorkspace, run_br};
use super::init_workspace;
use insta::assert_snapshot;
use regex::Regex;
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::process::{ExitStatus, Stdio};
use std::sync::LazyLock;

// Representative fixture for exact robot-output goldens.
//
// Golden update workflow:
// INSTA_UPDATE=always rch exec -- cargo test --test snapshots robot_golden
//
// Review the resulting tests/snapshots/snapshots/*.snap diffs before
// committing. The br ready fixture is fully deterministic and unmasked. The bv
// outputs are normalized only for wall-clock metadata, elapsed timings,
// external bv version/reporting hints, stale-day wording, and score fields that
// include current-date urgency.
const ROBOT_JSONL_FIXTURE: &str = r#"{"id":"bd-blocker","title":"00 Blocking Root","description":"Unblocks dependent work","status":"open","priority":0,"issue_type":"task","created_at":"2026-02-01T00:00:00Z","created_by":"fixture","updated_at":"2026-02-01T00:00:00Z","source_repo":".","labels":["core"],"compaction_level":0,"original_size":0}
{"id":"bd-ready-p0","title":"01 Ready Critical Unassigned","status":"open","priority":0,"issue_type":"bug","created_at":"2026-02-02T00:00:00Z","created_by":"fixture","updated_at":"2026-02-02T00:00:00Z","source_repo":".","labels":["ops","agent"],"compaction_level":0,"original_size":0}
{"id":"bd-ready-p1-assigned","title":"02 Ready Assigned Feature","status":"open","priority":1,"issue_type":"feature","assignee":"alice","owner":"owner@example.com","created_at":"2026-02-03T00:00:00Z","created_by":"fixture","updated_at":"2026-02-03T00:00:00Z","source_repo":".","labels":["frontend"],"compaction_level":0,"original_size":0}
{"id":"bd-ready-p2-label","title":"03 Ready Backend Task","status":"open","priority":2,"issue_type":"task","created_at":"2026-02-04T00:00:00Z","created_by":"fixture","updated_at":"2026-02-04T00:00:00Z","source_repo":".","labels":["backend"],"compaction_level":0,"original_size":0}
{"id":"bd-blocked","title":"04 Blocked By Root","status":"open","priority":1,"issue_type":"task","created_at":"2026-02-05T00:00:00Z","created_by":"fixture","updated_at":"2026-02-05T00:00:00Z","source_repo":".","labels":["blocked"],"dependencies":[{"issue_id":"bd-blocked","depends_on_id":"bd-blocker","type":"blocks","created_at":"2026-02-05T00:00:00Z","created_by":"fixture","metadata":"{}","thread_id":""}],"compaction_level":0,"original_size":0}
{"id":"bd-deferred","title":"05 Deferred Ready Later","status":"deferred","priority":1,"issue_type":"task","defer_until":"2026-09-01T00:00:00Z","created_at":"2026-02-06T00:00:00Z","created_by":"fixture","updated_at":"2026-02-06T00:00:00Z","source_repo":".","labels":["waiting"],"compaction_level":0,"original_size":0}
{"id":"bd-in-progress","title":"06 In Progress Assigned","status":"in_progress","priority":0,"issue_type":"task","assignee":"bob","created_at":"2026-02-07T00:00:00Z","created_by":"fixture","updated_at":"2026-02-07T00:00:00Z","source_repo":".","labels":["active"],"compaction_level":0,"original_size":0}
{"id":"bd-closed","title":"07 Closed Done","status":"closed","priority":2,"issue_type":"task","created_at":"2026-02-08T00:00:00Z","created_by":"fixture","updated_at":"2026-02-08T00:00:00Z","closed_at":"2026-02-08T01:00:00Z","close_reason":"done","source_repo":".","labels":["done"],"compaction_level":0,"original_size":0}
"#;

static GENERATED_AT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""generated_at"\s*:\s*"[^"]+""#).expect("generated_at regex"));
static WEEK_START_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""week_start"\s*:\s*"[^"]+""#).expect("week_start regex"));
static ELAPSED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(compute_time_ms|ms)"\s*:\s*[0-9]+(?:\.[0-9]+)?"#).expect("elapsed regex")
});
static SCORE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(score|urgency|urgency_norm)"\s*:\s*-?[0-9]+(?:\.[0-9]+)?"#)
        .expect("score regex")
});
static BV_VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""version"\s*:\s*"v\d+\.\d+\.\d+(?:[-+][^"]*)?""#).expect("bv version regex")
});
static GRAPH_ROOT_FIRST_USAGE_HINT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#""usage_hints"\s*:\s*\[\s*"--graph-root (?:\\u003c|<)id(?:\\u003e|>) - [^"]+",\s*"#,
    )
    .expect("first graph-root usage hint regex")
});
static GRAPH_ROOT_ONLY_USAGE_HINT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#""usage_hints"\s*:\s*\[\s*"--graph-root (?:\\u003c|<)id(?:\\u003e|>) - [^"]+"\s*\]"#,
    )
    .expect("only graph-root usage hint regex")
});
static GRAPH_ROOT_LATER_USAGE_HINT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#",\s*"--graph-root (?:\\u003c|<)id(?:\\u003e|>) - [^"]+""#)
        .expect("later graph-root usage hint regex")
});
static STALE_DAYS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"No activity in \d+ days").expect("stale days regex"));
static AGING_DAYS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"aging \(\d+ days\)").expect("aging days regex"));

#[derive(Debug)]
struct BvRun {
    stdout: String,
    stderr: String,
    status: ExitStatus,
}

fn init_robot_golden_workspace() -> BrWorkspace {
    let workspace = init_workspace();
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    fs::write(jsonl_path, ROBOT_JSONL_FIXTURE).expect("write robot JSONL fixture");

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "robot_golden_import",
    );
    assert!(
        import.status.success(),
        "robot fixture import failed:\nstdout:\n{}\nstderr:\n{}",
        import.stdout,
        import.stderr
    );

    workspace
}

fn clear_inherited_br_env(command: &mut std::process::Command) {
    for (key, _) in std::env::vars_os() {
        let key_str = key.to_string_lossy();
        if key_str.starts_with("BD_")
            || key_str.starts_with("BEADS_")
            || matches!(
                key_str.as_ref(),
                "BR_OUTPUT_FORMAT" | "TOON_DEFAULT_FORMAT" | "TOON_STATS"
            )
        {
            command.env_remove(key);
        }
    }
}

fn run_bv<I, S>(workspace: &BrWorkspace, args: I) -> BvRun
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = std::process::Command::new("bv");
    command.current_dir(&workspace.root);
    command.args(args);
    clear_inherited_br_env(&mut command);
    command.env("NO_COLOR", "1");
    command.env("CI", "1");

    let output = command
        .output()
        .expect("run bv; install bv to update robot goldens");
    BvRun {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        status: output.status,
    }
}

fn require_bv(test_name: &str) -> bool {
    let available = std::process::Command::new("bv")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();

    if !available {
        eprintln!("[skip] {test_name}: missing bv CLI; install bv to run robot bv goldens");
    }

    available
}

fn assert_valid_json(raw: &str, context: &str) {
    let error = serde_json::from_str::<Value>(raw)
        .err()
        .map(|err| format!("{context} did not emit valid JSON: {err}\n\n{raw}"));
    assert_eq!(None, error);
}

fn normalize_bv_usage_hints(raw: &str) -> String {
    let normalized = GRAPH_ROOT_ONLY_USAGE_HINT_RE
        .replace_all(raw, r#""usage_hints":[]"#)
        .to_string();
    let normalized = GRAPH_ROOT_FIRST_USAGE_HINT_RE
        .replace_all(&normalized, r#""usage_hints":["#)
        .to_string();
    GRAPH_ROOT_LATER_USAGE_HINT_RE
        .replace_all(&normalized, "")
        .to_string()
}

fn normalize_bv_robot_output(raw: &str) -> String {
    let mut normalized = raw.trim_end().to_string();
    normalized = GENERATED_AT_RE
        .replace_all(&normalized, r#""generated_at": "TIMESTAMP""#)
        .to_string();
    normalized = WEEK_START_RE
        .replace_all(&normalized, r#""week_start": "WEEK_START""#)
        .to_string();
    normalized = ELAPSED_RE
        .replace_all(&normalized, |captures: &regex::Captures<'_>| {
            format!(r#""{}": 0.0"#, &captures[1])
        })
        .to_string();
    normalized = SCORE_RE
        .replace_all(&normalized, |captures: &regex::Captures<'_>| {
            format!(r#""{}": 0.0"#, &captures[1])
        })
        .to_string();
    normalized = BV_VERSION_RE
        .replace_all(&normalized, r#""version":"BV_VERSION""#)
        .to_string();
    normalized = normalize_bv_usage_hints(&normalized);
    normalized = STALE_DAYS_RE
        .replace_all(&normalized, "No activity in DAYS days")
        .to_string();
    AGING_DAYS_RE
        .replace_all(&normalized, "aging (DAYS days)")
        .to_string()
}

#[test]
fn normalize_bv_usage_hints_removes_graph_root_hint_in_any_array_position() {
    let graph_root = r#""--graph-root \u003cid\u003e - Scope triage""#;
    let cases = [
        (
            format!(r#"{{"usage_hints":[{graph_root}]}}"#),
            r#"{"usage_hints":[]}"#,
        ),
        (
            format!(r#"{{"usage_hints":[{graph_root},"keep"]}}"#),
            r#"{"usage_hints":["keep"]}"#,
        ),
        (
            format!(r#"{{"usage_hints":["keep",{graph_root}]}}"#),
            r#"{"usage_hints":["keep"]}"#,
        ),
        (
            format!(r#"{{"usage_hints":["a",{graph_root},"b"]}}"#),
            r#"{"usage_hints":["a","b"]}"#,
        ),
    ];

    for (raw, expected) in cases {
        assert_eq!(normalize_bv_usage_hints(&raw), expected);
    }
}

#[test]
fn normalize_bv_robot_output_masks_semver_pseudo_versions() {
    let raw = r#"{"version":"v0.0.0-20260325195524-5f7fec28b24d","id":"bd-one"}"#;

    assert_eq!(
        normalize_bv_robot_output(raw),
        r#"{"version":"BV_VERSION","id":"bd-one"}"#
    );
}

#[test]
fn robot_golden_ready_output() {
    let workspace = init_robot_golden_workspace();

    let output = run_br(
        &workspace,
        [
            "ready",
            "--robot",
            "--include-deferred",
            "--sort",
            "priority",
            "--limit",
            "0",
        ],
        "robot_golden_ready",
    );
    assert!(
        output.status.success(),
        "ready --robot failed: {}",
        output.stderr
    );
    assert_valid_json(&output.stdout, "ready --robot");
    assert_snapshot!("robot_ready_output", output.stdout.trim_end());
}

#[test]
fn robot_golden_bv_next_output() {
    if !require_bv("robot_golden_bv_next_output") {
        return;
    }

    let workspace = init_robot_golden_workspace();

    let output = run_bv(&workspace, ["--robot-next"]);
    assert!(
        output.status.success(),
        "bv --robot-next failed: {}",
        output.stderr
    );
    assert_valid_json(&output.stdout, "bv --robot-next");
    let normalized = normalize_bv_robot_output(&output.stdout);
    assert_valid_json(&normalized, "normalized bv --robot-next");
    assert_snapshot!("bv_robot_next_output", normalized);
}

#[test]
fn robot_golden_bv_triage_output() {
    if !require_bv("robot_golden_bv_triage_output") {
        return;
    }

    let workspace = init_robot_golden_workspace();

    let output = run_bv(&workspace, ["--robot-triage"]);
    assert!(
        output.status.success(),
        "bv --robot-triage failed: {}",
        output.stderr
    );
    assert_valid_json(&output.stdout, "bv --robot-triage");
    let normalized = normalize_bv_robot_output(&output.stdout);
    assert_valid_json(&normalized, "normalized bv --robot-triage");
    assert_snapshot!("bv_robot_triage_output", normalized);
}

#[test]
fn robot_golden_bv_plan_output() {
    if !require_bv("robot_golden_bv_plan_output") {
        return;
    }

    let workspace = init_robot_golden_workspace();

    let output = run_bv(&workspace, ["--robot-plan"]);
    assert!(
        output.status.success(),
        "bv --robot-plan failed: {}",
        output.stderr
    );
    assert_valid_json(&output.stdout, "bv --robot-plan");
    let normalized = normalize_bv_robot_output(&output.stdout);
    assert_valid_json(&normalized, "normalized bv --robot-plan");
    assert_snapshot!("bv_robot_plan_output", normalized);
}
