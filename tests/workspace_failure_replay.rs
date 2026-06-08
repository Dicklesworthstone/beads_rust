mod common;

use common::cli::{BrRun, BrWorkspace, extract_json_payload, run_br};
use common::{
    WorkspaceFailureCommandOutcome, WorkspaceFailureFixtureMetadata,
    isolated_workspace_failure_fixture, list_workspace_failure_fixtures,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

struct FixtureWorkspace {
    metadata: WorkspaceFailureFixtureMetadata,
    beads_dir: PathBuf,
    workspace: BrWorkspace,
}

fn fixture_workspace(name: &str) -> FixtureWorkspace {
    let isolated = isolated_workspace_failure_fixture(name).expect("isolated fixture");
    let metadata = isolated.fixture.metadata.clone();
    let root = isolated.root.clone();
    let beads_dir = isolated.beads_dir.clone();
    let log_dir = root.join("logs");
    fs::create_dir_all(&log_dir).expect("log dir");

    FixtureWorkspace {
        metadata,
        beads_dir,
        workspace: BrWorkspace {
            temp_dir: isolated.temp_dir,
            root,
            log_dir,
        },
    }
}

fn parse_stdout_json(run: &BrRun, context: &str) -> Value {
    let payload = extract_json_payload(&run.stdout);
    match serde_json::from_str(&payload) {
        Ok(value) => value,
        Err(err) => {
            assert!(
                payload.len() == usize::MAX,
                "{context} should emit valid JSON on stdout: {err}\nstdout={}\nstderr={}",
                run.stdout,
                run.stderr
            );
            Value::Null
        }
    }
}

fn parse_stderr_json(run: &BrRun, context: &str) -> Value {
    let payload = extract_json_payload(&run.stderr);
    match serde_json::from_str(&payload) {
        Ok(value) => value,
        Err(err) => {
            assert!(
                payload.len() == usize::MAX,
                "{context} should emit structured JSON on stderr: {err}\nstdout={}\nstderr={}",
                run.stdout,
                run.stderr
            );
            Value::Null
        }
    }
}

fn doctor_check<'a>(doctor_json: &'a Value, name: &str) -> &'a Value {
    let Some(check) = doctor_json["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["name"] == name))
    else {
        assert!(
            name.len() == usize::MAX,
            "doctor report missing check '{name}': {doctor_json}"
        );
        return doctor_json;
    };
    check
}

fn surface_label(name: &str, surface: &str) -> String {
    let slug: String = surface
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    format!("{name}_{slug}")
}

fn run_surface(
    fixture: &FixtureWorkspace,
    surface: &str,
    outcome: WorkspaceFailureCommandOutcome,
) -> BrRun {
    let label = surface_label(&fixture.metadata.name, surface);
    match surface {
        "startup/open" => run_br(&fixture.workspace, ["list", "--json"], &label),
        "create" => run_br(
            &fixture.workspace,
            ["create", "Replay harness probe", "--json"],
            &label,
        ),
        "doctor" => run_br(&fixture.workspace, ["doctor", "--json"], &label),
        "doctor --repair"
            if outcome == WorkspaceFailureCommandOutcome::RepairApplied
                && fixture_has_failed_rebuild_evidence(fixture) =>
        {
            run_br(
                &fixture.workspace,
                ["doctor", "--repair", "--allow-repeated-repair", "--json"],
                &label,
            )
        }
        "doctor --repair" => run_br(&fixture.workspace, ["doctor", "--repair", "--json"], &label),
        "sync --status" => run_br(&fixture.workspace, ["sync", "--status", "--json"], &label),
        "sync --import-only" => run_br(
            &fixture.workspace,
            ["sync", "--import-only", "--json"],
            &label,
        ),
        "list --no-db" => run_br(&fixture.workspace, ["--no-db", "list", "--json"], &label),
        "config get" => run_br(
            &fixture.workspace,
            ["config", "get", "issue_prefix", "--json"],
            &label,
        ),
        "config list" => run_br(&fixture.workspace, ["config", "list", "--json"], &label),
        "history" => run_br(&fixture.workspace, ["history", "list", "--json"], &label),
        "where" => run_br(&fixture.workspace, ["where", "--json"], &label),
        "info" => run_br(&fixture.workspace, ["info", "--json"], &label),
        other => unreachable!("unsupported replay surface '{other}'"),
    }
}

fn fixture_has_failed_rebuild_evidence(fixture: &FixtureWorkspace) -> bool {
    let recovery_dir = fixture.beads_dir.join(".br_recovery");
    let Ok(entries) = fs::read_dir(recovery_dir) else {
        return false;
    };

    entries.filter_map(Result::ok).any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .contains(".rebuild-failed")
    })
}

fn assert_sqlite_header(db_path: &Path, context: &str) {
    let bytes = match fs::read(db_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            assert!(
                context.len() == usize::MAX,
                "{context} should leave a readable SQLite database at {}: {err}",
                db_path.display()
            );
            Vec::new()
        }
    };
    assert!(
        bytes.starts_with(b"SQLite format 3\0"),
        "{context} should leave a SQLite database header at {}",
        db_path.display()
    );
}

fn resolved_database_path(fixture: &FixtureWorkspace, surface: &str) -> PathBuf {
    let where_run = run_br(
        &fixture.workspace,
        ["where", "--json"],
        &surface_label(&fixture.metadata.name, surface),
    );
    assert!(
        where_run.status.success(),
        "{} {surface} failed: {}",
        fixture.metadata.name,
        where_run.stderr
    );
    let where_json = parse_stdout_json(&where_run, &format!("{} {surface}", fixture.metadata.name));
    where_json["database_path"]
        .as_str()
        .map(PathBuf::from)
        .expect("where output should include database_path")
}

fn assert_config_error(run: &BrRun, needle: &str, context: &str) {
    assert!(
        !run.status.success(),
        "{context} should fail\nstdout={}\nstderr={}",
        run.stdout,
        run.stderr
    );
    let error_json = parse_stderr_json(run, context);
    assert_eq!(
        error_json["error"]["code"].as_str(),
        Some("CONFIG_ERROR"),
        "{context} should surface CONFIG_ERROR: {error_json}"
    );
    assert!(
        error_json["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(needle)),
        "{context} should mention '{needle}': {error_json}"
    );
}

fn first_issue_id(list_json: &Value) -> String {
    list_json["issues"]
        .as_array()
        .and_then(|issues| issues.first())
        .and_then(|issue| issue["id"].as_str())
        .map(str::to_string)
        .expect("list output should contain at least one issue id")
}

fn first_issue_id_from_jsonl(jsonl_path: &Path) -> String {
    let contents = fs::read_to_string(jsonl_path).expect("read jsonl");
    contents
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|issue| issue["id"].as_str().map(str::to_string))
        .expect("fixture jsonl should contain at least one valid issue id")
}

fn create_issue_id(create_json: &Value) -> String {
    if let Some(created) = create_json["created"]
        .as_array()
        .and_then(|created| created.first())
    {
        return created["id"]
            .as_str()
            .map(str::to_string)
            .expect("created entry should contain id");
    }
    create_json["id"]
        .as_str()
        .map(str::to_string)
        .expect("create output should contain id")
}

fn assert_custom_path_resolution(fixture: &FixtureWorkspace, surface: &str, json: &Value) {
    if fixture.metadata.name != "metadata_custom_paths" {
        return;
    }

    let expected_db_path = fixture.beads_dir.join("custom.db");
    let expected_jsonl_path = fixture.beads_dir.join("custom.jsonl");
    let surface_name = match surface {
        "where" => "where",
        "info" => "info",
        other => unreachable!("unsupported custom-path surface '{other}'"),
    };

    assert!(
        json["database_path"]
            .as_str()
            .is_some_and(|path| path == expected_db_path.display().to_string()),
        "{surface_name} should resolve custom database path: {json}"
    );
    assert!(
        json["jsonl_path"]
            .as_str()
            .is_some_and(|path| path == expected_jsonl_path.display().to_string()),
        "{surface_name} should resolve custom JSONL path: {json}"
    );
}

fn assert_doctor_clean_surface(fixture: &FixtureWorkspace, context: &str, json: &Value) {
    assert_eq!(
        json["ok"],
        Value::Bool(true),
        "{context} should be clean: {json}"
    );
    assert_doctor_reliability_audit(fixture, context, json);
}

fn reliability_audit_anomalies<'a>(
    fixture: &FixtureWorkspace,
    context: &str,
    json: &'a Value,
) -> &'a Vec<Value> {
    static EMPTY_ANOMALIES: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();

    let workspace_health = json["workspace_health"].as_str().unwrap_or("");
    assert!(
        !workspace_health.is_empty(),
        "{context} should include workspace_health: {json}"
    );

    let audit = &json["reliability_audit"];
    assert!(
        audit.is_object(),
        "{context} should include reliability_audit: {json}"
    );
    assert_eq!(
        audit["source"].as_str(),
        Some("doctor.inspect"),
        "{context} should identify doctor.inspect as reliability audit source: {json}"
    );
    assert_eq!(
        audit["health"].as_str(),
        Some(workspace_health),
        "{context} should keep workspace_health and reliability_audit.health aligned: {json}"
    );

    let anomalies = if let Some(anomalies) = audit["anomalies"].as_array() {
        anomalies
    } else {
        assert!(
            context.len() == usize::MAX,
            "{context} reliability_audit.anomalies should be an array: {json}"
        );
        EMPTY_ANOMALIES.get_or_init(Vec::new)
    };
    assert_eq!(
        audit["anomaly_count"].as_u64(),
        Some(anomalies.len() as u64),
        "{context} should keep anomaly_count aligned with anomaly array length: {json}"
    );

    for anomaly in anomalies {
        assert!(
            anomaly["code"]
                .as_str()
                .is_some_and(|code| !code.is_empty()),
            "{context} anomaly should include a stable code: {anomaly}"
        );
        assert!(
            anomaly["severity"]
                .as_str()
                .is_some_and(|severity| matches!(severity, "degraded" | "recoverable" | "unsafe")),
            "{context} anomaly should include a non-healthy severity: {anomaly}"
        );
        assert!(
            anomaly["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "{context} anomaly should include an operator-facing message: {anomaly}"
        );
    }

    if matches!(
        fixture.metadata.expected_classification.as_str(),
        "degraded" | "recoverable" | "unsafe"
    ) {
        assert!(
            !anomalies.is_empty(),
            "{context} should expose at least one diagnostic anomaly for {:?}: {json}",
            fixture.metadata.expected_classification
        );
    }

    anomalies
}

fn assert_doctor_reliability_audit(fixture: &FixtureWorkspace, context: &str, json: &Value) {
    let anomalies = reliability_audit_anomalies(fixture, context, json);
    let has_code = |code: &str| {
        anomalies
            .iter()
            .any(|anomaly| anomaly["code"].as_str() == Some(code))
    };

    match fixture.metadata.family.as_str() {
        "sidecar_mismatch" => {
            assert!(
                has_code("sidecar_mismatch") || has_code("database_corrupt"),
                "{context} should surface sidecar or WAL-corruption diagnostics: {json}"
            );
        }
        "malformed_jsonl" => {
            assert!(
                has_code("jsonl_conflict_markers"),
                "{context} should surface JSONL conflict marker diagnostics: {json}"
            );
        }
        "drift" => {
            assert!(
                has_code("db_jsonl_count_mismatch"),
                "{context} should surface DB/JSONL drift diagnostics: {json}"
            );
            if fixture.metadata.name == "db_jsonl_disagreement" {
                let counts = doctor_check(json, "counts.db_vs_jsonl");
                assert_eq!(
                    counts["status"].as_str(),
                    Some("warn"),
                    "db_jsonl_disagreement should warn on DB/JSONL drift: {json}"
                );
            }
        }
        "legacy_schema_drift" => {
            assert!(
                has_code("duplicate_config_keys"),
                "{context} should surface duplicate config diagnostics: {json}"
            );
        }
        "corrupt_db" | "recovery_debris" => {
            assert!(
                has_code("database_not_sqlite") || has_code("database_corrupt"),
                "{context} should surface malformed database diagnostics: {json}"
            );
        }
        _ => {}
    }
}

fn assert_repair_applied_surface(context: &str, json: &Value) {
    assert_eq!(
        json["repaired"],
        Value::Bool(true),
        "{context} should apply repair: {json}"
    );
    assert_eq!(
        json["verified"],
        Value::Bool(true),
        "{context} should verify the repair: {json}"
    );
    assert_eq!(
        json["post_repair"]["ok"],
        Value::Bool(true),
        "{context} should leave the workspace healthy: {json}"
    );
}

fn assert_status_surface(
    context: &str,
    json: &Value,
    expected_jsonl_newer: bool,
    expected_db_newer: bool,
) {
    assert_eq!(
        json["jsonl_newer"],
        Value::Bool(expected_jsonl_newer),
        "{context} reported unexpected jsonl_newer: {json}"
    );
    assert_eq!(
        json["db_newer"],
        Value::Bool(expected_db_newer),
        "{context} reported unexpected db_newer: {json}"
    );
}

fn assert_surface_outcome(
    fixture: &FixtureWorkspace,
    surface: &str,
    outcome: WorkspaceFailureCommandOutcome,
) {
    let run = run_surface(fixture, surface, outcome);
    let context = format!("{} {surface}", fixture.metadata.name);

    match outcome {
        WorkspaceFailureCommandOutcome::Success => {
            assert!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context);
            if matches!(surface, "where" | "info") {
                assert_custom_path_resolution(fixture, surface, &json);
            }
        }
        WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery => {
            assert!(run.status.success(), "{context} failed: {}", run.stderr);
            let _json = parse_stdout_json(&run, &context);
            assert_sqlite_header(&resolved_database_path(fixture, "resolved_db"), &context);
        }
        WorkspaceFailureCommandOutcome::DoctorClean => {
            assert!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context);
            assert_doctor_clean_surface(fixture, &context, &json);
        }
        WorkspaceFailureCommandOutcome::ReportsErrors => {
            assert!(
                !run.status.success(),
                "{context} should report errors\nstdout={}\nstderr={}",
                run.stdout,
                run.stderr
            );
            let json = parse_stdout_json(&run, &context);
            assert_eq!(
                json["ok"],
                Value::Bool(false),
                "{context} should be unhealthy: {json}"
            );
            assert_doctor_reliability_audit(fixture, &context, &json);
        }
        WorkspaceFailureCommandOutcome::RepairApplied => {
            assert!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context);
            assert_repair_applied_surface(&context, &json);
        }
        WorkspaceFailureCommandOutcome::RepairNoop => {
            assert!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context);
            assert_eq!(
                json["repaired"],
                Value::Bool(false),
                "{context} should report a repair noop: {json}"
            );
        }
        WorkspaceFailureCommandOutcome::StatusInSync => {
            assert!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context);
            assert_status_surface(&context, &json, false, false);
        }
        WorkspaceFailureCommandOutcome::StatusJsonlNewer => {
            assert!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context);
            assert_status_surface(&context, &json, true, false);
        }
        WorkspaceFailureCommandOutcome::StatusDiverged => {
            assert!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context);
            assert_status_surface(&context, &json, true, true);
        }
        WorkspaceFailureCommandOutcome::StatusDbNewer => {
            assert!(run.status.success(), "{context} failed: {}", run.stderr);
            let json = parse_stdout_json(&run, &context);
            assert_status_surface(&context, &json, false, true);
        }
        WorkspaceFailureCommandOutcome::FailsPrefixMismatch => {
            assert_config_error(&run, "Prefix mismatch", &context);
        }
        WorkspaceFailureCommandOutcome::FailsConflictMarkers => {
            assert_config_error(&run, "conflict marker", &context);
        }
        WorkspaceFailureCommandOutcome::FailsInvalidJson => {
            assert_config_error(&run, "invalid issue record", &context);
        }
        WorkspaceFailureCommandOutcome::FailsRepeatedRepair => {
            assert_config_error(&run, "--allow-repeated-repair", &context);
        }
    }
}

fn assert_core_read_success(fixture: &FixtureWorkspace) {
    let list_workspace = fixture_workspace(&fixture.metadata.name);
    let list = run_br(
        &list_workspace.workspace,
        ["list", "--json"],
        &surface_label(&fixture.metadata.name, "core_list"),
    );
    assert!(
        list.status.success(),
        "{} list --json failed: {}",
        fixture.metadata.name,
        list.stderr
    );
    let list_json = parse_stdout_json(&list, &format!("{} core list", fixture.metadata.name));
    let issue_id = first_issue_id(&list_json);

    let ready_workspace = fixture_workspace(&fixture.metadata.name);
    let ready = run_br(
        &ready_workspace.workspace,
        ["ready", "--json"],
        &surface_label(&fixture.metadata.name, "core_ready"),
    );
    assert!(
        ready.status.success(),
        "{} ready --json failed: {}",
        fixture.metadata.name,
        ready.stderr
    );
    let _ready_json = parse_stdout_json(&ready, &format!("{} core ready", fixture.metadata.name));

    let show_workspace = fixture_workspace(&fixture.metadata.name);
    let show = run_br(
        &show_workspace.workspace,
        ["show", &issue_id, "--json"],
        &surface_label(&fixture.metadata.name, "core_show"),
    );
    assert!(
        show.status.success(),
        "{} show --json failed: {}",
        fixture.metadata.name,
        show.stderr
    );
    let _show_json = parse_stdout_json(&show, &format!("{} core show", fixture.metadata.name));

    if fixture
        .metadata
        .outcome_for("startup/open")
        .is_some_and(|outcome| outcome == WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery)
    {
        assert_sqlite_header(
            &resolved_database_path(&show_workspace, "core_resolved_db"),
            &format!("{} core show", fixture.metadata.name),
        );
    }
}

fn assert_core_read_failure(
    fixture: &FixtureWorkspace,
    where_json: &Value,
    failure: WorkspaceFailureCommandOutcome,
) {
    let list_workspace = fixture_workspace(&fixture.metadata.name);
    assert_surface_outcome(&list_workspace, "startup/open", failure);

    let ready_workspace = fixture_workspace(&fixture.metadata.name);
    let ready = run_br(
        &ready_workspace.workspace,
        ["ready", "--json"],
        &surface_label(&fixture.metadata.name, "core_ready_fail"),
    );
    match failure {
        WorkspaceFailureCommandOutcome::FailsPrefixMismatch => {
            assert_config_error(
                &ready,
                "Prefix mismatch",
                &format!("{} core ready", fixture.metadata.name),
            );
        }
        WorkspaceFailureCommandOutcome::FailsConflictMarkers => {
            assert_config_error(
                &ready,
                "conflict marker",
                &format!("{} core ready", fixture.metadata.name),
            );
        }
        _ => unreachable!(),
    }

    let jsonl_path = where_json["jsonl_path"]
        .as_str()
        .map(PathBuf::from)
        .expect("where jsonl_path");
    let issue_id = first_issue_id_from_jsonl(&jsonl_path);
    let show_workspace = fixture_workspace(&fixture.metadata.name);
    let show = run_br(
        &show_workspace.workspace,
        ["show", &issue_id, "--json"],
        &surface_label(&fixture.metadata.name, "core_show_fail"),
    );
    match failure {
        WorkspaceFailureCommandOutcome::FailsPrefixMismatch => {
            assert_config_error(
                &show,
                "Prefix mismatch",
                &format!("{} core show", fixture.metadata.name),
            );
        }
        WorkspaceFailureCommandOutcome::FailsConflictMarkers => {
            assert_config_error(
                &show,
                "conflict marker",
                &format!("{} core show", fixture.metadata.name),
            );
        }
        _ => unreachable!(),
    }
}

fn assert_core_write_success(
    fixture: &FixtureWorkspace,
    create: &BrRun,
    expected_create: WorkspaceFailureCommandOutcome,
) {
    let create_json = parse_stdout_json(create, &format!("{} core create", fixture.metadata.name));
    let issue_id = create_issue_id(&create_json);
    if expected_create == WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery {
        assert_sqlite_header(
            &resolved_database_path(fixture, "core_create_resolved_db"),
            &format!("{} core create", fixture.metadata.name),
        );
    }

    let show = run_br(
        &fixture.workspace,
        ["show", &issue_id, "--json"],
        &surface_label(&fixture.metadata.name, "core_show_created"),
    );
    assert!(
        show.status.success(),
        "{} show after create failed: {}",
        fixture.metadata.name,
        show.stderr
    );
    let _show_json = parse_stdout_json(
        &show,
        &format!("{} core show after create", fixture.metadata.name),
    );

    let update = run_br(
        &fixture.workspace,
        ["update", &issue_id, "--status", "in_progress", "--json"],
        &surface_label(&fixture.metadata.name, "core_update"),
    );
    assert!(
        update.status.success(),
        "{} update failed: {}",
        fixture.metadata.name,
        update.stderr
    );

    let label_add = run_br(
        &fixture.workspace,
        ["label", "add", &issue_id, "replay-probe", "--json"],
        &surface_label(&fixture.metadata.name, "core_label"),
    );
    assert!(
        label_add.status.success(),
        "{} label add failed: {}",
        fixture.metadata.name,
        label_add.stderr
    );

    let comment = run_br(
        &fixture.workspace,
        ["comments", "add", &issue_id, "Replay note", "--json"],
        &surface_label(&fixture.metadata.name, "core_comment"),
    );
    assert!(
        comment.status.success(),
        "{} comments add failed: {}",
        fixture.metadata.name,
        comment.stderr
    );

    let close = run_br(
        &fixture.workspace,
        ["close", &issue_id, "--reason", "Replay close", "--json"],
        &surface_label(&fixture.metadata.name, "core_close"),
    );
    assert!(
        close.status.success(),
        "{} close failed: {}",
        fixture.metadata.name,
        close.stderr
    );

    let reopen = run_br(
        &fixture.workspace,
        ["reopen", &issue_id, "--json"],
        &surface_label(&fixture.metadata.name, "core_reopen"),
    );
    assert!(
        reopen.status.success(),
        "{} reopen failed: {}",
        fixture.metadata.name,
        reopen.stderr
    );

    let delete = run_br(
        &fixture.workspace,
        ["delete", &issue_id, "--json"],
        &surface_label(&fixture.metadata.name, "core_delete"),
    );
    assert!(
        delete.status.success(),
        "{} delete failed: {}",
        fixture.metadata.name,
        delete.stderr
    );
}

fn assert_core_write_failure(
    fixture: &FixtureWorkspace,
    create: &BrRun,
    expected_create: WorkspaceFailureCommandOutcome,
) {
    match expected_create {
        WorkspaceFailureCommandOutcome::FailsPrefixMismatch => {
            assert_config_error(
                create,
                "Prefix mismatch",
                &format!("{} core create", fixture.metadata.name),
            );
        }
        WorkspaceFailureCommandOutcome::FailsConflictMarkers => {
            assert_config_error(
                create,
                "conflict marker",
                &format!("{} core create", fixture.metadata.name),
            );
        }
        other => unreachable!(
            "{} has unsupported create outcome for core write replay: {:?}",
            fixture.metadata.name, other
        ),
    }
}

#[test]
fn workspace_failure_replay_manifest_expectations_hold_on_fresh_copies() {
    let _guard = common::workspace_replay_test_guard();
    let _log =
        common::test_log("workspace_failure_replay_manifest_expectations_hold_on_fresh_copies");
    let fixtures = list_workspace_failure_fixtures().expect("fixture catalog");

    for fixture in fixtures {
        for expectation in &fixture.metadata.expected_command_outcomes {
            let workspace = fixture_workspace(&fixture.metadata.name);
            assert_surface_outcome(&workspace, &expectation.surface, expectation.outcome);
        }
    }
}

#[test]
fn workspace_failure_replay_doctor_reliability_audit_matches_fixture_posture() {
    let _guard = common::workspace_replay_test_guard();
    let _log = common::test_log(
        "workspace_failure_replay_doctor_reliability_audit_matches_fixture_posture",
    );
    let fixtures = list_workspace_failure_fixtures().expect("fixture catalog");

    for fixture in fixtures {
        let Some(doctor_outcome) = fixture.metadata.outcome_for("doctor") else {
            continue;
        };

        let workspace = fixture_workspace(&fixture.metadata.name);
        let doctor = run_surface(&workspace, "doctor", doctor_outcome);
        let context = format!("{} doctor", fixture.metadata.name);
        let json = parse_stdout_json(&doctor, &context);
        assert_doctor_reliability_audit(&workspace, &context, &json);
    }
}

#[test]
fn workspace_failure_replay_core_read_surfaces_match_expected_posture() {
    let _guard = common::workspace_replay_test_guard();
    let _log =
        common::test_log("workspace_failure_replay_core_read_surfaces_match_expected_posture");
    let fixtures = list_workspace_failure_fixtures().expect("fixture catalog");

    for fixture in fixtures {
        let where_workspace = fixture_workspace(&fixture.metadata.name);
        let where_run = run_br(
            &where_workspace.workspace,
            ["where", "--json"],
            &surface_label(&fixture.metadata.name, "core_where"),
        );
        assert!(
            where_run.status.success(),
            "{} where --json failed: {}",
            fixture.metadata.name,
            where_run.stderr
        );
        let where_json =
            parse_stdout_json(&where_run, &format!("{} core where", fixture.metadata.name));

        let info_workspace = fixture_workspace(&fixture.metadata.name);
        let info = run_br(
            &info_workspace.workspace,
            ["info", "--json"],
            &surface_label(&fixture.metadata.name, "core_info"),
        );
        assert!(
            info.status.success(),
            "{} info --json failed: {}",
            fixture.metadata.name,
            info.stderr
        );
        let _info_json = parse_stdout_json(&info, &format!("{} core info", fixture.metadata.name));

        match fixture
            .metadata
            .outcome_for("startup/open")
            .expect("startup/open expectation")
        {
            WorkspaceFailureCommandOutcome::Success
            | WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery => {
                assert_core_read_success(&where_workspace);
            }
            WorkspaceFailureCommandOutcome::FailsPrefixMismatch
            | WorkspaceFailureCommandOutcome::FailsConflictMarkers => {
                let failure = fixture
                    .metadata
                    .outcome_for("startup/open")
                    .expect("startup/open failure");
                assert_core_read_failure(&where_workspace, &where_json, failure);
            }
            other => unreachable!(
                "{} has unsupported startup/open outcome for core read replay: {:?}",
                fixture.metadata.name, other
            ),
        }
    }
}

#[test]
fn workspace_failure_replay_core_write_surfaces_match_expected_posture() {
    let _guard = common::workspace_replay_test_guard();
    let _log =
        common::test_log("workspace_failure_replay_core_write_surfaces_match_expected_posture");
    let fixtures = list_workspace_failure_fixtures().expect("fixture catalog");

    for fixture in fixtures {
        let expected_create = fixture
            .metadata
            .outcome_for("create")
            .expect("create expectation");
        let workspace = fixture_workspace(&fixture.metadata.name);
        let create = run_br(
            &workspace.workspace,
            ["create", "Replay write probe", "--json"],
            &surface_label(&fixture.metadata.name, "core_create"),
        );

        match expected_create {
            WorkspaceFailureCommandOutcome::Success
            | WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery => {
                assert!(
                    create.status.success(),
                    "{} create failed: {}",
                    fixture.metadata.name,
                    create.stderr
                );
                assert_core_write_success(&workspace, &create, expected_create);
            }
            WorkspaceFailureCommandOutcome::FailsPrefixMismatch
            | WorkspaceFailureCommandOutcome::FailsConflictMarkers => {
                assert_core_write_failure(&workspace, &create, expected_create);
            }
            other => unreachable!(
                "{} has unsupported create outcome for core write replay: {:?}",
                fixture.metadata.name, other
            ),
        }
    }
}

fn infer_classification(metadata: &WorkspaceFailureFixtureMetadata) -> &'static str {
    let startup = metadata.outcome_for("startup/open");
    let doctor = metadata.outcome_for("doctor");
    let create = metadata.outcome_for("create");
    let sync_status = metadata.outcome_for("sync --status");

    let startup_fails = matches!(
        startup,
        Some(
            WorkspaceFailureCommandOutcome::FailsPrefixMismatch
                | WorkspaceFailureCommandOutcome::FailsConflictMarkers
                | WorkspaceFailureCommandOutcome::FailsInvalidJson
        )
    );
    let startup_needs_recovery = matches!(
        startup,
        Some(WorkspaceFailureCommandOutcome::SuccessWithAutoRecovery)
    );
    let doctor_reports_errors =
        matches!(doctor, Some(WorkspaceFailureCommandOutcome::ReportsErrors));
    let sync_shows_drift = matches!(
        sync_status,
        Some(
            WorkspaceFailureCommandOutcome::StatusJsonlNewer
                | WorkspaceFailureCommandOutcome::StatusDiverged
        )
    );

    if startup_fails {
        return "unsafe";
    }
    if startup_needs_recovery {
        return "recoverable";
    }
    if doctor_reports_errors || sync_shows_drift {
        return "degraded";
    }
    match (startup, create) {
        (
            Some(WorkspaceFailureCommandOutcome::Success),
            Some(WorkspaceFailureCommandOutcome::Success),
        ) if !doctor_reports_errors && !sync_shows_drift => {
            if matches!(doctor, Some(WorkspaceFailureCommandOutcome::DoctorClean))
                && matches!(
                    sync_status,
                    Some(WorkspaceFailureCommandOutcome::StatusInSync) | None
                )
            {
                "healthy"
            } else {
                "usable"
            }
        }
        _ => "unknown",
    }
}

#[test]
fn workspace_failure_replay_classification_coherence() {
    let _guard = common::workspace_replay_test_guard();
    let _log = common::test_log("workspace_failure_replay_classification_coherence");
    let fixtures = list_workspace_failure_fixtures().expect("fixture catalog");

    assert!(
        !fixtures.is_empty(),
        "fixture catalog should contain at least one fixture"
    );

    let valid_classifications = ["healthy", "usable", "degraded", "recoverable", "unsafe"];

    for fixture in &fixtures {
        let declared = &fixture.metadata.expected_classification;
        assert!(
            valid_classifications.contains(&declared.as_str()),
            "{}: declared classification '{}' is not in the valid set {:?}",
            fixture.metadata.name,
            declared,
            valid_classifications
        );

        let inferred = infer_classification(&fixture.metadata);
        assert_eq!(
            declared.as_str(),
            inferred,
            "{}: declared classification '{}' does not match inferred '{}' from surface outcomes \
             (startup/open={:?}, doctor={:?}, create={:?})",
            fixture.metadata.name,
            declared,
            inferred,
            fixture.metadata.outcome_for("startup/open"),
            fixture.metadata.outcome_for("doctor"),
            fixture.metadata.outcome_for("create"),
        );
    }

    let families: std::collections::HashSet<&str> = fixtures
        .iter()
        .map(|f| f.metadata.expected_classification.as_str())
        .collect();
    assert!(
        families.len() >= 3,
        "fixture corpus should cover at least 3 distinct classification levels, got: {:?}",
        families
    );
}
