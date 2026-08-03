//! E2E tests for the `comments` command.
//!
//! Comments are an issue's append-only, attributed HISTORY, as opposed to
//! `notes`/`design`/`acceptance_criteria`, which are replaceable STATE.
//! These tests exercise the whole path through the real binary: appending,
//! reading the complete log, the bounded view in `show`, the compact marker
//! in `list`, and search reaching comment text.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_env, run_br_with_stdin};
use serde_json::Value;
use tracing::info;

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

fn workspace_with_issue(title: &str) -> (BrWorkspace, String) {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", title], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);
    assert!(!id.is_empty(), "could not parse id from {:?}", create.stdout);
    (workspace, id)
}

fn add_comment(workspace: &BrWorkspace, id: &str, text: &str, label: &str) {
    let out = run_br(workspace, ["comments", "add", id, text], label);
    assert!(out.status.success(), "comments add failed: {}", out.stderr);
}

fn json_of(stdout: &str) -> Value {
    serde_json::from_str(&extract_json_payload(stdout)).expect("valid json")
}

// =============================================================================
// Append and read back
// =============================================================================

#[test]
fn comments_add_then_list_shows_author_and_body() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Issue with history");

    add_comment(&workspace, &id, "first observation", "add_1");
    add_comment(&workspace, &id, "second observation", "add_2");

    let list = run_br(&workspace, ["comments", &id], "list");
    assert!(list.status.success(), "comments failed: {}", list.stderr);
    assert!(
        list.stdout.contains(&format!("Comments on {id}")),
        "missing header: {}",
        list.stdout
    );
    assert!(list.stdout.contains("first observation"));
    assert!(list.stdout.contains("second observation"));
    // Chronological: the first thing said appears before the second.
    let first = list.stdout.find("first observation").unwrap();
    let second = list.stdout.find("second observation").unwrap();
    assert!(first < second, "comments must read forwards in time");
}

/// The author must appear in the *rendered* log, not merely in the row.
/// Attribution is the entire reason to prefer a comment over a hand-typed
/// line in `notes`, so a text view that shows the timestamp and body but
/// drops the name has failed at the one job the feature exists to do.
#[test]
fn comments_list_text_names_the_author() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Attributed in text");

    let add = run_br_with_env(
        &workspace,
        ["comments", "add", &id, "who said this matters"],
        [("BD_AGENT_ID", "planner9")],
        "add_named",
    );
    assert!(add.status.success(), "add failed: {}", add.stderr);

    let list = run_br(&workspace, ["comments", &id], "list_named");
    assert!(list.status.success(), "comments failed: {}", list.stderr);
    assert!(
        list.stdout.contains("planner9"),
        "the author must be visible in the text log, got: {}",
        list.stdout
    );

    let show = run_br(&workspace, ["show", &id], "show_named");
    assert!(
        show.stdout.contains("planner9"),
        "the author must be visible in show, got: {}",
        show.stdout
    );
}

/// A body is a verbatim record. The console parses what it is handed as
/// markup, so a body mentioning `[bold]` — entirely ordinary when the
/// subject is CLI output or formatting — would be read as a style tag and
/// silently deleted. Losing part of what someone wrote, invisibly, is
/// strictly worse than an ugly line: the reader cannot tell it happened.
#[test]
fn comment_body_containing_markup_renders_verbatim() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Markup in a body");

    let body = "use [bold] for headings and [red]for errors";
    add_comment(&workspace, &id, body, "add_markup");

    // Ground truth: JSON is not rendered, so it shows what was stored.
    let json = run_br(&workspace, ["comments", &id, "--json"], "list_markup_json");
    let comments = json_of(&json.stdout);
    assert_eq!(comments.as_array().expect("array")[0]["text"], body);

    for (args, label) in [
        (vec!["comments", id.as_str()], "list_markup_text"),
        (vec!["show", id.as_str()], "show_markup_text"),
    ] {
        let out = run_br(&workspace, args, label);
        assert!(out.status.success(), "{label} failed: {}", out.stderr);
        assert!(
            out.stdout.contains("[bold]") && out.stdout.contains("[red]"),
            "{label} dropped markup from the body, got: {}",
            out.stdout
        );
    }
}

/// Appending must not disturb what is already there. This is the property
/// that `bd update --notes` cannot offer: it replaces.
#[test]
fn comments_add_is_append_only() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Append only");

    for n in 0..5 {
        add_comment(&workspace, &id, &format!("entry {n}"), &format!("add_{n}"));
    }

    let list = run_br(&workspace, ["comments", &id, "--json"], "list_json");
    let comments = json_of(&list.stdout);
    let array = comments.as_array().expect("array of comments");
    assert_eq!(array.len(), 5, "every append must survive");
    for (n, comment) in array.iter().enumerate() {
        assert_eq!(comment["text"], format!("entry {n}"));
    }
}

/// The notes field is untouched by comments, and vice versa: they are
/// different storage, and conflating them would make
/// `bd show --json | jq .notes` lie.
#[test]
fn comments_do_not_touch_the_notes_field() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("State and history are separate");

    let update = run_br(
        &workspace,
        ["update", &id, "--notes=standing summary"],
        "update_notes",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    add_comment(&workspace, &id, "an observation", "add");

    let show = run_br(&workspace, ["show", &id, "--json"], "show_json");
    let issues = json_of(&show.stdout);
    assert_eq!(issues[0]["notes"], "standing summary");
    assert_eq!(issues[0]["comments"][0]["text"], "an observation");
}

#[test]
fn comments_list_on_issue_without_comments_says_so() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("No history yet");

    let list = run_br(&workspace, ["comments", &id], "list_empty");
    assert!(list.status.success(), "comments failed: {}", list.stderr);
    assert!(
        list.stdout.contains("No comments"),
        "expected an explicit empty statement, got {:?}",
        list.stdout
    );

    let list_json = run_br(&workspace, ["comments", &id, "--json"], "list_empty_json");
    assert_eq!(json_of(&list_json.stdout), Value::Array(vec![]));
}

#[test]
fn comments_add_json_returns_the_stored_comment() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("JSON add");

    let add = run_br(
        &workspace,
        ["comments", "add", &id, "machine readable", "--json"],
        "add_json",
    );
    assert!(add.status.success(), "add failed: {}", add.stderr);
    let comment = json_of(&add.stdout);
    assert_eq!(comment["text"], "machine readable");
    assert_eq!(comment["issue_id"], id.as_str());
    assert!(comment["author"].is_string());
    assert!(comment["created_at"].is_string());
}

// =============================================================================
// Body sources and sharp edges
// =============================================================================

/// A body that starts with `-` is text, not a flag. Markdown bullets are
/// exactly how an agent writes a multi-point note, so this must not be a
/// trap that forces `--flag=value` gymnastics.
#[test]
fn comments_add_accepts_text_starting_with_a_dash() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Leading dash");

    let add = run_br(
        &workspace,
        ["comments", "add", &id, "- decided to ship"],
        "add_dash",
    );
    assert!(add.status.success(), "add failed: {}", add.stderr);

    let list = run_br(&workspace, ["comments", &id], "list_dash");
    assert!(list.stdout.contains("- decided to ship"));
}

#[test]
fn comments_add_reads_body_from_a_file() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("From file");

    let path = workspace.root.join("body.md");
    std::fs::write(&path, "- line one\n- line two\n").expect("write body");

    let add = run_br(
        &workspace,
        ["comments", "add", &id, "-f", path.to_str().unwrap()],
        "add_file",
    );
    assert!(add.status.success(), "add failed: {}", add.stderr);

    let list = run_br(&workspace, ["comments", &id, "--json"], "list_file");
    let comments = json_of(&list.stdout);
    assert_eq!(comments[0]["text"], "- line one\n- line two\n");
}

#[test]
fn comments_add_reads_body_from_stdin() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("From stdin");

    let add = run_br_with_stdin(
        &workspace,
        ["comments", "add", &id, "-f", "-"],
        "piped body\n",
        "add_stdin",
    );
    assert!(add.status.success(), "add failed: {}", add.stderr);

    let list = run_br(&workspace, ["comments", &id, "--json"], "list_stdin");
    let comments = json_of(&list.stdout);
    assert_eq!(comments[0]["text"], "piped body\n");
}

#[test]
fn comments_add_rejects_text_and_file_together() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Ambiguous body");

    let path = workspace.root.join("body.md");
    std::fs::write(&path, "from the file").expect("write body");

    let add = run_br(
        &workspace,
        ["comments", "add", &id, "inline", "-f", path.to_str().unwrap()],
        "add_both",
    );
    assert!(
        !add.status.success(),
        "supplying two bodies must not silently pick one"
    );
}

#[test]
fn comments_add_requires_a_body() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("No body");

    let add = run_br(&workspace, ["comments", "add", &id], "add_nothing");
    assert!(!add.status.success(), "an empty invocation must fail");
}

#[test]
fn comments_on_unknown_issue_fails_cleanly() {
    common::init_test_logging();
    let (workspace, _id) = workspace_with_issue("Real issue");

    let add = run_br(
        &workspace,
        ["comments", "add", "no-such-issue", "hello"],
        "add_unknown",
    );
    assert!(!add.status.success(), "unknown issue must be an error");
    let combined = format!("{}{}", add.stdout, add.stderr);
    assert!(
        combined.to_lowercase().contains("not found"),
        "expected a not-found error, got {combined:?}"
    );
}

// =============================================================================
// Attribution
// =============================================================================

/// The author is the agent identity, matching `created_by` and event actors
/// — not the unix user.
#[test]
fn comments_are_attributed_to_the_agent_identity() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Attribution");

    let add = run_br_with_env(
        &workspace,
        ["comments", "add", &id, "from an agent", "--json"],
        [("BD_AGENT_ID", "planner7")],
        "add_as_agent",
    );
    assert!(add.status.success(), "add failed: {}", add.stderr);
    assert_eq!(json_of(&add.stdout)["author"], "planner7");
}

#[test]
fn comments_author_can_be_overridden_explicitly() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Explicit author");

    let add = run_br_with_env(
        &workspace,
        [
            "comments",
            "add",
            &id,
            "on behalf of someone else",
            "--author",
            "reviewer2",
            "--json",
        ],
        [("BD_AGENT_ID", "planner7")],
        "add_explicit_author",
    );
    assert!(add.status.success(), "add failed: {}", add.stderr);
    assert_eq!(json_of(&add.stdout)["author"], "reviewer2");
}

/// `reopen --reason` writes its reason through the same single writer, so a
/// reopen shows up in the comment log like any other entry.
#[test]
fn reopen_reason_lands_in_the_comment_log() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Reopened issue");

    let close = run_br(&workspace, ["close", &id], "close");
    assert!(close.status.success(), "close failed: {}", close.stderr);
    let reopen = run_br(
        &workspace,
        ["reopen", &id, "--reason", "regression came back"],
        "reopen",
    );
    assert!(reopen.status.success(), "reopen failed: {}", reopen.stderr);

    let list = run_br(&workspace, ["comments", &id, "--json"], "list_reopen");
    let comments = json_of(&list.stdout);
    let array = comments.as_array().expect("array");
    assert_eq!(array.len(), 1);
    assert_eq!(array[0]["text"], "Reopened: regression came back");
}

// =============================================================================
// Bounded display in `show`
// =============================================================================

#[test]
fn show_bounds_comments_and_declares_the_truncation() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Long history");

    for n in 0..8 {
        add_comment(&workspace, &id, &format!("entry {n}"), &format!("add_{n}"));
    }

    let show = run_br(&workspace, ["show", &id, "--json"], "show_default");
    let issues = json_of(&show.stdout);
    let issue = &issues[0];
    let shown = issue["comments"].as_array().expect("comments array");

    assert!(
        shown.len() < 8,
        "show must bound a long comment log, got {} entries",
        shown.len()
    );
    assert_eq!(
        issue["comment_count"], 8,
        "the true total must survive bounding"
    );
    assert_eq!(
        issue["comments_truncated"], true,
        "truncation must never be silent"
    );
    // The newest entries, in chronological order.
    assert_eq!(shown.last().unwrap()["text"], "entry 7");
    let texts: Vec<&str> = shown.iter().map(|c| c["text"].as_str().unwrap()).collect();
    let mut sorted = texts.clone();
    sorted.sort_unstable();
    assert_eq!(texts, sorted, "bounded window must stay chronological");
}

#[test]
fn show_comments_all_renders_the_complete_log() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Full history on request");

    for n in 0..8 {
        add_comment(&workspace, &id, &format!("entry {n}"), &format!("add_{n}"));
    }

    let show = run_br(
        &workspace,
        ["show", &id, "--comments", "all", "--json"],
        "show_all",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let issue = &json_of(&show.stdout)[0];
    assert_eq!(issue["comments"].as_array().unwrap().len(), 8);
    assert_eq!(issue["comment_count"], 8);
    assert!(
        issue.get("comments_truncated").is_none()
            || issue["comments_truncated"] == Value::Bool(false),
        "a complete view must not claim truncation"
    );
}

#[test]
fn show_comments_accepts_an_explicit_count() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Explicit bound");

    for n in 0..6 {
        add_comment(&workspace, &id, &format!("entry {n}"), &format!("add_{n}"));
    }

    let show = run_br(
        &workspace,
        ["show", &id, "--comments", "2", "--json"],
        "show_two",
    );
    let issue = &json_of(&show.stdout)[0];
    let shown = issue["comments"].as_array().unwrap();
    assert_eq!(shown.len(), 2);
    assert_eq!(shown[0]["text"], "entry 4");
    assert_eq!(shown[1]["text"], "entry 5");
    assert_eq!(issue["comment_count"], 6);
    assert_eq!(issue["comments_truncated"], true);
}

#[test]
fn show_comments_zero_hides_bodies_but_not_the_count() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Bodies suppressed");

    for n in 0..3 {
        add_comment(&workspace, &id, &format!("entry {n}"), &format!("add_{n}"));
    }

    let show = run_br(
        &workspace,
        ["show", &id, "--comments", "0", "--json"],
        "show_zero",
    );
    let issue = &json_of(&show.stdout)[0];
    assert!(
        issue.get("comments").is_none() || issue["comments"].as_array().unwrap().is_empty(),
        "no bodies should be rendered"
    );
    assert_eq!(issue["comment_count"], 3);
    assert_eq!(issue["comments_truncated"], true);
}

#[test]
fn show_text_points_at_the_full_log_when_bounding() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Text truncation notice");

    for n in 0..8 {
        add_comment(&workspace, &id, &format!("entry {n}"), &format!("add_{n}"));
    }

    let show = run_br(&workspace, ["show", &id], "show_text");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    assert!(
        show.stdout.contains("Comments (8)"),
        "heading must carry the total: {}",
        show.stdout
    );
    assert!(
        show.stdout.contains("hidden"),
        "text output must announce hidden comments: {}",
        show.stdout
    );
    assert!(
        show.stdout.contains(&format!("comments {id}")),
        "and name the command that shows them all: {}",
        show.stdout
    );
}

#[test]
fn show_comments_rejects_a_nonsense_bound() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Bad bound");

    let show = run_br(
        &workspace,
        ["show", &id, "--comments", "lots"],
        "show_bad_bound",
    );
    assert!(!show.status.success(), "a bad --comments value must error");
}

// =============================================================================
// `list` marker
// =============================================================================

#[test]
fn list_shows_a_compact_comment_marker_only_where_history_exists() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let with = run_br(&workspace, ["create", "Has comments"], "create_with");
    let without = run_br(&workspace, ["create", "Has none"], "create_without");
    let with_id = parse_created_id(&with.stdout);
    let without_id = parse_created_id(&without.stdout);

    add_comment(&workspace, &with_id, "something happened", "add");

    let list = run_br(&workspace, ["list"], "list_text");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let with_line = list
        .stdout
        .lines()
        .find(|line| line.contains(&with_id))
        .expect("commented issue listed");
    let without_line = list
        .stdout
        .lines()
        .find(|line| line.contains(&without_id))
        .expect("uncommented issue listed");

    // The marker is a trailing `[count·age]`: a count and an age, never a
    // body.
    let trailing = with_line.trim_end();
    assert!(
        trailing.ends_with(']') && trailing.contains("[1·"),
        "expected a trailing comment marker on {with_line:?}"
    );
    assert!(
        !with_line.contains("something happened"),
        "list must never print comment bodies: {with_line:?}"
    );
    assert!(
        !without_line.trim_end().ends_with(']'),
        "an issue with no comments should carry no marker: {without_line:?}"
    );
}

#[test]
fn list_json_carries_comment_count_and_recency() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Counted in list");
    add_comment(&workspace, &id, "one", "add_1");
    add_comment(&workspace, &id, "two", "add_2");

    let list = run_br(&workspace, ["list", "--json"], "list_json");
    let issues = json_of(&list.stdout);
    let row = issues
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id.as_str())
        .expect("issue in list");

    assert_eq!(row["comment_count"], 2);
    assert!(row["last_comment_at"].is_string());
    // Bodies stay out of listings.
    assert!(row.get("comments").is_none() || row["comments"].as_array().unwrap().is_empty());
}

// =============================================================================
// Search reaches comment text
// =============================================================================

#[test]
fn search_finds_text_that_lives_only_in_a_comment() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Unrevealing title");
    add_comment(
        &workspace,
        &id,
        "chose the streaming approach over batching",
        "add",
    );

    let search = run_br(&workspace, ["search", "streaming"], "search_text");
    assert!(search.status.success(), "search failed: {}", search.stderr);
    assert!(
        search.stdout.contains(&id),
        "comment text must be searchable: {}",
        search.stdout
    );
    // And the hit explains itself rather than looking arbitrary.
    assert!(
        search.stdout.contains("comment"),
        "a comment-sourced hit should say so: {}",
        search.stdout
    );
}

#[test]
fn search_json_marks_comment_sourced_hits() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Unrevealing title");
    add_comment(&workspace, &id, "distinctive-token appears here", "add");

    let search = run_br(
        &workspace,
        ["search", "distinctive-token", "--json"],
        "search_json",
    );
    let hits = json_of(&search.stdout);
    let row = hits
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id.as_str())
        .expect("issue found by comment text");
    assert_eq!(row["comment_match"], true);
    assert_eq!(row["comment_count"], 1);
}

#[test]
fn search_does_not_mark_title_only_hits_as_comment_matches() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("A memorable title token");
    add_comment(&workspace, &id, "unrelated remark", "add");

    let search = run_br(&workspace, ["search", "memorable", "--json"], "search_title");
    let hits = json_of(&search.stdout);
    let row = hits
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id.as_str())
        .expect("issue found by title");
    assert!(
        row.get("comment_match").is_none() || row["comment_match"] == Value::Bool(false),
        "a title hit must not claim to be a comment hit"
    );
    info!("search_does_not_mark_title_only_hits_as_comment_matches: ok");
}

// =============================================================================
// Export fidelity through the CLI
// =============================================================================

/// Bounding the `show` view must not touch what is written to JSONL: export
/// reads comments through its own bulk path. If those ever converged, a
/// display tweak would start silently deleting history on the next flush.
#[test]
fn every_comment_reaches_the_jsonl_export() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_issue("Exported history");

    for n in 0..9 {
        add_comment(&workspace, &id, &format!("entry {n}"), &format!("add_{n}"));
    }

    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let jsonl = std::fs::read_to_string(workspace.root.join(".beads").join("issues.jsonl"))
        .expect("read jsonl");
    let line = jsonl
        .lines()
        .find(|line| line.contains(&id))
        .expect("issue exported");
    let exported: Value = serde_json::from_str(line).expect("valid jsonl");
    let comments = exported["comments"].as_array().expect("comments exported");
    assert_eq!(
        comments.len(),
        9,
        "export must carry the whole log, not the displayed window"
    );
}
