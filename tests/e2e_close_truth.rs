//! E2E tests for `br close` telling the truth about what it did
//! (bead `beads1-3c8h4`).
//!
//! Two defects are pinned here:
//!
//! 1. A `br close A B` batch where B was skipped exited 0 and, in
//!    `--json` mode, said nothing about B on ANY machine-readable
//!    surface: stdout carried the closed-issues array, stderr was empty.
//!    `br close A B && next-step` proceeded as though both had closed.
//! 2. The `hint` said "All specified issues were already closed or not
//!    found" for an open, existing, BLOCKED bead — contradicting the
//!    warning line printed two lines above it, which had correctly
//!    computed `blocked by: <id>`. Half that sentence described a state
//!    the code cannot even reach from here (a wholly unknown id fails at
//!    id resolution with `ISSUE_NOT_FOUND` before `close` runs).
//!
//! Every exit-code assertion reads the binary's own status via
//! `run_br`, never a pipeline's.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

/// The exact sentence the old hint printed regardless of what happened.
const OLD_WRONG_HINT: &str = "All specified issues were already closed or not found.";

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

fn workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

fn create(workspace: &BrWorkspace, title: &str) -> String {
    let created = run_br(workspace, ["create", title, "-p", "2"], "create");
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );
    parse_created_id(&created.stdout)
}

/// `blocked` cannot close until `blocker` does.
fn create_blocked_pair(workspace: &BrWorkspace) -> (String, String) {
    let blocker = create(workspace, "Blocker issue");
    let blocked = create(workspace, "Blocked issue");
    let dep = run_br(workspace, ["dep", "add", &blocked, &blocker], "dep_add");
    assert!(dep.status.success(), "dep add failed: {}", dep.stderr);
    (blocker, blocked)
}

fn status_of(workspace: &BrWorkspace, id: &str) -> String {
    let show = run_br(workspace, ["show", id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let value: Value =
        serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show json");
    // `show --json` returns either the issue or a single-element array.
    let issue = value.get(0).unwrap_or(&value);
    issue["status"].as_str().unwrap_or("<none>").to_string()
}

/// The `{"error": ...}` (or `{"notice": ...}`) envelope printed on stderr.
///
/// Reads the first JSON value at or after the envelope's opening brace and
/// ignores whatever else the command logged around it.
fn envelope(stderr: &str, key: &str) -> Value {
    let start = stderr
        .find(&format!("{{\n  \"{key}\""))
        .unwrap_or_else(|| panic!("no {key} envelope on stderr: {stderr}"));
    let value: Value = serde_json::Deserializer::from_str(&stderr[start..])
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no json after the {key} brace: {stderr}"))
        .unwrap_or_else(|err| panic!("envelope json: {err}: {stderr}"));
    value[key].clone()
}

// =============================================================================
// Defect 2: the hint must be the reason that was actually computed
// =============================================================================

#[test]
fn blocked_close_names_the_blocker_in_the_hint() {
    let workspace = workspace();
    let (blocker, blocked) = create_blocked_pair(&workspace);

    let closed = run_br(
        &workspace,
        ["close", &blocked, "--reason", "work done", "--json"],
        "close_blocked",
    );

    assert_eq!(
        closed.status.code(),
        Some(3),
        "a blocked close must fail: stderr={}",
        closed.stderr
    );
    let error = envelope(&closed.stderr, "error");
    assert_eq!(error["code"], "NOTHING_TO_DO");
    let hint = error["hint"].as_str().expect("hint");
    assert_ne!(hint, OLD_WRONG_HINT, "the generic sentence is back: {hint}");
    assert!(
        hint.contains(&blocker),
        "hint must name the blocker {blocker}: {hint}"
    );
    assert!(
        !hint.contains("already closed") && !hint.contains("not found"),
        "hint must not describe a state that did not happen: {hint}"
    );
    assert!(
        hint.contains("--force"),
        "hint must say what to do about it: {hint}"
    );
}

#[test]
fn blocked_close_is_machine_readable_without_parsing_prose() {
    let workspace = workspace();
    let (blocker, blocked) = create_blocked_pair(&workspace);

    let closed = run_br(
        &workspace,
        ["close", &blocked, "--json"],
        "close_blocked_context",
    );
    assert_eq!(closed.status.code(), Some(3), "stderr={}", closed.stderr);

    let context = envelope(&closed.stderr, "error")["context"].clone();
    assert_eq!(context["skipped"][0]["id"], blocked);
    // The discriminator: `blocked`, not a sentence to grep.
    assert_eq!(context["skipped"][0]["reason"], "blocked");
    assert_eq!(context["skipped"][0]["end_state_reached"], false);
    assert_eq!(
        context["skipped"][0]["blockers"][0],
        format!("{blocker}:open")
    );
    assert_eq!(context["outstanding"][0], blocked);
    assert_eq!(context["closed_count"], 0);
    assert_eq!(context["requested_count"], 1);
    assert_eq!(context["skip_reasons"][0], "blocked");
}

#[test]
fn the_hint_and_the_warning_line_say_the_same_thing() {
    // The two surfaces disagreeing is the defect: the warning had the
    // truth, the hint replaced it with a generic string.
    let workspace = workspace();
    let (blocker, blocked) = create_blocked_pair(&workspace);

    let human = run_br(&workspace, ["close", &blocked], "close_blocked_human");
    assert_eq!(human.status.code(), Some(3), "stderr={}", human.stderr);
    let warning_line = human
        .stderr
        .lines()
        .find(|line| line.contains("Skipped") && line.contains(&blocked))
        .unwrap_or_else(|| panic!("no warning line: {}", human.stderr))
        .to_string();
    let detail = warning_line
        .split_once(&format!("{blocked}: "))
        .map(|(_, rest)| rest.trim().to_string())
        .unwrap_or_else(|| panic!("no detail in warning line: {warning_line}"));
    assert!(detail.starts_with("blocked by"), "detail={detail}");
    // The human line must name the blocker in its own right (mutation M10:
    // degrading this to "blocked by dependencies" was invisible to every
    // other assertion, because the hint gets the id from the remedy and the
    // JSON from `blockers`).
    assert!(
        detail.contains(&blocker),
        "the warning line must name the blocker {blocker}: {detail}"
    );

    let hint = envelope(&human.stderr, "error")["hint"]
        .as_str()
        .expect("hint")
        .to_string();
    assert!(
        hint.contains(&detail),
        "hint {hint:?} must carry the reason the warning line printed ({detail:?})"
    );
}

#[test]
fn a_tombstone_is_not_reported_as_already_closed() {
    // A tombstone is a deleted bead, not finished work. Saying "already
    // closed" would tell an agent its work landed on an object that no
    // longer exists.
    let workspace = workspace();
    let doomed = create(&workspace, "Doomed issue");
    let deleted = run_br(&workspace, ["delete", &doomed, "--force"], "delete");
    assert!(
        deleted.status.success(),
        "delete failed: {}",
        deleted.stderr
    );

    let closed = run_br(&workspace, ["close", &doomed, "--json"], "close_tombstone");
    assert_eq!(
        closed.status.code(),
        Some(3),
        "closing a tombstone must fail: stderr={}",
        closed.stderr
    );
    let error = envelope(&closed.stderr, "error");
    let context = error["context"].clone();
    assert_eq!(context["skipped"][0]["reason"], "tombstoned");
    assert_eq!(
        context["skipped"][0]["end_state_reached"], false,
        "a tombstone is not the end state the caller asked for"
    );
    assert_eq!(context["outstanding"][0], doomed);
    let hint = error["hint"].as_str().expect("hint");
    assert!(hint.contains("tombstone"), "{hint}");
    assert!(
        !hint.contains("br reopen"),
        "a tombstone is not reopenable work: {hint}"
    );
}

// =============================================================================
// Defect 1: a skip must never be reported as success, and never silently
// =============================================================================

#[test]
fn a_partial_batch_closes_what_it_can_and_still_exits_non_zero() {
    let workspace = workspace();
    let (_blocker, blocked) = create_blocked_pair(&workspace);
    let first = create(&workspace, "Closable one");
    let last = create(&workspace, "Closable two");

    let closed = run_br(
        &workspace,
        [
            "close", &first, &blocked, &last, "--reason", "batch", "--json",
        ],
        "close_partial",
    );

    assert_eq!(
        closed.status.code(),
        Some(3),
        "a batch with an outstanding id must fail: stderr={}",
        closed.stderr
    );
    let error = envelope(&closed.stderr, "error");
    // A distinct code: "part of it happened" needs different recovery from
    // "nothing happened".
    assert_eq!(error["code"], "PARTIALLY_CLOSED");
    assert_eq!(error["context"]["closed_count"], 2);
    assert_eq!(error["context"]["outstanding"][0], blocked);

    // The writes that could happen did happen, and the one that could not
    // did not.
    assert_eq!(status_of(&workspace, &first), "closed");
    assert_eq!(status_of(&workspace, &last), "closed");
    assert_eq!(status_of(&workspace, &blocked), "open");

    // stdout stays bd-conformant: the issues that actually closed.
    let payload: Value =
        serde_json::from_str(&extract_json_payload(&closed.stdout)).expect("stdout json");
    let ids: Vec<&str> = payload
        .as_array()
        .expect("array")
        .iter()
        .map(|i| i["id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(ids, vec![first.as_str(), last.as_str()]);
    assert!(
        !ids.contains(&blocked.as_str()),
        "a skipped id must never appear as closed"
    );
}

#[test]
fn a_partial_batch_skip_is_visible_on_a_machine_readable_surface() {
    // This is the exact hole: in --json mode the skip used to appear
    // NOWHERE a program looks. The human warning line is not a
    // machine-readable surface.
    let workspace = workspace();
    let (_blocker, blocked) = create_blocked_pair(&workspace);
    let closable = create(&workspace, "Closable");

    let closed = run_br(
        &workspace,
        ["close", &closable, &blocked, "--json"],
        "close_partial_visible",
    );

    assert!(
        !closed.status.success(),
        "exit 0 tells `br close ... && next-step` the batch succeeded"
    );
    let context = envelope(&closed.stderr, "error")["context"].clone();
    let skipped = context["skipped"].as_array().expect("skipped array");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["id"], blocked);
    assert_eq!(skipped[0]["reason"], "blocked");
}

// =============================================================================
// Idempotency: already-closed satisfies the request, but is still named
// =============================================================================

#[test]
fn closing_an_already_closed_issue_succeeds_and_still_reports_the_skip() {
    let workspace = workspace();
    let id = create(&workspace, "Done already");
    let first = run_br(
        &workspace,
        ["close", &id, "--reason", "done"],
        "close_first",
    );
    assert!(first.status.success(), "stderr={}", first.stderr);

    let again = run_br(&workspace, ["close", &id, "--json"], "close_again");
    assert_eq!(
        again.status.code(),
        Some(0),
        "re-closing a closed issue is the requested end state: stderr={}",
        again.stderr
    );
    // Idempotent must not mean silent.
    let notice = envelope(&again.stderr, "notice");
    assert_eq!(notice["code"], "ALREADY_SATISFIED");
    assert_eq!(notice["context"]["skipped"][0]["id"], id);
    assert_eq!(notice["context"]["skipped"][0]["reason"], "already_closed");
    assert_eq!(notice["context"]["skipped"][0]["end_state_reached"], true);
    assert_eq!(
        notice["context"]["outstanding"],
        serde_json::json!([]),
        "nothing is outstanding when the world is as the caller asked"
    );
    let hint = notice["hint"].as_str().expect("hint");
    assert!(hint.contains("br reopen"), "{hint}");
    assert_ne!(hint, OLD_WRONG_HINT);
    // A success must not be dressed as an error.
    assert!(
        !again.stderr.contains("\"error\""),
        "exit 0 must not print an error envelope: {}",
        again.stderr
    );
}

#[test]
fn a_batch_of_only_already_closed_issues_exits_zero_with_every_skip_named() {
    let workspace = workspace();
    let first = create(&workspace, "Done one");
    let second = create(&workspace, "Done two");
    for id in [&first, &second] {
        let close = run_br(&workspace, ["close", id, "--reason", "done"], "close_setup");
        assert!(close.status.success(), "stderr={}", close.stderr);
    }

    let again = run_br(
        &workspace,
        ["close", &first, &second, "--json"],
        "close_all_already",
    );
    assert_eq!(
        again.status.code(),
        Some(0),
        "the world is as the caller asked: stderr={}",
        again.stderr
    );
    let notice = envelope(&again.stderr, "notice");
    assert_eq!(notice["context"]["skipped_count"], 2);
    let named: Vec<&str> = notice["context"]["skipped"]
        .as_array()
        .expect("skipped")
        .iter()
        .map(|s| s["id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(named, vec![first.as_str(), second.as_str()]);
}

#[test]
fn an_already_closed_id_mixed_with_a_blocked_one_still_fails() {
    let workspace = workspace();
    let (_blocker, blocked) = create_blocked_pair(&workspace);
    let done = create(&workspace, "Done");
    let close = run_br(&workspace, ["close", &done, "--reason", "d"], "close_setup");
    assert!(close.status.success(), "stderr={}", close.stderr);

    let mixed = run_br(
        &workspace,
        ["close", &done, &blocked, "--json"],
        "close_mixed",
    );
    assert_eq!(
        mixed.status.code(),
        Some(3),
        "one requested end state was not reached: stderr={}",
        mixed.stderr
    );
    let error = envelope(&mixed.stderr, "error");
    // Nothing was written, so this is NOTHING_TO_DO, not PARTIALLY_CLOSED.
    assert_eq!(error["code"], "NOTHING_TO_DO");
    let context = error["context"].clone();
    assert_eq!(context["outstanding"], serde_json::json!([blocked]));
    // Both skips are reported, each with its own reason.
    let reasons: Vec<&str> = context["skipped"]
        .as_array()
        .expect("skipped")
        .iter()
        .map(|s| s["reason"].as_str().unwrap_or_default())
        .collect();
    assert!(reasons.contains(&"blocked"), "{reasons:?}");
    assert!(reasons.contains(&"already_closed"), "{reasons:?}");
}

// =============================================================================
// The partial-apply decision itself
// =============================================================================

#[test]
fn closing_a_blocker_and_its_dependent_in_one_call_closes_both() {
    // Characterization test for the partial-apply decision: blocked-ness
    // is recomputed per id as the batch proceeds, so this invocation
    // succeeds completely today. An up-front, `br update`-style
    // whole-batch refusal would REFUSE A BATCH THAT WORKS. This test is
    // the thing that would break if someone added one, which is why it
    // exists.
    let workspace = workspace();
    let (blocker, blocked) = create_blocked_pair(&workspace);

    let closed = run_br(
        &workspace,
        ["close", &blocker, &blocked, "--reason", "cascade", "--json"],
        "close_cascade",
    );

    assert_eq!(
        closed.status.code(),
        Some(0),
        "closing the blocker first unblocks the dependent: stderr={}",
        closed.stderr
    );
    assert_eq!(status_of(&workspace, &blocker), "closed");
    assert_eq!(status_of(&workspace, &blocked), "closed");
    assert!(
        !closed.stderr.contains("\"error\""),
        "no error envelope on a complete success: {}",
        closed.stderr
    );
}

#[test]
fn force_closes_a_blocked_issue_and_reports_nothing_skipped() {
    let workspace = workspace();
    let (_blocker, blocked) = create_blocked_pair(&workspace);

    let closed = run_br(
        &workspace,
        [
            "close", &blocked, "--force", "--reason", "override", "--json",
        ],
        "close_force",
    );
    assert_eq!(closed.status.code(), Some(0), "stderr={}", closed.stderr);
    assert_eq!(status_of(&workspace, &blocked), "closed");
    assert!(
        !closed.stderr.contains("\"error\"") && !closed.stderr.contains("\"notice\""),
        "nothing was skipped, so nothing should be reported: {}",
        closed.stderr
    );
}
