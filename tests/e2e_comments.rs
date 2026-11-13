//! E2E tests for the `comments` command.
//!
//! Tests cover:
//! - Adding comments to issues
//! - Listing comments on issues
//! - JSON output validation
//! - Error cases (non-existent issues, empty comments)
//! - Edge cases (special characters, long comments, closed issues)

mod common;

use common::cli::{BrRun, BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    // Handle both formats: "Created bd-xxx: title" and "✓ Created bd-xxx: title"
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    let id_part = normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

fn successful_json(run: &BrRun) -> Value {
    assert!(
        run.status.success(),
        "command failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    serde_json::from_str(&extract_json_payload(&run.stdout)).expect("parse JSON result")
}

fn privacy_fixture(text: &str) -> (BrWorkspace, String, Value) {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "privacy"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = run_br(&workspace, ["create", "Comment privacy"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);
    assert!(!issue_id.is_empty(), "missing created issue id");
    let comment = successful_json(&run_br(
        &workspace,
        [
            "comments", "add", &issue_id, text, "--actor", "alice", "--json",
        ],
        "add_private_comment",
    ));
    (workspace, issue_id, comment)
}

fn listed_comments(workspace: &BrWorkspace, issue_id: &str, label: &str) -> Vec<Value> {
    successful_json(&run_br(
        workspace,
        ["comments", "list", issue_id, "--include-deleted", "--json"],
        label,
    ))
    .as_array()
    .expect("comment list")
    .clone()
}

fn assert_comment_identity(before: &Value, after: &Value) {
    for key in ["id", "author", "created_at"] {
        assert_eq!(before[key], after[key], "immutable comment field {key}");
    }
    assert_eq!(
        before["revision"]["identity"],
        after["revision"]["identity"]
    );
}

#[derive(Clone)]
struct CommentLedger {
    issues: String,
    history: String,
}

impl CommentLedger {
    fn read(workspace: &BrWorkspace) -> Self {
        let beads = workspace.root.join(".beads");
        Self {
            issues: std::fs::read_to_string(beads.join("issues.jsonl")).expect("issue ledger"),
            history: std::fs::read_to_string(beads.join("issues.history.jsonl"))
                .expect("portable audit history"),
        }
    }

    fn write(&self, workspace: &BrWorkspace) {
        let beads = workspace.root.join(".beads");
        std::fs::write(beads.join("issues.jsonl"), &self.issues).expect("pull issue ledger");
        std::fs::write(beads.join("issues.history.jsonl"), &self.history)
            .expect("pull portable audit history");
    }

    fn assert_private(&self, removed: &[&str]) {
        for &sentinel in removed {
            assert!(
                !self.issues.contains(sentinel),
                "removed text in current JSONL"
            );
            assert!(
                !self.history.contains(sentinel),
                "removed text in a retained portable history generation"
            );
        }
        // Multiple mutations retain two distinct generations. Check every row,
        // including the older checkpoint that a future import may replay.
        assert_eq!(self.history.lines().count(), 2);
        for line in self.history.lines() {
            let checkpoint: Value = serde_json::from_str(line).expect("history checkpoint");
            assert!(checkpoint["history"]["tables"].is_array());
        }
    }
}

fn clone_comment_workspace(source: &BrWorkspace) -> BrWorkspace {
    let target = BrWorkspace::new();
    let source_beads = source.root.join(".beads");
    let target_beads = target.root.join(".beads");
    std::fs::create_dir_all(&target_beads).expect("clone tracked directory");
    for name in [
        "issues.jsonl",
        "issues.history.jsonl",
        "config.yaml",
        "metadata.json",
        ".gitignore",
    ] {
        let path = source_beads.join(name);
        if path.exists() {
            std::fs::copy(path, target_beads.join(name)).expect("clone tracked file");
        }
    }
    let rebuild = run_br(&target, ["sync", "--import-only"], "clone_rebuild");
    assert!(
        rebuild.status.success(),
        "rebuild failed: {}",
        rebuild.stderr
    );
    target
}

fn import_and_flush(workspace: &BrWorkspace, ledger: &CommentLedger, label: &str) {
    ledger.write(workspace);
    let import = run_br(workspace, ["sync", "--import-only"], label);
    assert!(import.status.success(), "{label}: {}", import.stderr);
    let flush = run_br(
        workspace,
        ["sync", "--flush-only"],
        &format!("{label}_flush"),
    );
    assert!(flush.status.success(), "{label}: {}", flush.stderr);
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_comment_edit_preserves_identity_and_checks_author_and_revision() {
    let _log =
        common::test_log("e2e_comment_edit_preserves_identity_and_checks_author_and_revision");
    let (workspace, issue_id, original) = privacy_fixture("EDIT_ORIGINAL_PRIVATE_SENTINEL");
    let comment_id = original["id"].as_i64().unwrap().to_string();
    let original_head = original["revision"]["head"].as_str().unwrap();
    let denied = run_br(
        &workspace,
        [
            "comments",
            "edit",
            &issue_id,
            &comment_id,
            "unauthorized",
            "--actor",
            "bob",
        ],
        "deny_other_author",
    );
    assert!(!denied.status.success());
    let empty = run_br(
        &workspace,
        [
            "comments",
            "edit",
            &issue_id,
            &comment_id,
            "-m",
            "  ",
            "--actor",
            "alice",
        ],
        "deny_empty_edit",
    );
    assert!(!empty.status.success());
    assert_eq!(
        listed_comments(&workspace, &issue_id, "unchanged_before_edit"),
        [original.clone()]
    );

    let edited = successful_json(&run_br(
        &workspace,
        [
            "comments",
            "edit",
            &issue_id,
            &comment_id,
            "reviewed replacement",
            "--actor",
            "alice",
            "--if-revision",
            original_head,
            "--json",
        ],
        "edit_own_comment",
    ));
    assert_comment_identity(&original, &edited);
    assert_eq!(edited["text"], "reviewed replacement");
    assert_eq!(edited["revision"]["generation"], 1);
    assert_eq!(edited["revision"]["updated_by"], "alice");

    let forced = successful_json(&run_br(
        &workspace,
        [
            "comments",
            "edit",
            &issue_id,
            &comment_id,
            "moderator replacement",
            "--actor",
            "bob",
            "--force",
            "--if-revision",
            edited["revision"]["head"].as_str().unwrap(),
            "--json",
        ],
        "force_other_author",
    ));
    assert_comment_identity(&original, &forced);
    assert_eq!(forced["author"], "alice");
    assert_eq!(forced["revision"]["updated_by"], "bob");
    let stale = run_br(
        &workspace,
        [
            "comments",
            "edit",
            &issue_id,
            &comment_id,
            "stale write",
            "--actor",
            "alice",
            "--if-revision",
            original_head,
        ],
        "deny_stale_head",
    );
    assert!(!stale.status.success());
    assert_eq!(
        listed_comments(&workspace, &issue_id, "unchanged_after_stale"),
        [forced.clone()]
    );

    let audit = successful_json(&run_br(
        &workspace,
        ["audit", "log", &issue_id, "--json"],
        "audit_edits",
    ));
    let event = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event_type"] == "comment_edited" && event["actor"] == "bob")
        .expect("forced edit audit event");
    let evidence: Value = serde_json::from_str(event["comment"].as_str().unwrap()).unwrap();
    assert_eq!(evidence["forced"], true);
    assert_eq!(evidence["identity"], forced["revision"]["identity"]);
    assert_eq!(evidence["head"], forced["revision"]["head"]);
    CommentLedger::read(&workspace).assert_private(&["EDIT_ORIGINAL_PRIVATE_SENTINEL"]);
}

#[test]
fn e2e_comment_mutations_are_scoped_to_the_requested_issue() {
    let _log = common::test_log("e2e_comment_mutations_are_scoped_to_the_requested_issue");
    let (workspace, issue_id, original) = privacy_fixture("scope evidence");
    let create = run_br(&workspace, ["create", "Other issue"], "create_other");
    assert!(create.status.success());
    let other_id = parse_created_id(&create.stdout);
    let comment_id = original["id"].as_i64().unwrap().to_string();
    for operation in ["redact", "delete"] {
        let denied = run_br(
            &workspace,
            [
                "comments",
                operation,
                &other_id,
                &comment_id,
                "--actor",
                "alice",
                "--force",
            ],
            &format!("deny_wrong_issue_{operation}"),
        );
        assert!(
            !denied.status.success(),
            "wrong issue must not authorize {operation}"
        );
    }
    let denied = run_br(
        &workspace,
        [
            "comments",
            "edit",
            &other_id,
            &comment_id,
            "wrong issue",
            "--actor",
            "alice",
            "--force",
        ],
        "deny_wrong_issue_edit",
    );
    assert!(!denied.status.success());
    assert_eq!(
        listed_comments(&workspace, &issue_id, "original_unmodified"),
        [original]
    );
    assert!(listed_comments(&workspace, &other_id, "other_stays_empty").is_empty());
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_comment_redaction_and_deletion_keep_tombstones_and_metadata_only_audit() {
    let _log = common::test_log(
        "e2e_comment_redaction_and_deletion_keep_tombstones_and_metadata_only_audit",
    );
    let (workspace, issue_id, original) = privacy_fixture("REDACT_ORIGINAL_PRIVATE_SENTINEL");
    let comment_id = original["id"].as_i64().unwrap().to_string();
    let edited = successful_json(&run_br(
        &workspace,
        [
            "comments",
            "edit",
            &issue_id,
            &comment_id,
            "REDACT_EDITED_PRIVATE_SENTINEL",
            "--actor",
            "alice",
            "--json",
        ],
        "edit_before_redaction",
    ));
    for operation in ["redact", "delete"] {
        let denied = run_br(
            &workspace,
            [
                "comments",
                operation,
                &issue_id,
                &comment_id,
                "--actor",
                "bob",
            ],
            &format!("deny_other_author_{operation}"),
        );
        assert!(!denied.status.success());
    }
    let redacted = successful_json(&run_br(
        &workspace,
        [
            "comments",
            "redact",
            &issue_id,
            &comment_id,
            "--actor",
            "alice",
            "--if-revision",
            edited["revision"]["head"].as_str().unwrap(),
            "--json",
        ],
        "redact_comment",
    ));
    assert_comment_identity(&original, &redacted);
    assert_eq!(redacted["text"], "");
    assert_eq!(redacted["revision"]["state"], "redacted");
    let repeat = successful_json(&run_br(
        &workspace,
        [
            "comments",
            "redact",
            &issue_id,
            &comment_id,
            "--actor",
            "alice",
            "--json",
        ],
        "redact_again",
    ));
    assert_eq!(redacted, repeat);
    let restore = run_br(
        &workspace,
        [
            "comments",
            "edit",
            &issue_id,
            &comment_id,
            "revive removed text",
            "--actor",
            "alice",
            "--force",
        ],
        "deny_restore",
    );
    assert!(!restore.status.success());
    let plain = run_br(&workspace, ["comments", "list", &issue_id], "show_redacted");
    assert!(plain.status.success());
    assert!(plain.stdout.contains(&format!("#{comment_id}")));
    assert!(plain.stdout.contains("[redacted]"));

    let deleted = successful_json(&run_br(
        &workspace,
        [
            "comments",
            "delete",
            &issue_id,
            &comment_id,
            "--actor",
            "alice",
            "--json",
        ],
        "delete_comment",
    ));
    assert_comment_identity(&original, &deleted);
    assert_eq!(deleted["revision"]["state"], "deleted");
    assert_eq!(deleted["text"], "");
    let repeat = successful_json(&run_br(
        &workspace,
        [
            "comments",
            "delete",
            &issue_id,
            &comment_id,
            "--actor",
            "alice",
            "--json",
        ],
        "delete_again",
    ));
    assert_eq!(deleted, repeat);
    let visible = successful_json(&run_br(
        &workspace,
        ["comments", "list", &issue_id, "--json"],
        "hide_deleted",
    ));
    assert_eq!(visible, serde_json::json!([]));
    assert_eq!(
        listed_comments(&workspace, &issue_id, "show_tombstone"),
        [deleted.clone()]
    );
    let ledger = CommentLedger::read(&workspace);
    ledger.assert_private(&[
        "REDACT_ORIGINAL_PRIVATE_SENTINEL",
        "REDACT_EDITED_PRIVATE_SENTINEL",
    ]);
    let exported: Value = serde_json::from_str(ledger.issues.lines().next().unwrap()).unwrap();
    assert_eq!(exported["comments"][0]["revision"], deleted["revision"]);
    let audit = successful_json(&run_br(
        &workspace,
        ["audit", "log", &issue_id, "--json"],
        "audit_removals",
    ));
    for operation in ["comment_edited", "comment_redacted", "comment_deleted"] {
        assert!(
            audit["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["event_type"] == operation && event["actor"] == "alice")
        );
    }
    assert!(!audit.to_string().contains("PRIVATE_SENTINEL"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_comment_redaction_survives_divergent_machine_and_repeated_old_imports() {
    let _log = common::test_log(
        "e2e_comment_redaction_survives_divergent_machine_and_repeated_old_imports",
    );
    let (machine_a, issue_id, original) = privacy_fixture("SYNC_ORIGINAL_PRIVATE_SENTINEL");
    let original_ledger = CommentLedger::read(&machine_a);
    let mut legacy_issue: Value = serde_json::from_str(original_ledger.issues.trim()).unwrap();
    for comment in legacy_issue["comments"].as_array_mut().unwrap() {
        comment.as_object_mut().unwrap().remove("revision");
    }
    let legacy_ledger = CommentLedger {
        issues: format!("{legacy_issue}\n"),
        history: original_ledger.history.clone(),
    };
    let machine_b = clone_comment_workspace(&machine_a);
    let comment_id = original["id"].as_i64().unwrap().to_string();
    let peer_edit = successful_json(&run_br(
        &machine_b,
        [
            "comments",
            "edit",
            &issue_id,
            &comment_id,
            "SYNC_PEER_PRIVATE_SENTINEL",
            "--actor",
            "alice",
            "--json",
        ],
        "peer_edit",
    ));
    assert_eq!(peer_edit["revision"]["state"], "active");
    let mut peer_ledger = CommentLedger::read(&machine_b);
    // A skewed parent clock must not let timestamp-based issue selection
    // restore a removed comment. Keep the actual comment revision intact.
    let mut future_issue: Value = serde_json::from_str(peer_ledger.issues.trim()).unwrap();
    future_issue["updated_at"] = Value::String("2099-01-01T00:00:00Z".to_owned());
    peer_ledger.issues = format!("{future_issue}\n");
    let redacted = successful_json(&run_br(
        &machine_a,
        [
            "comments",
            "redact",
            &issue_id,
            &comment_id,
            "--actor",
            "alice",
            "--json",
        ],
        "local_redaction",
    ));
    import_and_flush(&machine_a, &peer_ledger, "import_future_peer_edit");
    assert_eq!(
        listed_comments(&machine_a, &issue_id, "redaction_wins_future_clock"),
        [redacted.clone()]
    );
    CommentLedger::read(&machine_a).assert_private(&[
        "SYNC_ORIGINAL_PRIVATE_SENTINEL",
        "SYNC_PEER_PRIVATE_SENTINEL",
    ]);

    for (attempt, older) in [&original_ledger, &legacy_ledger, &original_ledger]
        .into_iter()
        .enumerate()
    {
        import_and_flush(&machine_a, older, &format!("import_old_{attempt}"));
        assert_eq!(
            listed_comments(&machine_a, &issue_id, &format!("no_restore_{attempt}")),
            [redacted.clone()]
        );
        CommentLedger::read(&machine_a).assert_private(&[
            "SYNC_ORIGINAL_PRIVATE_SENTINEL",
            "SYNC_PEER_PRIVATE_SENTINEL",
        ]);
    }
    import_and_flush(
        &machine_b,
        &CommentLedger::read(&machine_a),
        "peer_import_redaction",
    );
    let peer_comments = listed_comments(&machine_b, &issue_id, "peer_redacted");
    assert_eq!(peer_comments.len(), 1);
    assert_eq!(peer_comments[0]["revision"], redacted["revision"]);
    assert_eq!(peer_comments[0]["text"], "");
    CommentLedger::read(&machine_b).assert_private(&[
        "SYNC_ORIGINAL_PRIVATE_SENTINEL",
        "SYNC_PEER_PRIVATE_SENTINEL",
    ]);

    let deleted = successful_json(&run_br(
        &machine_a,
        [
            "comments",
            "delete",
            &issue_id,
            &comment_id,
            "--actor",
            "alice",
            "--json",
        ],
        "delete_after_redaction",
    ));
    import_and_flush(
        &machine_b,
        &CommentLedger::read(&machine_a),
        "peer_import_deletion",
    );
    import_and_flush(&machine_b, &legacy_ledger, "peer_reimports_original");
    assert_eq!(
        listed_comments(&machine_b, &issue_id, "peer_deleted")[0]["revision"],
        deleted["revision"]
    );
    CommentLedger::read(&machine_b).assert_private(&[
        "SYNC_ORIGINAL_PRIVATE_SENTINEL",
        "SYNC_PEER_PRIVATE_SENTINEL",
    ]);
    // A machine with no database must rebuild the same terminal identity from
    // the JSONL tombstone; no local prior revision is needed to enforce it.
    let machine_c = clone_comment_workspace(&machine_b);
    let rebuilt = listed_comments(&machine_c, &issue_id, "fresh_machine_tombstone");
    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0]["revision"], deleted["revision"]);
    assert_eq!(rebuilt[0]["text"], "");
    let visible = successful_json(&run_br(
        &machine_c,
        ["comments", "list", &issue_id, "--json"],
        "fresh_machine_default_list",
    ));
    assert_eq!(visible, serde_json::json!([]));
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_comment_concurrent_edits_fail_closed_on_both_machines() {
    let _log = common::test_log("e2e_comment_concurrent_edits_fail_closed_on_both_machines");
    let (machine_a, issue_id, original) = privacy_fixture("CONCURRENT_ORIGINAL_PRIVATE_SENTINEL");
    let machine_b = clone_comment_workspace(&machine_a);
    let comment_id = original["id"].as_i64().unwrap().to_string();
    let edit_a = successful_json(&run_br(
        &machine_a,
        [
            "comments",
            "edit",
            &issue_id,
            &comment_id,
            "CONCURRENT_A_PRIVATE_SENTINEL",
            "--actor",
            "alice",
            "--json",
        ],
        "concurrent_edit_a",
    ));
    let edit_b = successful_json(&run_br(
        &machine_b,
        [
            "comments",
            "edit",
            &issue_id,
            &comment_id,
            "CONCURRENT_B_PRIVATE_SENTINEL",
            "--actor",
            "alice",
            "--json",
        ],
        "concurrent_edit_b",
    ));
    let ledger_a = CommentLedger::read(&machine_a);
    let ledger_b = CommentLedger::read(&machine_b);
    for (machine, incoming, local, label) in [
        (&machine_a, &ledger_b, &edit_a, "a"),
        (&machine_b, &ledger_a, &edit_b, "b"),
    ] {
        incoming.write(machine);
        for attempt in 0..2 {
            let conflict = run_br(
                machine,
                ["sync", "--import-only"],
                &format!("{label}_conflict_{attempt}"),
            );
            assert!(
                !conflict.status.success(),
                "divergent ordinary edits must not silently win"
            );
            assert!(!conflict.stdout.contains("PRIVATE_SENTINEL"));
            assert!(!conflict.stderr.contains("PRIVATE_SENTINEL"));
            let preserved = successful_json(&run_br(
                machine,
                [
                    "--no-auto-import",
                    "--allow-stale",
                    "comments",
                    "list",
                    &issue_id,
                    "--json",
                ],
                &format!("{label}_local_after_conflict_{attempt}"),
            ));
            assert_eq!(preserved, serde_json::json!([local]));
        }
        let rebuild = run_br(
            machine,
            ["sync", "--import-only", "--force"],
            &format!("{label}_forced_rebuild_conflict"),
        );
        assert!(
            !rebuild.status.success(),
            "force must not discard a concurrent comment revision"
        );
        assert!(!rebuild.stdout.contains("PRIVATE_SENTINEL"));
        assert!(!rebuild.stderr.contains("PRIVATE_SENTINEL"));
        let preserved = successful_json(&run_br(
            machine,
            [
                "--no-auto-import",
                "--allow-stale",
                "comments",
                "list",
                &issue_id,
                "--json",
            ],
            &format!("{label}_local_after_forced_conflict"),
        ));
        assert_eq!(preserved, serde_json::json!([local]));
    }
}

/// Test 1: Add single comment, verify in list
#[test]
fn e2e_comments_add_single_and_list() {
    let _log = common::test_log("e2e_comments_add_single_and_list");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "Test issue for comments"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);
    assert!(!id.is_empty(), "missing created id");

    // Add a comment
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "This is my first comment"],
        "add_comment",
    );
    assert!(add.status.success(), "add comment failed: {}", add.stderr);

    // List comments
    let list = run_br(&workspace, ["comments", "list", &id], "list_comments");
    assert!(
        list.status.success(),
        "list comments failed: {}",
        list.stderr
    );
    assert!(
        list.stdout.contains("This is my first comment"),
        "comment not found in list output"
    );
}

/// Test 2: Add multiple comments, verify order (newest last)
#[test]
fn e2e_comments_add_multiple_verify_order() {
    let _log = common::test_log("e2e_comments_add_multiple_verify_order");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Multiple comments test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add three comments
    let add1 = run_br(
        &workspace,
        ["comments", "add", &id, "First comment"],
        "add_comment1",
    );
    assert!(
        add1.status.success(),
        "add comment 1 failed: {}",
        add1.stderr
    );

    let add2 = run_br(
        &workspace,
        ["comments", "add", &id, "Second comment"],
        "add_comment2",
    );
    assert!(
        add2.status.success(),
        "add comment 2 failed: {}",
        add2.stderr
    );

    let add3 = run_br(
        &workspace,
        ["comments", "add", &id, "Third comment"],
        "add_comment3",
    );
    assert!(
        add3.status.success(),
        "add comment 3 failed: {}",
        add3.stderr
    );

    // Regression for #461: each `comments add` auto-flush must preserve the
    // earlier comment rows instead of repeating the newest row into every
    // pre-existing slot. Validate the published ledger directly so agreement
    // between two damaged query surfaces cannot make this test pass.
    let jsonl = std::fs::read_to_string(workspace.root.join(".beads/issues.jsonl"))
        .expect("read issues.jsonl");
    let exported_issue = jsonl
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("parse JSONL issue"))
        .find(|issue| issue["id"].as_str() == Some(id.as_str()))
        .expect("find commented issue in JSONL");
    let exported_comments = exported_issue["comments"]
        .as_array()
        .expect("comments array");
    let exported_ids: Vec<i64> = exported_comments
        .iter()
        .map(|comment| comment["id"].as_i64().expect("numeric comment id"))
        .collect();
    let exported_bodies: Vec<&str> = exported_comments
        .iter()
        .map(|comment| comment["text"].as_str().expect("comment body"))
        .collect();
    assert_eq!(exported_ids.len(), 3);
    assert!(
        exported_ids.windows(2).all(|pair| pair[0] != pair[1]),
        "comment ids must remain distinct in JSONL: {exported_ids:?}"
    );
    assert_eq!(
        exported_bodies,
        ["First comment", "Second comment", "Third comment"]
    );

    // List comments in JSON format to verify order
    let list = run_br(&workspace, ["comments", "list", &id, "--json"], "list_json");
    assert!(list.status.success(), "list json failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse comments json");

    assert_eq!(comments.len(), 3, "should have 3 comments");

    // Verify comments are in order (first, second, third)
    let texts: Vec<&str> = comments.iter().filter_map(|c| c["text"].as_str()).collect();
    assert_eq!(texts[0], "First comment");
    assert_eq!(texts[1], "Second comment");
    assert_eq!(texts[2], "Third comment");
}

/// Test 3: List comments with --json, validate structure
#[test]
fn e2e_comments_list_json_structure() {
    let _log = common::test_log("e2e_comments_list_json_structure");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "JSON structure test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add a comment with explicit author
    let add = run_br(
        &workspace,
        [
            "comments",
            "add",
            &id,
            "--author",
            "test-user",
            "JSON structure comment",
        ],
        "add_comment",
    );
    assert!(add.status.success(), "add comment failed: {}", add.stderr);

    // List in JSON format
    let list = run_br(&workspace, ["comments", "list", &id, "--json"], "list_json");
    assert!(list.status.success(), "list json failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse comments json");

    assert_eq!(comments.len(), 1, "should have 1 comment");
    let comment = &comments[0];

    // Validate structure
    assert!(
        comment["id"].is_number() || comment["id"].is_string(),
        "comment should have id"
    );
    assert_eq!(comment["text"], "JSON structure comment");
    assert_eq!(comment["author"], "test-user"); // invariant: hardcoded actor name, not an issue ID
    assert!(
        comment["created_at"].is_string(),
        "comment should have created_at"
    );
}

/// Test 4: Add comment to issue with existing comments
#[test]
fn e2e_comments_add_to_existing() {
    let _log = common::test_log("e2e_comments_add_to_existing");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Existing comments test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add first comment
    let add1 = run_br(
        &workspace,
        ["comments", "add", &id, "Existing comment"],
        "add_comment1",
    );
    assert!(
        add1.status.success(),
        "add comment 1 failed: {}",
        add1.stderr
    );

    // Verify one comment
    let list1 = run_br(&workspace, ["comments", "list", &id, "--json"], "list1");
    assert!(list1.status.success(), "list1 failed: {}", list1.stderr);
    let payload1 = extract_json_payload(&list1.stdout);
    let comments1: Vec<Value> = serde_json::from_str(&payload1).expect("parse json");
    assert_eq!(comments1.len(), 1, "should have 1 comment");

    // Add another comment
    let add2 = run_br(
        &workspace,
        ["comments", "add", &id, "New comment added"],
        "add_comment2",
    );
    assert!(
        add2.status.success(),
        "add comment 2 failed: {}",
        add2.stderr
    );

    // Verify two comments
    let list2 = run_br(&workspace, ["comments", "list", &id, "--json"], "list2");
    assert!(list2.status.success(), "list2 failed: {}", list2.stderr);
    let payload2 = extract_json_payload(&list2.stdout);
    let comments2: Vec<Value> = serde_json::from_str(&payload2).expect("parse json");
    assert_eq!(comments2.len(), 2, "should have 2 comments");
}

/// Test 5: Add comment to non-existent issue → error
#[test]
fn e2e_comments_add_nonexistent_issue() {
    let _log = common::test_log("e2e_comments_add_nonexistent_issue");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Try to add comment to non-existent issue
    let add = run_br(
        &workspace,
        ["comments", "add", "bd-nonexistent", "This should fail"],
        "add_nonexistent",
    );
    assert!(
        !add.status.success(),
        "add comment to non-existent issue should fail"
    );
    assert!(
        add.stderr.contains("not found")
            || add.stderr.contains("Issue")
            || add.stderr.contains("error"),
        "error message should indicate issue not found: {}",
        add.stderr
    );
}

/// Test 6: Add empty comment → error or rejection
#[test]
fn e2e_comments_add_empty() {
    let _log = common::test_log("e2e_comments_add_empty");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Empty comment test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Try to add empty comment (no text arguments)
    let add = run_br(&workspace, ["comments", "add", &id], "add_empty");
    // This might either fail or succeed with empty - check behavior
    // Most implementations reject empty comments
    if add.status.success() {
        // If it succeeded, verify comment list
        let list = run_br(
            &workspace,
            ["comments", "list", &id, "--json"],
            "list_empty",
        );
        let payload = extract_json_payload(&list.stdout);
        let comments: Vec<Value> = serde_json::from_str(&payload).unwrap_or_default();
        // Either no comment was added, or an empty comment exists
        assert!(
            comments.is_empty()
                || comments
                    .iter()
                    .all(|c| c["text"].as_str().is_none_or(str::is_empty)),
            "empty comment handling"
        );
    } else {
        // Expected: error for empty comment
        assert!(
            add.stderr.contains("empty")
                || add.stderr.contains("required")
                || add.stderr.contains("text"),
            "error message should indicate empty comment rejected: {}",
            add.stderr
        );
    }
}

/// Test 7: List comments on issue with no comments → empty list
#[test]
fn e2e_comments_list_empty() {
    let _log = common::test_log("e2e_comments_list_empty");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No comments issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // List comments on issue with no comments
    let list = run_br(
        &workspace,
        ["comments", "list", &id, "--json"],
        "list_empty",
    );
    assert!(
        list.status.success(),
        "list empty comments failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert!(comments.is_empty(), "should have 0 comments");
}

/// Test 8: Comment with special characters (quotes, newlines, unicode)
#[test]
fn e2e_comments_special_characters() {
    let _log = common::test_log("e2e_comments_special_characters");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Special chars test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add comment with special characters using --message flag for complex text
    let special_text = "Quote: \"hello\" and apostrophe's and emoji: 🚀";
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "--message", special_text],
        "add_special",
    );
    assert!(
        add.status.success(),
        "add special comment failed: {}",
        add.stderr
    );

    // Verify comment was stored correctly
    let list = run_br(
        &workspace,
        ["comments", "list", &id, "--json"],
        "list_special",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(comments.len(), 1, "should have 1 comment");

    let text = comments[0]["text"].as_str().expect("text field");
    assert!(text.contains("Quote:"), "should contain quote");
    assert!(text.contains("hello"), "should contain quoted text");
    assert!(
        text.contains("apostrophe") || text.contains('\''),
        "should contain apostrophe"
    );
}

/// Test 9: Very long comment (near limits)
#[test]
fn e2e_comments_long_text() {
    let _log = common::test_log("e2e_comments_long_text");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Long comment test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Create a long comment (10KB)
    let long_text = "x".repeat(10_000);
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "--message", &long_text],
        "add_long",
    );
    assert!(
        add.status.success(),
        "add long comment failed: {}",
        add.stderr
    );

    // Verify comment was stored
    let list = run_br(&workspace, ["comments", "list", &id, "--json"], "list_long");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(comments.len(), 1, "should have 1 comment");

    let text = comments[0]["text"].as_str().expect("text field");
    assert_eq!(text.len(), 10_000, "comment should be 10KB");
}

/// Test 10: Comment on closed issue (should work)
#[test]
fn e2e_comments_on_closed_issue() {
    let _log = common::test_log("e2e_comments_on_closed_issue");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Closed issue test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Close the issue
    let close = run_br(
        &workspace,
        ["close", &id, "--reason", "Testing closed comments"],
        "close_issue",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);

    // Add comment to closed issue
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "Comment on closed issue"],
        "add_closed",
    );
    assert!(
        add.status.success(),
        "add comment to closed issue failed: {}",
        add.stderr
    );

    // Verify comment was added
    let list = run_br(
        &workspace,
        ["comments", "list", &id, "--json"],
        "list_closed",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(comments.len(), 1, "should have 1 comment");
    assert_eq!(comments[0]["text"], "Comment on closed issue");
}

/// Test: Comments add with --json output
#[test]
fn e2e_comments_add_json_output() {
    let _log = common::test_log("e2e_comments_add_json_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "JSON add test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add comment with --json output
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "--json", "JSON output comment"],
        "add_json",
    );
    assert!(add.status.success(), "add json failed: {}", add.stderr);

    // Verify JSON output
    let payload = extract_json_payload(&add.stdout);
    let result: Value = serde_json::from_str(&payload).expect("parse add json");

    // The result should contain information about the added comment
    assert!(
        result.is_object() || result.is_array(),
        "add --json should return structured output"
    );
}

/// Test: Comments shorthand (br comments <id> = br comments list <id>)
#[test]
fn e2e_comments_shorthand() {
    let _log = common::test_log("e2e_comments_shorthand");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Shorthand test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add a comment
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "Shorthand comment"],
        "add_comment",
    );
    assert!(add.status.success(), "add comment failed: {}", add.stderr);

    // Use shorthand to list comments
    let list = run_br(&workspace, ["comments", &id], "list_shorthand");
    assert!(
        list.status.success(),
        "list shorthand failed: {}",
        list.stderr
    );
    assert!(
        list.stdout.contains("Shorthand comment"),
        "shorthand should list comments"
    );
}

/// Test: Comments are preserved in JSONL sync
#[test]
fn e2e_comments_sync_roundtrip() {
    let _log = common::test_log("e2e_comments_sync_roundtrip");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Sync roundtrip test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add comments
    let add1 = run_br(
        &workspace,
        ["comments", "add", &id, "First sync comment"],
        "add_comment1",
    );
    assert!(
        add1.status.success(),
        "add comment 1 failed: {}",
        add1.stderr
    );

    let add2 = run_br(
        &workspace,
        ["comments", "add", &id, "Second sync comment"],
        "add_comment2",
    );
    assert!(
        add2.status.success(),
        "add comment 2 failed: {}",
        add2.stderr
    );

    // Export to JSONL
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    // Create a new workspace and import
    let workspace2 = BrWorkspace::new();
    let init2 = run_br(&workspace2, ["init"], "init2");
    assert!(init2.status.success(), "init2 failed: {}", init2.stderr);

    // Copy JSONL to new workspace
    let jsonl_src = workspace.root.join(".beads").join("issues.jsonl");
    let jsonl_dst = workspace2.root.join(".beads").join("issues.jsonl");
    std::fs::copy(&jsonl_src, &jsonl_dst).expect("copy jsonl");

    // Import
    let import = run_br(
        &workspace2,
        ["sync", "--import-only", "--force"],
        "sync_import",
    );
    assert!(import.status.success(), "import failed: {}", import.stderr);

    // Verify comments were imported
    let list = run_br(
        &workspace2,
        ["comments", "list", &id, "--json"],
        "list_after_import",
    );
    assert!(
        list.status.success(),
        "list after import failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(comments.len(), 2, "should have 2 comments after import");

    let texts: Vec<&str> = comments.iter().filter_map(|c| c["text"].as_str()).collect();
    assert!(texts.contains(&"First sync comment"));
    assert!(texts.contains(&"Second sync comment"));
}

fn comment_texts(workspace: &BrWorkspace, id: &str, label: &str) -> Vec<(String, String)> {
    let list = run_br(workspace, ["comments", "list", id, "--json"], label);
    assert!(
        list.status.success(),
        "{label}: comments list failed: {}",
        list.stderr
    );
    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse comments json");
    comments
        .iter()
        .map(|comment| {
            (
                comment["author"].as_str().unwrap_or_default().to_string(),
                comment["text"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn jsonl_line_for(path: &std::path::Path, id: &str) -> String {
    let needle = format!("\"id\":\"{id}\"");
    std::fs::read_to_string(path)
        .expect("read issues.jsonl")
        .lines()
        .find(|line| line.contains(&needle))
        .expect("issues.jsonl must contain a line for the requested issue id")
        .to_string()
}

/// GitHub #486: two clones of one workspace each add one comment. Every
/// clone's AUTOINCREMENT hands out comment id 1, so the git-merged JSONL
/// carries the same positive comment id for two different issues. Importing
/// it must succeed on both clones (never refuse, never roll back), and the
/// documented "renumber one side" remedy must not break the other clone.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_comments_colliding_ids_from_two_clones_import_on_both_sides() {
    let _log = common::test_log("e2e_comments_colliding_ids_from_two_clones_import_on_both_sides");
    let clone_a = BrWorkspace::new();
    let clone_b = BrWorkspace::new();

    // 1. Clone A creates the shared workspace with one issue.
    let init = run_br(&clone_a, ["init", "--prefix", "demo"], "init_a");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create_a = run_br(
        &clone_a,
        ["create", "Issue from clone A", "--actor", "alice"],
        "create_a",
    );
    assert!(
        create_a.status.success(),
        "create failed: {}",
        create_a.stderr
    );
    let a_id = parse_created_id(&create_a.stdout);
    assert!(!a_id.is_empty(), "missing created id: {}", create_a.stdout);
    let flush_a = run_br(&clone_a, ["sync", "--flush-only"], "flush_a_initial");
    assert!(flush_a.status.success(), "flush failed: {}", flush_a.stderr);

    // 2. Clone B = git clone of A: tracked .beads files only, no database.
    let beads_a = clone_a.root.join(".beads");
    let beads_b = clone_b.root.join(".beads");
    std::fs::create_dir_all(&beads_b).expect("create clone B .beads");
    for name in ["issues.jsonl", "config.yaml", "metadata.json", ".gitignore"] {
        let src = beads_a.join(name);
        if src.exists() {
            std::fs::copy(&src, beads_b.join(name)).expect("copy tracked beads file");
        }
    }
    let warm_b = run_br(&clone_b, ["sync", "--import-only"], "sync_b_rebuild");
    assert!(
        warm_b.status.success(),
        "clone B rebuild failed: {}",
        warm_b.stderr
    );

    // 3. Each clone adds ONE comment to a different issue; both mint comment id 1.
    let comment_a = run_br(
        &clone_a,
        [
            "comments",
            "add",
            &a_id,
            "comment from clone A",
            "--actor",
            "alice",
        ],
        "comment_a",
    );
    assert!(
        comment_a.status.success(),
        "comment failed: {}",
        comment_a.stderr
    );
    let flush_a = run_br(&clone_a, ["sync", "--flush-only"], "flush_a_comment");
    assert!(flush_a.status.success(), "flush failed: {}", flush_a.stderr);

    let create_b = run_br(
        &clone_b,
        ["create", "Issue from clone B", "--actor", "bob"],
        "create_b",
    );
    assert!(
        create_b.status.success(),
        "create failed: {}",
        create_b.stderr
    );
    let b_id = parse_created_id(&create_b.stdout);
    assert!(!b_id.is_empty(), "missing created id: {}", create_b.stdout);
    let comment_b = run_br(
        &clone_b,
        [
            "comments",
            "add",
            &b_id,
            "comment from clone B",
            "--actor",
            "bob",
        ],
        "comment_b",
    );
    assert!(
        comment_b.status.success(),
        "comment failed: {}",
        comment_b.stderr
    );
    let flush_b = run_br(&clone_b, ["sync", "--flush-only"], "flush_b_comment");
    assert!(flush_b.status.success(), "flush failed: {}", flush_b.stderr);

    let jsonl_a = beads_a.join("issues.jsonl");
    let jsonl_b = beads_b.join("issues.jsonl");
    let line_a = jsonl_line_for(&jsonl_a, &a_id);
    let line_b = jsonl_line_for(&jsonl_b, &b_id);
    assert!(
        line_a.contains("\"id\":1,") && line_b.contains("\"id\":1,"),
        "both clones must have minted comment id 1: A={line_a} B={line_b}"
    );

    // 4. git merge: A keeps its own line and gains B's new line.
    let mut merged = std::fs::read_to_string(&jsonl_a).expect("read A jsonl");
    if !merged.ends_with('\n') {
        merged.push('\n');
    }
    merged.push_str(&line_b);
    merged.push('\n');
    std::fs::write(&jsonl_a, merged).expect("write merged jsonl");

    // 5. Clone A imports the merged JSONL (auto-import runs this on every command).
    let import_a = run_br(&clone_a, ["sync", "--import-only"], "import_a_merged");
    assert!(
        import_a.status.success(),
        "clone A must import colliding comment ids: {}",
        import_a.stderr
    );
    let list_a = run_br(&clone_a, ["list"], "list_a_after_merge");
    assert!(list_a.status.success(), "list failed: {}", list_a.stderr);
    let show_a = run_br(&clone_a, ["show", &a_id], "show_a_after_merge");
    assert!(show_a.status.success(), "show failed: {}", show_a.stderr);
    assert_eq!(
        comment_texts(&clone_a, &a_id, "a_comments_on_a"),
        vec![("alice".to_string(), "comment from clone A".to_string())]
    );
    assert_eq!(
        comment_texts(&clone_a, &b_id, "a_comments_on_b"),
        vec![("bob".to_string(), "comment from clone B".to_string())]
    );

    // 6. Clone A republishes (its local rowid for B's comment may differ) and
    //    clone B pulls that JSONL while its own DB still holds its comment as id 1.
    let flush_a = run_br(&clone_a, ["sync", "--flush-only"], "flush_a_after_merge");
    assert!(flush_a.status.success(), "flush failed: {}", flush_a.stderr);
    std::fs::copy(&jsonl_a, &jsonl_b).expect("pull merged jsonl into clone B");

    // 7. Clone B imports: its skipped local issue keeps comment id 1 while the
    //    peer's comment also claims id 1. This used to roll back every command.
    let import_b = run_br(&clone_b, ["sync", "--import-only"], "import_b_merged");
    assert!(
        import_b.status.success(),
        "clone B must import the peer's colliding comment id: {}",
        import_b.stderr
    );
    let list_b = run_br(&clone_b, ["list"], "list_b_after_merge");
    assert!(list_b.status.success(), "list failed: {}", list_b.stderr);
    assert_eq!(
        comment_texts(&clone_b, &a_id, "b_comments_on_a"),
        vec![("alice".to_string(), "comment from clone A".to_string())]
    );
    assert_eq!(
        comment_texts(&clone_b, &b_id, "b_comments_on_b"),
        vec![("bob".to_string(), "comment from clone B".to_string())]
    );

    // Both clones converge: a further round trip is a no-op on each side.
    let flush_b = run_br(&clone_b, ["sync", "--flush-only"], "flush_b_after_merge");
    assert!(flush_b.status.success(), "flush failed: {}", flush_b.stderr);
    std::fs::copy(&jsonl_b, &jsonl_a).expect("pull clone B jsonl into clone A");
    let import_a = run_br(&clone_a, ["sync", "--import-only"], "import_a_roundtrip");
    assert!(
        import_a.status.success(),
        "round-trip import must succeed: {}",
        import_a.stderr
    );
    assert_eq!(
        comment_texts(&clone_a, &b_id, "a_comments_on_b_roundtrip"),
        vec![("bob".to_string(), "comment from clone B".to_string())]
    );
}
