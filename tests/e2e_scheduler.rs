mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn create_issue(workspace: &BrWorkspace, title: &str, priority: &str) -> String {
    let result = run_br(
        workspace,
        ["create", title, "-p", priority, "-t", "task"],
        "create_issue",
    );
    assert!(result.status.success(), "create failed: {}", result.stderr);
    parse_created_id(&result.stdout)
}

fn create_labeled_issue(
    workspace: &BrWorkspace,
    title: &str,
    priority: &str,
    label: &str,
) -> String {
    let issue_id = create_issue(workspace, title, priority);
    let result = run_br(
        workspace,
        ["update", &issue_id, "--add-label", label],
        "label_issue",
    );
    assert!(result.status.success(), "label failed: {}", result.stderr);
    issue_id
}

fn scheduler_json(workspace: &BrWorkspace, args: &[&str], label: &str) -> Value {
    let result = run_br(workspace, args, label);
    assert!(
        result.status.success(),
        "scheduler failed: {}",
        result.stderr
    );
    let payload = extract_json_payload(&result.stdout);
    serde_json::from_str(&payload).expect("scheduler json")
}

fn assert_fresh_scheduler_claim_evidence(fresh: &[Value], assigned: &str, unassigned: &str) {
    assert_eq!(fresh[0]["issue"]["id"], unassigned);
    assert_eq!(fresh[0]["evidence"]["fairness"]["unassigned"], true);
    assert_eq!(
        fresh[0]["evidence"]["stale_claim"]["classification"],
        "unassigned"
    );
    assert_eq!(
        fresh[0]["evidence"]["stale_claim"]["recommended_action"],
        "observe"
    );
    assert_eq!(fresh[1]["issue"]["id"], assigned);
    assert_eq!(fresh[1]["evidence"]["fairness"]["contribution"], -2);
    assert_eq!(
        fresh[1]["evidence"]["stale_claim"]["owner_kind"],
        "swarm_agent"
    );
    assert_eq!(
        fresh[1]["evidence"]["stale_claim"]["reservation_status"],
        "no_snapshot"
    );
    assert_eq!(
        fresh[1]["evidence"]["stale_claim"]["classification"],
        "fresh"
    );
    assert_eq!(
        fresh[1]["evidence"]["stale_claim"]["recommended_action"],
        "observe"
    );
    assert_eq!(fresh[1]["evidence"]["stale_claim"]["is_stale"], false);
    assert!(fresh[1]["evidence"]["stale_claim"]["coordination_status_hint"].is_null());
}

fn assert_stale_scheduler_claim_evidence(assigned_row: &Value) {
    assert_eq!(
        assigned_row["evidence"]["stale_claim"]["assignee"],
        "agent-a"
    );
    assert_eq!(assigned_row["evidence"]["stale_claim"]["is_stale"], true);
    assert_eq!(
        assigned_row["evidence"]["stale_claim"]["classification"],
        "no_mail_snapshot"
    );
    assert_eq!(
        assigned_row["evidence"]["stale_claim"]["recommended_action"],
        "inspect_mail"
    );
    assert_eq!(
        assigned_row["evidence"]["stale_claim"]["reservation_status"],
        "no_snapshot"
    );
    assert!(
        assigned_row["evidence"]["stale_claim"]["coordination_status_hint"]
            .as_str()
            .expect("coordination hint")
            .contains("br coordination status")
    );
    assert_eq!(assigned_row["evidence"]["stale_claim"]["contribution"], 4);
    assert_eq!(
        assigned_row["evidence"]["stale_claim"]["stale_threshold_minutes"],
        0
    );
    assert!(
        assigned_row["rationale"]
            .as_array()
            .expect("rationale")
            .iter()
            .any(|line| line
                .as_str()
                .unwrap_or("")
                .contains("not abandonment proof"))
    );
}

#[test]
fn scheduler_json_ranks_ready_bottlenecks_with_evidence() {
    let _log = common::test_log("scheduler_json_ranks_ready_bottlenecks_with_evidence");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let foundation = create_issue(&workspace, "Foundation task", "1");
    let ui = create_issue(&workspace, "Independent UI task", "1");
    let follow_on = create_issue(&workspace, "Depends on foundation", "2");

    let label_foundation = run_br(
        &workspace,
        ["update", &foundation, "--add-label", "core"],
        "label_foundation",
    );
    assert!(
        label_foundation.status.success(),
        "label foundation failed: {}",
        label_foundation.stderr
    );
    let label_ui = run_br(&workspace, ["update", &ui, "--add-label", "ui"], "label_ui");
    assert!(
        label_ui.status.success(),
        "label ui failed: {}",
        label_ui.stderr
    );

    let dep = run_br(
        &workspace,
        ["dep", "add", &follow_on, &foundation],
        "add_dependency",
    );
    assert!(dep.status.success(), "dep add failed: {}", dep.stderr);

    let json = scheduler_json(
        &workspace,
        &["scheduler", "--json", "--limit", "2"],
        "scheduler_json",
    );
    let recommendations = json["recommendations"].as_array().expect("recommendations");

    assert_eq!(json["schema"], "br.scheduler.v1");
    assert_eq!(json["candidate_count"], 2);
    assert_eq!(recommendations.len(), 2);
    assert_eq!(recommendations[0]["issue"]["id"], foundation);
    assert_eq!(
        recommendations[0]["evidence"]["dependency_impact"]["dependent_count"],
        1
    );
    assert!(
        recommendations[0]["score"].as_i64().unwrap()
            > recommendations[1]["score"].as_i64().unwrap(),
        "dependency impact should break same-priority fallback ties"
    );
    assert_eq!(
        json["fallback_policy"]["sort"],
        "priority ASC, created_at ASC, id ASC"
    );
}

#[test]
fn scheduler_candidate_limit_refills_after_external_blockers() {
    let _log = common::test_log("scheduler_candidate_limit_refills_after_external_blockers");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let blocked_one = create_issue(&workspace, "Blocked external one", "0");
    let blocked_two = create_issue(&workspace, "Blocked external two", "0");
    let free_one = create_issue(&workspace, "Free local one", "1");
    let free_two = create_issue(&workspace, "Free local two", "1");

    for (label, issue_id) in [("block_one", &blocked_one), ("block_two", &blocked_two)] {
        let dep = run_br(
            &workspace,
            ["dep", "add", issue_id, "external:missing:capability"],
            label,
        );
        assert!(
            dep.status.success(),
            "external dep add failed: {}",
            dep.stderr
        );
    }

    let json = scheduler_json(
        &workspace,
        &[
            "scheduler",
            "--json",
            "--candidate-limit",
            "2",
            "--limit",
            "2",
        ],
        "scheduler_external_candidate_limit",
    );
    let recommendations = json["recommendations"].as_array().expect("recommendations");

    assert_eq!(json["candidate_count"], 2);
    assert_eq!(recommendations.len(), 2);
    let recommended_ids = recommendations
        .iter()
        .map(|item| item["issue"]["id"].as_str().expect("issue id"))
        .collect::<Vec<_>>();
    assert!(recommended_ids.contains(&free_one.as_str()));
    assert!(recommended_ids.contains(&free_two.as_str()));
    assert!(!recommended_ids.contains(&blocked_one.as_str()));
    assert!(!recommended_ids.contains(&blocked_two.as_str()));
}

#[test]
fn scheduler_candidate_limit_keeps_satisfied_external_prefix() {
    let _log = common::test_log("scheduler_candidate_limit_keeps_satisfied_external_prefix");
    let workspace = BrWorkspace::new();
    let external = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let init_external = run_br(&external, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    let config_path = workspace.root.join(".beads/config.yaml");
    let external_path = external.root.display();
    let config = format!("issue_prefix: bd\nexternal_projects:\n  extproj: \"{external_path}\"\n");
    fs::write(&config_path, config).expect("write config");

    let provider = create_labeled_issue(&external, "Provide auth", "1", "provides:auth");
    let close = run_br(&external, ["close", &provider], "close_provider");
    assert!(
        close.status.success(),
        "external close failed: {}",
        close.stderr
    );

    let external_one = create_issue(&workspace, "Satisfied external one", "0");
    let external_two = create_issue(&workspace, "Satisfied external two", "0");
    let local_one = create_issue(&workspace, "Free local one", "1");

    for (label, issue_id) in [
        ("dep_external_one", &external_one),
        ("dep_external_two", &external_two),
    ] {
        let dep = run_br(
            &workspace,
            ["dep", "add", issue_id, "external:extproj:auth"],
            label,
        );
        assert!(
            dep.status.success(),
            "external dep add failed: {}",
            dep.stderr
        );
    }

    let json = scheduler_json(
        &workspace,
        &[
            "scheduler",
            "--json",
            "--candidate-limit",
            "2",
            "--limit",
            "2",
        ],
        "scheduler_satisfied_external_candidate_limit",
    );
    let recommendations = json["recommendations"].as_array().expect("recommendations");
    let recommended_ids = recommendations
        .iter()
        .map(|item| item["issue"]["id"].as_str().expect("issue id"))
        .collect::<Vec<_>>();

    assert_eq!(json["candidate_count"], 2);
    assert_eq!(recommendations.len(), 2);
    assert!(recommended_ids.contains(&external_one.as_str()));
    assert!(recommended_ids.contains(&external_two.as_str()));
    assert!(!recommended_ids.contains(&local_one.as_str()));
}

#[test]
fn scheduler_stale_claim_and_fairness_evidence_are_parseable() {
    let _log = common::test_log("scheduler_stale_claim_and_fairness_evidence_are_parseable");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let assigned = create_issue(&workspace, "Assigned open task", "1");
    let unassigned = create_issue(&workspace, "Unassigned open task", "1");

    let assign = run_br(
        &workspace,
        ["update", &assigned, "--assignee", "agent-a"],
        "assign_issue",
    );
    assert!(assign.status.success(), "assign failed: {}", assign.stderr);

    let fresh_json = scheduler_json(
        &workspace,
        &[
            "scheduler",
            "--json",
            "--stale-claim-hours",
            "999999",
            "--limit",
            "2",
        ],
        "scheduler_fresh_claim",
    );
    let fresh = fresh_json["recommendations"]
        .as_array()
        .expect("fresh recommendations");
    assert_fresh_scheduler_claim_evidence(fresh, &assigned, &unassigned);

    let stale_json = scheduler_json(
        &workspace,
        &[
            "scheduler",
            "--json",
            "--stale-claim-hours",
            "0",
            "--limit",
            "2",
        ],
        "scheduler_stale_claim",
    );
    let stale = stale_json["recommendations"]
        .as_array()
        .expect("stale recommendations");
    let assigned_row = stale
        .iter()
        .find(|row| row["issue"]["id"] == assigned)
        .expect("assigned row");
    assert_stale_scheduler_claim_evidence(assigned_row);
}

#[test]
fn scheduler_wide_queue_diversifies_contention_domains() {
    let _log = common::test_log("scheduler_wide_queue_diversifies_contention_domains");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    for index in 0..5 {
        let _id = create_labeled_issue(
            &workspace,
            &format!("Backend queue task {index}"),
            "1",
            "backend",
        );
    }
    let docs = create_labeled_issue(&workspace, "Docs isolated task", "1", "docs");

    let json = scheduler_json(
        &workspace,
        &["scheduler", "--json", "--limit", "3"],
        "scheduler_wide_queue",
    );
    let recommendations = json["recommendations"].as_array().expect("recommendations");

    assert_eq!(json["candidate_count"], 6);
    assert_eq!(recommendations[0]["issue"]["id"], docs);
    assert_eq!(
        recommendations[0]["evidence"]["domain_contention"]["domain"],
        "docs"
    );
    assert_eq!(
        recommendations[0]["evidence"]["domain_contention"]["candidate_count_in_domain"],
        1
    );
    assert_eq!(
        recommendations[1]["evidence"]["domain_contention"]["domain"],
        "backend"
    );
    assert_eq!(
        recommendations[1]["evidence"]["domain_contention"]["candidate_count_in_domain"],
        5
    );
}

#[test]
fn scheduler_cycle_fixture_stays_parseable_after_cycle_rejection() {
    let _log = common::test_log("scheduler_cycle_fixture_stays_parseable_after_cycle_rejection");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let root = create_issue(&workspace, "Cycle root", "1");
    let dependent = create_issue(&workspace, "Cycle dependent", "1");

    let dep = run_br(
        &workspace,
        ["dep", "add", &dependent, &root],
        "add_cycle_base_dep",
    );
    assert!(dep.status.success(), "dep add failed: {}", dep.stderr);

    let cycle = run_br(
        &workspace,
        ["dep", "add", &root, &dependent],
        "reject_cycle_dep",
    );
    assert!(
        !cycle.status.success(),
        "cycle should be rejected: {}",
        cycle.stdout
    );

    let json = scheduler_json(
        &workspace,
        &["scheduler", "--json", "--limit", "1"],
        "scheduler_after_cycle_rejection",
    );
    let recommendations = json["recommendations"].as_array().expect("recommendations");

    assert_eq!(json["schema"], "br.scheduler.v1");
    assert_eq!(recommendations.len(), 1);
    assert_eq!(recommendations[0]["issue"]["id"], root);
    assert_eq!(
        recommendations[0]["evidence"]["dependency_impact"]["dependent_count"],
        1
    );
}

#[test]
fn scheduler_robot_output_has_stable_dry_run_shape() {
    let _log = common::test_log("scheduler_robot_output_has_stable_dry_run_shape");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let _id = create_labeled_issue(&workspace, "Robot parseable task", "2", "agents");

    let json = scheduler_json(
        &workspace,
        &["scheduler", "--robot", "--limit", "1"],
        "scheduler_robot_shape",
    );
    let recommendation = json["recommendations"]
        .as_array()
        .expect("recommendations")
        .first()
        .expect("recommendation");

    assert_eq!(json["schema"], "br.scheduler.v1");
    assert_eq!(json["returned_count"], 1);
    assert!(json["generated_at"].is_string());
    assert_eq!(
        json["fallback_policy"]["exhaustion_behavior"],
        "if scoring evidence is tied or incomplete, preserve fallback rank"
    );
    assert!(recommendation["rank"].is_u64());
    assert!(recommendation["fallback_rank"].is_u64());
    assert!(recommendation["score"].is_i64());
    assert!(recommendation["issue"]["id"].is_string());
    assert!(recommendation["evidence"]["priority"]["contribution"].is_i64());
    assert!(recommendation["evidence"]["dependency_impact"]["contribution"].is_i64());
    assert!(recommendation["evidence"]["stale_claim"]["owner_kind"].is_string());
    assert!(recommendation["evidence"]["stale_claim"]["classification"].is_string());
    assert!(recommendation["evidence"]["stale_claim"]["recommended_action"].is_string());
    assert!(recommendation["evidence"]["stale_claim"]["reservation_status"].is_string());
    assert!(recommendation["evidence"]["stale_claim"]["is_stale"].is_boolean());
    assert!(recommendation["evidence"]["fairness"]["reason"].is_string());
    assert!(recommendation["evidence"]["domain_contention"]["domain"].is_string());
    assert!(
        recommendation["rationale"]
            .as_array()
            .expect("rationale")
            .len()
            >= 4
    );
}

#[test]
fn scheduler_alias_emits_text_recommendations() {
    let _log = common::test_log("scheduler_alias_emits_text_recommendations");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let _id = create_issue(&workspace, "Standalone task", "2");

    let result = run_br(&workspace, ["schedule", "--limit", "1"], "schedule_text");
    assert!(
        result.status.success(),
        "schedule failed: {}",
        result.stderr
    );
    assert!(
        result.stdout.contains("Scheduler recommendations"),
        "unexpected scheduler text output: {}",
        result.stdout
    );
}
