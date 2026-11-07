//! Release-layout incompatibility must never authorize generic reconstruction.

use super::{
    TableRows, edit_schema19_fixture, engine_rows, header_user_version, released_schema19_workspace,
    run_br, schema19_authority_rows, schema19_json, schema19_tracker_files,
    seed_schema19_database_authority, seed_workflow_schema19_operator_state,
    workflow_schema19_authority, workflow_schema19_durable_files, workflow_schema19_workspace,
};
use crate::common::cli::{BrWorkspace, run_br_with_env};
use beads_rust::franken_sync::Connection;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

#[test]
fn released_layout_health_is_unsafe_in_full_and_quick_reports() {
    let workspace = released_schema19_workspace();
    let beads = workspace.root.join(".beads");
    let before = schema19_tracker_files(&beads);
    for (args, label) in [
        (vec!["doctor", "--json"], "release_health_full"),
        (vec!["doctor", "--quick", "--json"], "release_health_quick"),
    ] {
        let inspected = run_br(&workspace, args, label);
        assert!(!inspected.status.success(), "{}", inspected.stdout);
        let report: Value = serde_json::from_str(&inspected.stdout).expect("whole doctor JSON");
        assert_eq!(report["ok"], false);
        assert_eq!(report["workspace_health"], "unsafe");
        assert_eq!(report["reliability_audit"]["health"], "unsafe");
        let anomaly = report["reliability_audit"]["anomalies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|anomaly| anomaly["code"] == "schema_incompatible")
            .expect("runtime incompatibility must affect aggregate health");
        assert_eq!(anomaly["severity"], "unsafe");
        assert_eq!(schema19_tracker_files(&beads), before);
    }
}

#[test]
fn generic_repair_refuses_release_upgrade_before_any_rebuild_or_session() {
    let workspace = released_schema19_workspace();
    seed_schema19_database_authority(&workspace);
    let beads = workspace.root.join(".beads");
    let db = beads.join("beads.db");
    // Materialize only ordinary coordination files before the byte baseline.
    // No engine or generic repair is used to establish these lock paths.
    let authority =
        beads_rust::sync::blocking_database_family_write_lock_with_timeout(&beads, &db, Some(1000))
            .unwrap();
    authority.bind_database_inode_for_mutation().unwrap();
    drop(authority);
    let before = schema19_tracker_files(&beads);
    let rows = schema19_authority_rows(&db, None);
    let runs = workspace.root.join("repair-run-artifacts");
    let additions = json!({
        "jsonl_row_receipts_created": true,
        "closed_by_column_added": true,
    });
    let variants: &[&[&str]] = &[
        &[],
        &["--dry-run"],
        &["--unsafe-auto-fix"],
        &["--dry-run", "--unsafe-auto-fix"],
        &["--only", "fm-schemas-missing-required-column"],
        &["--skip", "fm-schemas-missing-required-column"],
    ];
    for (index, extra) in variants.iter().enumerate() {
        let mut args = vec!["doctor", "--repair", "--json"];
        args.extend_from_slice(extra);
        let refused = run_br_with_env(
            &workspace,
            args,
            [("BR_DOCTOR_RUNS_DIR", runs.as_os_str())],
            &format!("release_generic_repair_refusal_{index}"),
        );
        assert_eq!(
            refused.status.code(),
            Some(4),
            "{} {}",
            refused.stdout,
            refused.stderr
        );
        let report: Value = serde_json::from_str(&refused.stdout).expect("whole refusal JSON");
        assert_eq!(report["ok"], false);
        assert_eq!(report["code"], "refused_unsafe");
        assert_eq!(report["gate"], "schema19_runtime_upgrade");
        assert_eq!(report["evidence"]["schema19_runtime_upgrade"], additions);
        assert_eq!(report["evidence"]["dry_run"], extra.contains(&"--dry-run"));
        assert_eq!(
            report["evidence"]["recommended_fix"],
            "br doctor migrate-schema plan --json"
        );
        assert_eq!(report["recovery_audit"]["outcome"], "refused");
        assert!(report["recovery_audit"].get("applied_actions").is_none());
        assert!(
            !runs.exists(),
            "refusal must precede repair-session creation"
        );
        assert!(!workspace.root.join(".doctor").exists());
        assert_eq!(schema19_tracker_files(&beads), before);
        assert_eq!(schema19_authority_rows(&db, None), rows);
    }

    // Refusal must leave the same authoritative source eligible for the
    // explicitly requested lossless operation, including its DB-only issue.
    let plan = schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "plan", "--json"],
        "release_plan_after_generic_refusal",
    ));
    assert_eq!(plan["eligible"], true);
    assert_eq!(plan["forecast"]["schema19_runtime_upgrade"], additions);
    assert_eq!(schema19_tracker_files(&beads), before);
}

fn current_release_workflow_workspace() -> BrWorkspace {
    let workspace = workflow_schema19_workspace();
    seed_workflow_schema19_operator_state(&workspace);
    // Start with the real release database and install exactly the two known
    // additions. The unsupported-layout tests must not pass only because the
    // older, missing-additions gate already refuses this source.
    edit_schema19_fixture(
        &workspace,
        &[
            "ALTER TABLE close_metadata ADD COLUMN closed_by TEXT",
            "UPDATE close_metadata SET closed_by = 'preserved close actor'",
            "CREATE TABLE jsonl_row_receipts (issue_id TEXT PRIMARY KEY, row_hash TEXT NOT NULL)",
            "INSERT INTO jsonl_row_receipts (rowid, issue_id, row_hash) VALUES (77, 'release-66o', 'preserved receipt')",
            "INSERT INTO config (rowid, key, value) VALUES (101, 'operator_duplicate', 'older'), (207, 'operator_duplicate', 'newer')",
            "INSERT INTO metadata (rowid, key, value) VALUES (173, 'operator_duplicate', 'older'), (277, 'operator_duplicate', 'newer')",
            "UPDATE issues SET rowid = 4096 WHERE id = 'release-9sp'",
        ],
    );
    workspace
}

#[test]
#[allow(clippy::too_many_lines)]
fn generic_repair_refuses_readable_unreviewed_current_layout_before_any_mutation() {
    const GATE: &str = "schema_current_layout_unsupported";
    let variants: &[&[&str]] = &[
        &[],
        &["--dry-run"],
        &["--unsafe-auto-fix"],
        &["--dry-run", "--unsafe-auto-fix"],
        &["--only", "fm-schemas-missing-required-column"],
        &["--skip", "fm-schemas-missing-required-column"],
        &["--allow-repeated-repair"],
    ];
    for table in ["issues", "config", "metadata"] {
        let workspace = current_release_workflow_workspace();
        edit_schema19_fixture(
            &workspace,
            &[
                &format!("ALTER TABLE {table} ADD COLUMN operator_payload BLOB"),
                &format!("UPDATE {table} SET operator_payload = X'0001FF007F'"),
            ],
        );
        let beads = workspace.root.join(".beads");
        let db = beads.join("beads.db");
        assert_eq!(header_user_version(&db), 19);
        let rows = workflow_schema19_authority(&db);
        let (columns, records) = &rows[table];
        let payload = columns
            .iter()
            .position(|column| column == "operator_payload")
            .expect("opaque authoritative column is present");
        assert!(!records.is_empty());
        assert!(
            records
                .iter()
                .all(|row| row[payload] == "blob [0, 1, 255, 0, 127]")
        );
        assert_eq!(rows["issues"].1.len(), 4);
        assert_eq!(rows["events"].1.len(), 23);
        assert_eq!(rows["close_metadata"].1.len(), 1);
        assert_eq!(rows["jsonl_row_receipts"].1.len(), 1);
        assert_eq!(rows["gate_result_history"].1.len(), 2);
        assert_eq!(rows["capacity_exemption_history"].1.len(), 2);
        assert_eq!(
            fs::read_to_string(beads.join("issues.jsonl"))
                .unwrap()
                .lines()
                .count(),
            3,
            "the JSONL cannot recreate every release database record"
        );

        // Pre-create only the normal authority files. The guard must inspect
        // a private family snapshot before opening or repairing the source.
        let authority =
            beads_rust::sync::blocking_database_family_write_lock_with_timeout(
                &beads,
                &db,
                Some(1000),
            )
            .unwrap();
        authority.bind_database_inode_for_mutation().unwrap();
        drop(authority);
        let before = schema19_tracker_files(&beads);
        let runs = workspace.root.join("repair-run-artifacts");
        for (index, extra) in variants.iter().enumerate() {
            let mut args = vec!["doctor", "--repair", "--json"];
            args.extend_from_slice(extra);
            let refused = run_br_with_env(
                &workspace,
                args,
                [("BR_DOCTOR_RUNS_DIR", runs.as_os_str())],
                &format!("unreviewed_{table}_generic_repair_{index}"),
            );
            // A partial-repair failure after installing a JSONL replacement
            // is still data loss. Check authority before trusting its exit.
            assert_eq!(
                workflow_schema19_authority(&db),
                rows,
                "{table}: {extra:?}"
            );
            assert!(
                schema19_tracker_files(&beads) == before,
                "{table}: {extra:?} changed tracker files: {} {}",
                refused.stdout,
                refused.stderr
            );
            assert_eq!(
                refused.status.code(),
                Some(4),
                "{} {}",
                refused.stdout,
                refused.stderr
            );
            let report: Value =
                serde_json::from_str(&refused.stdout).expect("whole refusal JSON");
            assert_eq!(report["ok"], false);
            assert_eq!(report["code"], "refused_unsafe");
            assert_eq!(report["gate"], GATE);
            assert_eq!(report["evidence"]["gate"], GATE);
            assert_eq!(report["evidence"]["finding"], "schema.runtime");
            assert_eq!(report["evidence"]["user_version"], 19);
            assert_eq!(report["evidence"]["runtime_compatible"], false);
            assert_eq!(
                report["evidence"]["layout_classification"],
                "unsupported_current_layout"
            );
            assert_eq!(
                report["evidence"]["dry_run"],
                extra.contains(&"--dry-run")
            );
            assert_eq!(report["recovery_audit"]["outcome"], "refused");
            assert!(report["recovery_audit"].get("applied_actions").is_none());
            assert!(
                !runs.exists(),
                "refusal must precede repair-session creation"
            );
            assert!(!workspace.root.join(".doctor").exists());
            assert!(!beads.join(".br_recovery").exists());
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn generic_repair_restores_current_owned_index_without_jsonl_reconstruction() {
    const MAINTENANCE: &str = "fm-schemas-current-runtime-maintenance";
    for (label, selected) in [("default", false), ("selected", true)] {
        let workspace = current_release_workflow_workspace();
        edit_schema19_fixture(&workspace, &["DROP INDEX idx_config_key"]);
        let beads = workspace.root.join(".beads");
        let db = beads.join("beads.db");
        let rows = workflow_schema19_authority(&db);
        let catalog_columns = TableRows::from([(
            "sqlite_schema".to_owned(),
            (
                ["type", "name", "tbl_name", "rootpage", "sql"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                Vec::new(),
            ),
        )]);
        let catalog = engine_rows(&db, Some(&catalog_columns));
        let authority =
            beads_rust::sync::blocking_database_family_write_lock_with_timeout(
                &beads,
                &db,
                Some(1000),
            )
            .unwrap();
        authority.bind_database_inode_for_mutation().unwrap();
        drop(authority);
        let before_preview = schema19_tracker_files(&beads);
        let runs = workspace.root.join("repair-run-artifacts");
        let mut preview_args = vec!["doctor", "--repair", "--dry-run", "--json"];
        if selected {
            preview_args.extend_from_slice(&["--only", MAINTENANCE]);
        }
        let preview = run_br_with_env(
            &workspace,
            preview_args,
            [("BR_DOCTOR_RUNS_DIR", runs.as_os_str())],
            &format!("current_index_repair_{label}_preview"),
        );
        let preview = schema19_json(&preview);
        assert_eq!(preview["schema"], "br.doctor.repair-plan.v1");
        let candidates = preview["candidate_repairs"].as_array().unwrap();
        assert_eq!(candidates.len(), 1, "{preview}");
        assert_eq!(
            candidates[0]["id"],
            "doctor.repair_current_schema_maintenance"
        );
        let runtime = preview["report"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "schema.runtime")
            .unwrap();
        assert_eq!(
            runtime["details"]["layout_classification"],
            "current_maintenance_required"
        );
        assert_eq!(schema19_tracker_files(&beads), before_preview);
        assert!(!runs.exists());

        if !selected {
            let skipped = run_br_with_env(
                &workspace,
                ["doctor", "--repair", "--skip", MAINTENANCE, "--json"],
                [("BR_DOCTOR_RUNS_DIR", runs.as_os_str())],
                "current_index_repair_skip_refusal",
            );
            assert_eq!(schema19_tracker_files(&beads), before_preview);
            assert_eq!(workflow_schema19_authority(&db), rows);
            assert_eq!(
                skipped.status.code(),
                Some(4),
                "{} {}",
                skipped.stdout,
                skipped.stderr
            );
            let refusal: Value =
                serde_json::from_str(&skipped.stdout).expect("whole skip refusal JSON");
            assert_eq!(refusal["code"], "refused_unsafe");
            assert_eq!(refusal["gate"], "schema_current_maintenance_required");
            assert!(!runs.exists());
            assert!(!workspace.root.join(".doctor").exists());
            assert!(!beads.join(".br_recovery").exists());
        }

        let mut source_artifacts = workflow_schema19_durable_files(&workspace);
        for name in ["beads.db", "beads.db-wal", "beads.db-journal"] {
            source_artifacts.remove(Path::new(name));
        }
        let mut apply_args = vec!["doctor", "--repair", "--json"];
        if selected {
            apply_args.extend_from_slice(&["--only", MAINTENANCE]);
        }
        let repaired = run_br_with_env(
            &workspace,
            apply_args,
            [("BR_DOCTOR_RUNS_DIR", runs.as_os_str())],
            &format!("current_index_repair_{label}_apply"),
        );
        assert_eq!(workflow_schema19_authority(&db), rows);
        let mut repaired_catalog = engine_rows(&db, Some(&catalog_columns));
        let indexes = &mut repaired_catalog.get_mut("sqlite_schema").unwrap().1;
        let restored = indexes
            .iter()
            .find(|row| row[1] == "text Some(\"idx_config_key\")")
            .expect("the missing owned index must actually be recreated");
        assert_eq!(restored[0], "text Some(\"index\")");
        assert_eq!(restored[2], "text Some(\"config\")");
        indexes.retain(|row| row[1] != "text Some(\"idx_config_key\")");
        assert_eq!(
            repaired_catalog, catalog,
            "only the missing index may change the catalog"
        );
        let mut after_artifacts = workflow_schema19_durable_files(&workspace);
        for name in ["beads.db", "beads.db-wal", "beads.db-journal"] {
            after_artifacts.remove(Path::new(name));
        }
        assert_eq!(after_artifacts, source_artifacts);
        assert_eq!(header_user_version(&db), 19);
        assert!(!beads.join(".br_recovery").exists());

        // Unrelated findings in this deliberately rich tracker may remain.
        // Maintenance must finish without another fixer or JSONL replacement.
        let report: Value = serde_json::from_str(&repaired.stdout).unwrap_or_else(|error| {
            panic!(
                "whole repair JSON: {error}: {} {}",
                repaired.stdout, repaired.stderr
            )
        });
        assert_eq!(report["repaired"], true, "{report}");
        assert_eq!(report["current_schema_maintenance_verified"], true);
        assert_eq!(
            report["recovery_audit"]["phase"],
            "doctor.current_schema_maintenance"
        );
        assert_eq!(
            report["recovery_audit"]["action"],
            "doctor.repair_current_schema_maintenance"
        );
        let actions = report["recovery_audit"]["applied_actions"]
            .as_array()
            .unwrap();
        assert_eq!(actions.as_slice(), &[json!("current_schema_maintenance")]);
        assert!(report["recovery_audit"].get("imported").is_none());

        // Repeat only the selected maintenance. Remaining, unrelated findings
        // do not authorize new work during this idempotence check.
        let repeated_source = workflow_schema19_durable_files(&workspace);
        let repeated = run_br_with_env(
            &workspace,
            ["doctor", "--repair", "--only", MAINTENANCE, "--json"],
            [("BR_DOCTOR_RUNS_DIR", runs.as_os_str())],
            &format!("current_index_repair_{label}_repeat"),
        );
        assert_eq!(
            workflow_schema19_durable_files(&workspace),
            repeated_source
        );
        assert_eq!(workflow_schema19_authority(&db), rows);
        let repeated: Value =
            serde_json::from_str(&repeated.stdout).expect("whole repeat JSON");
        assert_eq!(repeated["repaired"], false);
        assert!(
            repeated["recovery_audit"]["applied_actions"]
                .as_array()
                .is_none_or(Vec::is_empty)
        );
        assert!(!beads.join(".br_recovery").exists());
    }
}

/// Reconstruct a persisted undo intent, then commit newer work only to its
/// WAL. This exercises recovery admission, not abrupt process termination.
#[test]
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[allow(clippy::too_many_lines)]
fn prepared_release_undo_preserves_newer_wal_and_resumes_unchanged_control() {
    const NEW_TITLE: &str = "Newer work committed only to WAL after undo was prepared";
    for newer_write in [false, true] {
        let workspace = released_schema19_workspace();
        seed_schema19_database_authority(&workspace);
        let beads = workspace.root.join(".beads");
        let db = beads.join("beads.db");
        let original_files = schema19_tracker_files(&beads);
        let original_rows = schema19_authority_rows(&db, None);
        let plan = schema19_json(&run_br(
            &workspace,
            ["doctor", "migrate-schema", "plan", "--json"],
            "prepared_undo_plan",
        ));
        let applied = schema19_json(&run_br(
            &workspace,
            [
                "doctor",
                "migrate-schema",
                "apply",
                "--plan-token",
                plan["plan_token"].as_str().unwrap(),
                "--json",
            ],
            "prepared_undo_apply",
        ));
        let run_id = applied["run_id"].as_str().unwrap();
        let run_dir = beads.join(".br_recovery/schema-migrations").join(run_id);

        // Select WAL mode before capturing undo's exact live-family receipt.
        // The later edit must leave that main-file generation unchanged.
        let connection = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        connection.execute("PRAGMA journal_mode=WAL").unwrap();
        connection
            .execute("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        connection.close().unwrap();
        let mut prepared = schema19_json(&run_br(
            &workspace,
            [
                "doctor",
                "migrate-schema",
                "undo",
                run_id,
                "--dry-run",
                "--json",
            ],
            "prepared_undo_preview",
        ));
        assert_eq!(prepared["dry_run"], true);
        prepared["dry_run"] = json!(false);
        let quarantine = Path::new(prepared["quarantine_path"].as_str().unwrap());
        fs::create_dir_all(quarantine.parent().unwrap()).unwrap();
        fs::write(
            run_dir.join("undo-prepared.json"),
            serde_json::to_vec_pretty(&prepared).unwrap(),
        )
        .unwrap();
        let prepared_main = fs::read(&db).unwrap();

        if newer_write {
            let mut connection = Connection::open(db.to_string_lossy().into_owned()).unwrap();
            connection.execute("PRAGMA wal_autocheckpoint=0").unwrap();
            connection
                .execute_with_params(
                    "UPDATE issues SET title=? WHERE id='rel19-local'",
                    &[NEW_TITLE.into()],
                )
                .unwrap();
            connection.close_without_checkpoint_in_place().unwrap();
            drop(connection);
            assert_eq!(fs::read(&db).unwrap(), prepared_main);
            let wal = fs::read(beads.join("beads.db-wal")).unwrap();
            assert!(
                wal.windows(NEW_TITLE.len())
                    .any(|bytes| bytes == NEW_TITLE.as_bytes())
            );
            assert!(
                !prepared_main
                    .windows(NEW_TITLE.len())
                    .any(|bytes| bytes == NEW_TITLE.as_bytes())
            );
        }
        let before_resume = schema19_tracker_files(&beads);
        let rows_before_resume = schema19_authority_rows(&db, None);
        let resumed = run_br(
            &workspace,
            ["doctor", "migrate-schema", "undo", run_id, "--json"],
            "prepared_undo_resume",
        );

        if newer_write {
            assert!(!resumed.status.success(), "{}", resumed.stdout);
            assert!(!run_dir.join("undone.json").exists());
            assert_eq!(schema19_tracker_files(&beads), before_resume);
            assert_eq!(schema19_authority_rows(&db, None), rows_before_resume);
            let list = schema19_json(&run_br(
                &workspace,
                ["list", "--all", "--json"],
                "prepared_undo_newer_work_readable",
            ));
            assert!(
                list["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|issue| issue["title"] == NEW_TITLE)
            );
            assert_eq!(schema19_tracker_files(&beads), before_resume);
        } else {
            schema19_json(&resumed);
            assert!(run_dir.join("undone.json").is_file());
            let restored = schema19_tracker_files(&beads);
            for name in [
                "beads.db",
                "beads.db-wal",
                "beads.db-shm",
                "beads.db-journal",
                "issues.jsonl",
                "config.yaml",
            ] {
                assert_eq!(
                    restored.get(Path::new(name)),
                    original_files.get(Path::new(name))
                );
            }
            assert_eq!(schema19_authority_rows(&db, None), original_rows);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod native_writers {
    use super::{
        released_schema19_workspace, run_br, schema19_authority_rows, schema19_json,
        schema19_tracker_files, seed_schema19_database_authority,
    };
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::Duration;

    const NEW_TITLE: &str = "Native writer committed after migration contention refusal";
    const PEER_SCRIPT: &str = r#"
import json, sqlite3, sys

connection = sqlite3.connect(sys.argv[1], isolation_level=None, timeout=0.1)
assert connection.execute('PRAGMA journal_mode=WAL').fetchone()[0].lower() == 'wal'
connection.execute('PRAGMA wal_autocheckpoint=0')
assert connection.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone()[0] == 0
original = connection.execute("SELECT title FROM issues WHERE id='rel19-local'").fetchone()[0]
print(json.dumps({'phase': 'idle', 'title': original}), flush=True)
assert sys.stdin.readline().strip() == 'begin'
connection.execute('BEGIN IMMEDIATE')
assert connection.execute("UPDATE issues SET title=? WHERE id='rel19-local'", (sys.argv[2],)).rowcount == 1
assert connection.in_transaction
print(json.dumps({'phase': 'writing', 'in_transaction': connection.in_transaction}), flush=True)
decision = sys.stdin.readline().strip()
assert decision in ('commit', 'rollback'), decision
connection.execute(decision.upper())
assert not connection.in_transaction
title = connection.execute("SELECT title FROM issues WHERE id='rel19-local'").fetchone()[0]
connection.close()
print(json.dumps({'phase': 'closed', 'committed': decision == 'commit', 'title': title}), flush=True)
"#;

    /// A real native SQLite connection owns its locks until the test sends an
    /// explicit decision. Readiness and release use pipes, never a timed sleep.
    struct NativePeer {
        child: Child,
        stdin: ChildStdin,
        responses: Receiver<String>,
    }

    impl NativePeer {
        fn spawn(db: &Path) -> Self {
            let mut child = Command::new("python3")
                .args(["-u", "-c", PEER_SCRIPT])
                .arg(db)
                .arg(NEW_TITLE)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("python3 with the stdlib sqlite3 module is required for this regression");
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let (sender, responses) = mpsc::channel();
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
            });
            let peer = Self {
                child,
                stdin,
                responses,
            };
            peer.receive("idle");
            peer
        }

        fn receive(&self, phase: &str) -> Value {
            let line = self
                .responses
                .recv_timeout(Duration::from_secs(15))
                .unwrap_or_else(|error| panic!("native peer did not reach {phase}: {error}"));
            let response: Value = serde_json::from_str(&line).expect("native peer JSON response");
            assert_eq!(response["phase"], phase, "{response}");
            response
        }

        fn begin(&mut self) {
            writeln!(self.stdin, "begin").unwrap();
            self.stdin.flush().unwrap();
            assert_eq!(self.receive("writing")["in_transaction"], true);
        }

        fn finish(mut self, commit: bool) -> Value {
            writeln!(self.stdin, "{}", if commit { "commit" } else { "rollback" }).unwrap();
            self.stdin.flush().unwrap();
            let response = self.receive("closed");
            assert!(self.child.wait().unwrap().success());
            response
        }
    }

    impl Drop for NativePeer {
        fn drop(&mut self) {
            // Also release the peer if an assertion or handshake fails.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    type SourceFiles = BTreeMap<&'static str, (u64, u64, Vec<u8>)>;

    fn source_files(beads: &Path) -> SourceFiles {
        let mut files = BTreeMap::new();
        for name in [
            "beads.db",
            "beads.db-wal",
            "beads.db-shm",
            "beads.db-journal",
            "issues.jsonl",
            "issues.history.jsonl",
            "metadata.json",
            "config.yaml",
            "policy.yaml",
        ] {
            let path = beads.join(name);
            if path.is_file() {
                let metadata = path.metadata().unwrap();
                files.insert(
                    name,
                    (metadata.dev(), metadata.ino(), fs::read(path).unwrap()),
                );
            }
        }
        files
    }

    #[allow(clippy::too_many_lines)]
    fn migration_preserves_native_writer(operation: &str) {
        for commit_newer_work in [false, true] {
            let workspace = released_schema19_workspace();
            seed_schema19_database_authority(&workspace);
            let beads = workspace.root.join(".beads");
            let db = beads.join("beads.db");
            let original_files = schema19_tracker_files(&beads);
            let original_rows = schema19_authority_rows(&db, None);
            let run_id = (operation == "undo").then(|| {
                let plan = schema19_json(&run_br(
                    &workspace,
                    ["doctor", "migrate-schema", "plan", "--json"],
                    "native_undo_setup_plan",
                ));
                let applied = schema19_json(&run_br(
                    &workspace,
                    [
                        "doctor",
                        "migrate-schema",
                        "apply",
                        "--plan-token",
                        plan["plan_token"].as_str().unwrap(),
                        "--json",
                    ],
                    "native_undo_setup_apply",
                ));
                applied["run_id"].as_str().unwrap().to_owned()
            });
            let mut peer = NativePeer::spawn(&db);
            // Inspection remains observational even while a native connection
            // is open. This produces a genuine current plan or undo preview.
            let inspect_args = run_id.as_ref().map_or_else(
                || vec!["doctor", "migrate-schema", "plan", "--json"],
                |run_id| {
                    vec![
                        "doctor",
                        "migrate-schema",
                        "undo",
                        run_id,
                        "--dry-run",
                        "--json",
                    ]
                },
            );
            let inspected = schema19_json(&run_br(&workspace, inspect_args, "native_inspection"));
            let mut mutation = vec!["doctor", "migrate-schema", operation];
            if let Some(run_id) = &run_id {
                mutation.push(run_id);
            } else {
                mutation.extend(["--plan-token", inspected["plan_token"].as_str().unwrap()]);
            }
            mutation.extend([
                "--json",
                "--lock-timeout",
                "50",
                "--no-auto-import",
                "--no-auto-flush",
            ]);
            let before = source_files(&beads);
            let rows = schema19_authority_rows(&db, None);
            peer.begin();
            assert_eq!(
                source_files(&beads),
                before,
                "the native edit is uncommitted"
            );

            let refused = run_br(&workspace, mutation.clone(), "native_writer_refusal");
            assert!(!refused.status.success(), "{}", refused.stdout);
            let refusal: Value = serde_json::from_str(&refused.stdout).unwrap();
            assert_eq!(refusal["error"]["code"], "SYNC_CONFLICT", "{refusal}");
            assert_eq!(
                source_files(&beads),
                before,
                "refusal must not replace any inode or alter main/WAL/SHM/journal bytes"
            );
            assert_eq!(schema19_authority_rows(&db, None), rows);
            if let Some(run_id) = &run_id {
                let run = beads.join(".br_recovery/schema-migrations").join(run_id);
                assert!(!run.join("undo-prepared.json").exists());
                assert!(!run.join("undone.json").exists());
            } else {
                assert!(!beads.join(".br_recovery/schema-migrations").exists());
            }

            let closed = peer.finish(commit_newer_work);
            assert_eq!(closed["committed"], commit_newer_work);
            let after_peer = source_files(&beads);
            let committed_rows = schema19_authority_rows(&db, None);
            if commit_newer_work {
                assert_eq!(closed["title"], NEW_TITLE);
                let (columns, issue_rows) = &committed_rows["issues"];
                let title = columns.iter().position(|name| name == "title").unwrap();
                assert!(
                    issue_rows
                        .iter()
                        .any(|row| row[title] == format!("text {:?}", Some(NEW_TITLE))),
                    "the acknowledged native edit must remain in the active database"
                );
                let stale = run_br(&workspace, mutation, "native_writer_stale_operation");
                assert!(!stale.status.success(), "{}", stale.stdout);
                assert_eq!(source_files(&beads), after_peer);
                assert_eq!(schema19_authority_rows(&db, None), committed_rows);
                if operation == "undo" {
                    continue;
                }
                let refreshed = schema19_json(&run_br(
                    &workspace,
                    ["doctor", "migrate-schema", "plan", "--json"],
                    "native_committed_fresh_plan",
                ));
                schema19_json(&run_br(
                    &workspace,
                    [
                        "doctor",
                        "migrate-schema",
                        "apply",
                        "--plan-token",
                        refreshed["plan_token"].as_str().unwrap(),
                        "--json",
                    ],
                    "native_committed_fresh_apply",
                ));
            } else {
                schema19_json(&run_br(
                    &workspace,
                    mutation,
                    "native_writer_released_retry",
                ));
            }
            if operation == "apply" {
                assert_eq!(
                    schema19_authority_rows(&db, Some(&committed_rows)),
                    committed_rows
                );
                if !commit_newer_work {
                    // The guard leaves the old quiescent namespace record
                    // untouched. Ordinary pager admission must reopen the
                    // installed inode and complete a real write after release.
                    let title = "Ordinary br write after native peer release";
                    schema19_json(&run_br(
                        &workspace,
                        [
                            "update",
                            "rel19-local",
                            "--title",
                            title,
                            "--json",
                            "--no-auto-import",
                            "--no-auto-flush",
                        ],
                        "native_released_ordinary_write",
                    ));
                    let updated = schema19_authority_rows(&db, None);
                    let (columns, issues) = &updated["issues"];
                    let title_column = columns.iter().position(|name| name == "title").unwrap();
                    assert!(
                        issues
                            .iter()
                            .any(|row| { row[title_column] == format!("text {:?}", Some(title)) })
                    );
                }
            } else {
                let restored = schema19_tracker_files(&beads);
                for name in [
                    "beads.db",
                    "beads.db-wal",
                    "beads.db-shm",
                    "beads.db-journal",
                    "issues.jsonl",
                    "config.yaml",
                ] {
                    assert_eq!(
                        restored.get(Path::new(name)),
                        original_files.get(Path::new(name))
                    );
                }
                assert_eq!(schema19_authority_rows(&db, None), original_rows);
            }
        }
    }

    #[test]
    fn released_schema19_apply_excludes_native_writers_and_preserves_their_commits() {
        migration_preserves_native_writer("apply");
    }

    #[test]
    fn released_schema19_undo_excludes_native_writers_and_preserves_their_commits() {
        migration_preserves_native_writer("undo");
    }
}
