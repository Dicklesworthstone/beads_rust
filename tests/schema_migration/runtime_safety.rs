//! Release-layout incompatibility must never authorize generic reconstruction.

use super::{
    released_schema19_workspace, run_br, schema19_authority_rows, schema19_json,
    schema19_tracker_files, seed_schema19_database_authority,
};
use crate::common::cli::run_br_with_env;
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
