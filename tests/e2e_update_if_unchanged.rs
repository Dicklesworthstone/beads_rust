//! E2E: `br update --if-unchanged` (GitHub #500).
//!
//! The lost update this guards against needs no concurrency to reproduce —
//! only a stale read. Two writers read the same prose field, each revises it,
//! and the second write discards the first. Both commands print the same
//! success line, because the second value is nearly the same length as the
//! first and shares nearly all of its words: it derives from the same base, so
//! on every axis the overwrite guard measures, it looks like a legitimate
//! revision.
//!
//! These tests drive the real binary, so they cover the wiring between the
//! flag, the parsed token and the in-transaction check — the storage-level
//! invariant is tested in `storage_crud.rs`, and the parsing in the
//! `update` unit tests.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, parse_created_id, run_br};
use serde_json::Value;

/// The reporter's base: long enough that a one-sentence append is well within
/// the overwrite guard's "this is a revision" band.
fn base_text() -> String {
    "Original paragraph one. ".repeat(12).trim_end().to_string()
}

fn init(workspace: &BrWorkspace, label: &str) {
    let init = run_br(workspace, ["init"], label);
    assert!(init.status.success(), "init failed: {}", init.stderr);
}

fn seed(workspace: &BrWorkspace, label: &str) -> (String, String) {
    let base = base_text();
    let created = run_br(
        workspace,
        [
            "create",
            "--title",
            "lost update probe",
            "-p",
            "3",
            "-t",
            "task",
            "-d",
            base.as_str(),
        ],
        label,
    );
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );
    let id = parse_created_id(&created.stdout);
    assert!(
        !id.is_empty(),
        "could not parse id from {:?}",
        created.stdout
    );

    let shown = run_br(workspace, ["show", id.as_str(), "--json"], label);
    let value: Value = serde_json::from_str(&extract_json_payload(&shown.stdout)).unwrap();
    let updated_at = value[0]["updated_at"]
        .as_str()
        .expect("show --json must expose updated_at")
        .to_string();
    (id, updated_at)
}

fn description_of(workspace: &BrWorkspace, id: &str, label: &str) -> String {
    let shown = run_br(workspace, ["show", id, "--json"], label);
    let value: Value = serde_json::from_str(&extract_json_payload(&shown.stdout)).unwrap();
    value[0]["description"].as_str().unwrap_or("").to_string()
}

#[test]
fn e2e_if_unchanged_refuses_a_stale_write_and_keeps_the_first_revision() {
    let _log = common::test_log("e2e_if_unchanged_stale");
    let workspace = BrWorkspace::new();
    init(&workspace, "e2e_if_unchanged_stale");
    let (id, base_token) = seed(&workspace, "e2e_if_unchanged_stale");
    let base = base_text();

    // Writer A lands first, without a precondition — the flag is opt-in and
    // must not become required.
    let a = run_br(
        &workspace,
        [
            "update",
            id.as_str(),
            "--description",
            &format!("{base} Writer A added a sentence here."),
        ],
        "e2e_if_unchanged_stale",
    );
    assert!(a.status.success(), "writer A failed: {}", a.stderr);

    // Writer B is still holding the token it read before A wrote.
    let b = run_br(
        &workspace,
        [
            "update",
            id.as_str(),
            "--description",
            &format!("{base} Writer B added a different one."),
            "--if-unchanged",
            base_token.as_str(),
        ],
        "e2e_if_unchanged_stale",
    );
    assert!(
        !b.status.success(),
        "a stale precondition must not succeed: {}",
        b.stdout
    );
    assert_eq!(
        b.status.code(),
        Some(6),
        "stale precondition must exit 6 (stdout: {}, stderr: {})",
        b.stdout,
        b.stderr
    );

    // The whole point: A's revision is still there.
    let final_text = description_of(&workspace, &id, "e2e_if_unchanged_stale");
    assert!(
        final_text.contains("Writer A added"),
        "A's revision was lost: {final_text:?}"
    );
    assert!(
        !final_text.contains("Writer B added"),
        "the refused write landed anyway: {final_text:?}"
    );
}

#[test]
fn e2e_if_unchanged_matching_token_applies_and_returns_a_usable_next_token() {
    let _log = common::test_log("e2e_if_unchanged_match");
    let workspace = BrWorkspace::new();
    init(&workspace, "e2e_if_unchanged_match");
    let (id, token) = seed(&workspace, "e2e_if_unchanged_match");
    let base = base_text();

    let ok = run_br(
        &workspace,
        [
            "update",
            id.as_str(),
            "--description",
            &format!("{base} A revision."),
            "--if-unchanged",
            token.as_str(),
        ],
        "e2e_if_unchanged_match",
    );
    assert!(
        ok.status.success(),
        "a current token must be accepted: {} {}",
        ok.stdout,
        ok.stderr
    );
    assert!(description_of(&workspace, &id, "e2e_if_unchanged_match").contains("A revision."));

    // A retry loop needs the new token to be readable and different, or it
    // cannot make progress after a refusal.
    let shown = run_br(
        &workspace,
        ["show", id.as_str(), "--json"],
        "e2e_if_unchanged_match",
    );
    let value: Value = serde_json::from_str(&extract_json_payload(&shown.stdout)).unwrap();
    let next = value[0]["updated_at"].as_str().expect("updated_at");
    assert_ne!(next, token, "a write must move the token");

    // And that new token works for the next write.
    let second = run_br(
        &workspace,
        [
            "update",
            id.as_str(),
            "--description",
            &format!("{base} A second revision."),
            "--if-unchanged",
            next,
        ],
        "e2e_if_unchanged_match",
    );
    assert!(
        second.status.success(),
        "re-read token must work: {} {}",
        second.stdout,
        second.stderr
    );
}

#[test]
fn e2e_if_unchanged_json_error_is_machine_actionable() {
    let _log = common::test_log("e2e_if_unchanged_json");
    let workspace = BrWorkspace::new();
    init(&workspace, "e2e_if_unchanged_json");
    let (id, base_token) = seed(&workspace, "e2e_if_unchanged_json");
    let base = base_text();

    run_br(
        &workspace,
        [
            "update",
            id.as_str(),
            "--description",
            &format!("{base} Moved."),
        ],
        "e2e_if_unchanged_json",
    );

    let refused = run_br(
        &workspace,
        [
            "update",
            id.as_str(),
            "--description",
            &format!("{base} Stale."),
            "--if-unchanged",
            base_token.as_str(),
            "--json",
        ],
        "e2e_if_unchanged_json",
    );
    assert_eq!(refused.status.code(), Some(6));

    let combined = format!("{}{}", refused.stdout, refused.stderr);
    let payload: Value = serde_json::from_str(&extract_json_payload(&combined))
        .unwrap_or_else(|e| panic!("expected JSON error, got {combined:?} ({e})"));
    let text = payload.to_string();
    assert!(
        text.contains("UPDATE_PRECONDITION_FAILED"),
        "error code missing: {text}"
    );
    // A caller deciding whether to retry needs both timestamps and the fact
    // that nothing was written.
    assert!(text.contains("expected_updated_at"), "{text}");
    assert!(text.contains("actual_updated_at"), "{text}");
    assert!(text.contains(&base_token), "{text}");
    assert!(
        text.contains("\"written\":false") || text.contains("\"written\": false"),
        "{text}"
    );
}

#[test]
fn e2e_if_unchanged_refuses_a_batch_and_a_malformed_token() {
    let _log = common::test_log("e2e_if_unchanged_guards");
    let workspace = BrWorkspace::new();
    init(&workspace, "e2e_if_unchanged_guards");
    let (first, token) = seed(&workspace, "e2e_if_unchanged_guards");
    let (second, _) = seed(&workspace, "e2e_if_unchanged_guards");

    // One `updated_at` cannot describe two issues. Checking it against only
    // the first would give the second exactly the false assurance the flag
    // exists to remove, so this is refused rather than partially applied.
    let batch = run_br(
        &workspace,
        [
            "update",
            first.as_str(),
            second.as_str(),
            "-p",
            "1",
            "--if-unchanged",
            token.as_str(),
        ],
        "e2e_if_unchanged_guards",
    );
    assert!(
        !batch.status.success(),
        "a batch with one token must be refused: {}",
        batch.stdout
    );

    // Neither issue may have been touched by the refusal.
    for id in [&first, &second] {
        let shown = run_br(
            &workspace,
            ["show", id.as_str(), "--json"],
            "e2e_if_unchanged_guards",
        );
        let value: Value = serde_json::from_str(&extract_json_payload(&shown.stdout)).unwrap();
        assert_eq!(
            value[0]["priority"].as_i64(),
            Some(3),
            "refused batch changed {id}"
        );
    }

    // A token that cannot be parsed must be an error, not a silently ignored
    // precondition.
    let malformed = run_br(
        &workspace,
        [
            "update",
            first.as_str(),
            "-p",
            "1",
            "--if-unchanged",
            "yesterday",
        ],
        "e2e_if_unchanged_guards",
    );
    assert!(
        !malformed.status.success(),
        "an unparseable token must not be ignored: {}",
        malformed.stdout
    );
}
