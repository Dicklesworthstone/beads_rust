//! Release-layout incompatibility must never authorize generic reconstruction.

use super::{
    released_schema19_workspace, run_br, schema19_authority_rows, schema19_json,
    schema19_tracker_files, seed_schema19_database_authority,
};
use crate::common::cli::run_br_with_env;
use serde_json::{Value, json};

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
