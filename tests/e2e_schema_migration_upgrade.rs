//! E2E regression tests for issue #398: `br doctor migrate-schema` must be
//! able to upgrade every schema actually shipped since v13 to the current
//! schema — in particular schema 15 (the #388 gate-history schema from the
//! v0.2.19-era line) and schema 16 (created by the released v0.2.19 binary),
//! both of which the reviewed migration used to reject with
//! "available only for 13->17 and 14->17".
//!
//! Fixture provenance (NOT synthesized `PRAGMA user_version` stamps):
//! - `tests/fixtures/schema_migration/schema15_pre384_era.db.gz` was created
//!   by a br binary built from commit `7c4af2a6~1` (`d1b90640`), the last
//!   commit with `CURRENT_SCHEMA_VERSION = 15`, by running real `init` /
//!   `create` / `dep add` / `label add` / `comment add` / `close` / `sync
//!   --flush-only` commands.
//! - `tests/fixtures/schema_migration/schema16_v0219_release.db.gz` was
//!   created the same way by the actual released `br 0.2.19` binary
//!   (linux_x86_64 GitHub release asset), which stamps schema 16.
//!
//! Each test follows exactly the remediation the SCHEMA_MISMATCH error
//! prints: plan -> apply -> verify data -> reject stale receipt -> undo ->
//! re-apply.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("schema_migration")
}

fn install_fixture_workspace(workspace: &BrWorkspace, db_gz: &str, issues: &str, config: &str) {
    let beads_dir = workspace.root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");

    let mut decoder = GzDecoder::new(fs::File::open(fixture_dir().join(db_gz)).expect("open gz"));
    let mut db_bytes = Vec::new();
    decoder
        .read_to_end(&mut db_bytes)
        .expect("gunzip fixture db");
    fs::write(beads_dir.join("beads.db"), &db_bytes).expect("write beads.db");
    fs::copy(fixture_dir().join(issues), beads_dir.join("issues.jsonl")).expect("copy jsonl");
    fs::copy(fixture_dir().join(config), beads_dir.join("config.yaml")).expect("copy config");
}

fn header_user_version(db_path: &Path) -> u32 {
    let bytes = fs::read(db_path).expect("read db");
    u32::from_be_bytes(bytes[60..64].try_into().expect("db header"))
}

fn db_declares_table(db_path: &Path, table: &str) -> bool {
    // The sqlite_schema table stores the verbatim CREATE TABLE DDL (with or
    // without IF NOT EXISTS), so a raw byte scan is a connection-free
    // existence witness good enough for a test.
    let bytes = fs::read(db_path).expect("read db");
    [
        format!("CREATE TABLE {table}"),
        format!("CREATE TABLE IF NOT EXISTS {table}"),
    ]
    .iter()
    .any(|needle| {
        bytes
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    })
}

#[allow(clippy::too_many_lines)]
fn upgrade_fixture_end_to_end(
    label: &str,
    db_gz: &str,
    issues: &str,
    config: &str,
    expected_from: u64,
    expected_issue_total: u64,
) {
    let workspace = BrWorkspace::new();
    install_fixture_workspace(&workspace, db_gz, issues, config);
    let db_path = workspace.root.join(".beads").join("beads.db");
    assert_eq!(
        u64::from(header_user_version(&db_path)),
        expected_from,
        "{label}: fixture must genuinely be at schema {expected_from}"
    );

    // 1. Ordinary commands refuse and print the reviewed-migration remediation.
    let stats = run_br(
        &workspace,
        ["stats", "--json", "--no-auto-flush", "--no-auto-import"],
        "stats_schema_mismatch",
    );
    assert!(
        !stats.status.success(),
        "{label}: stats must refuse on an old schema; stdout: {}",
        stats.stdout
    );
    let refusal = format!("{}{}", stats.stdout, stats.stderr);
    assert!(
        refusal.contains("migrate-schema plan"),
        "{label}: SCHEMA_MISMATCH remediation must name `br doctor migrate-schema plan`; got: {refusal}"
    );

    // 2. Follow the remediation: plan must accept the fixture.
    let plan = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "plan",
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "migrate_plan",
    );
    assert!(
        plan.status.success(),
        "{label}: plan must accept schema {expected_from}; stdout: {} stderr: {}",
        plan.stdout,
        plan.stderr
    );
    let plan_json: Value =
        serde_json::from_str(&extract_json_payload(&plan.stdout)).expect("plan JSON");
    assert_eq!(
        plan_json["eligible"],
        Value::Bool(true),
        "{label}: plan not eligible"
    );
    assert_eq!(plan_json["from_version"].as_u64(), Some(expected_from));
    assert_eq!(plan_json["to_version"].as_u64(), Some(17));
    let plan_token = plan_json["plan_token"]
        .as_str()
        .expect("plan token")
        .to_string();

    // 3. Apply migrates atomically to schema 17.
    let apply = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            &plan_token,
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "migrate_apply",
    );
    assert!(
        apply.status.success(),
        "{label}: apply failed; stdout: {} stderr: {}",
        apply.stdout,
        apply.stderr
    );
    let applied_json: Value =
        serde_json::from_str(&extract_json_payload(&apply.stdout)).expect("applied JSON");
    let run_id = applied_json["run_id"].as_str().expect("run id").to_string();
    assert_eq!(
        header_user_version(&db_path),
        17,
        "{label}: post-apply schema"
    );
    for table in [
        "gate_result_history",
        "capacity_exemptions",
        "capacity_exemption_history",
        "capacity_occupancy",
    ] {
        assert!(
            db_declares_table(&db_path, table),
            "{label}: migrated database must declare {table}"
        );
    }

    // 4. Tracker data survives and ordinary commands work again.
    let stats_after = run_br(
        &workspace,
        ["stats", "--json", "--no-auto-flush", "--no-auto-import"],
        "stats_after_apply",
    );
    assert!(
        stats_after.status.success(),
        "{label}: stats after apply failed: {}",
        stats_after.stderr
    );
    let stats_json: Value =
        serde_json::from_str(&extract_json_payload(&stats_after.stdout)).expect("stats JSON");
    assert_eq!(
        stats_json["summary"]["total_issues"].as_u64(),
        Some(expected_issue_total),
        "{label}: issue count must survive the migration"
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
        "list_after_apply",
    );
    assert!(
        list.status.success(),
        "{label}: list failed: {}",
        list.stderr
    );

    // 5. The consumed receipt is stale: re-plan reports nothing to do, and
    //    re-applying the old token must be rejected without mutating.
    let replan = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "plan",
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "replan_after_apply",
    );
    assert!(
        replan.status.success(),
        "{label}: re-plan failed: {}",
        replan.stderr
    );
    let replan_json: Value =
        serde_json::from_str(&extract_json_payload(&replan.stdout)).expect("replan JSON");
    assert_eq!(
        replan_json["eligible"],
        Value::Bool(false),
        "{label}: second plan must be a no-op"
    );

    let stale_apply = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            &plan_token,
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "stale_apply",
    );
    assert!(
        !stale_apply.status.success(),
        "{label}: stale plan token must be rejected; stdout: {}",
        stale_apply.stdout
    );
    assert_eq!(
        header_user_version(&db_path),
        17,
        "{label}: rejected stale apply must not mutate the database"
    );

    // 6. Undo restores the exact pre-migration family, and the migration can
    //    be re-planned and re-applied afterwards.
    let undo = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "undo",
            &run_id,
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "migrate_undo",
    );
    assert!(
        undo.status.success(),
        "{label}: undo failed; stdout: {} stderr: {}",
        undo.stdout,
        undo.stderr
    );
    assert_eq!(
        u64::from(header_user_version(&db_path)),
        expected_from,
        "{label}: undo must restore the pre-migration schema version"
    );

    let plan2 = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "plan",
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "plan_after_undo",
    );
    assert!(
        plan2.status.success(),
        "{label}: plan after undo failed: {}",
        plan2.stderr
    );
    let plan2_json: Value =
        serde_json::from_str(&extract_json_payload(&plan2.stdout)).expect("plan2 JSON");
    assert_eq!(plan2_json["eligible"], Value::Bool(true));
    let token2 = plan2_json["plan_token"]
        .as_str()
        .expect("token2")
        .to_string();
    let apply2 = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            &token2,
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "apply_after_undo",
    );
    assert!(
        apply2.status.success(),
        "{label}: apply after undo failed; stdout: {} stderr: {}",
        apply2.stdout,
        apply2.stderr
    );
    assert_eq!(header_user_version(&db_path), 17);
}

/// Schema 15 (gate-history era, pre-#384) upgrades to the current schema.
#[test]
fn e2e_migrate_schema_upgrades_real_schema15_database() {
    let _log = common::test_log("e2e_migrate_schema_upgrades_real_schema15_database");
    upgrade_fixture_end_to_end(
        "schema15",
        "schema15_pre384_era.db.gz",
        "schema15_issues.jsonl",
        "schema15_config.yaml",
        15,
        2,
    );
}

/// Schema 16 (as created by the released v0.2.19 binary) upgrades to the
/// current schema.
#[test]
fn e2e_migrate_schema_upgrades_real_schema16_database() {
    let _log = common::test_log("e2e_migrate_schema_upgrades_real_schema16_database");
    upgrade_fixture_end_to_end(
        "schema16",
        "schema16_v0219_release.db.gz",
        "schema16_issues.jsonl",
        "schema16_config.yaml",
        16,
        3,
    );
}
