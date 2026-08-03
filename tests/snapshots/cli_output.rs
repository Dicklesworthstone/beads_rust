use super::common::cli::{BrWorkspace, run_br};
use super::{
    compose_invocation, create_issue, init_workspace, normalize_output,
    normalize_output_with_age_masking, parse_created_id,
};
use insta::assert_snapshot;

#[cfg(feature = "self_update")]
#[test]
fn snapshot_help_output() {
    let workspace = BrWorkspace::new();
    let output = run_br(&workspace, ["--help"], "help");
    assert!(output.status.success(), "help failed: {}", output.stderr);
    assert_snapshot!("help_output", normalize_output(&output.stdout));
}

#[test]
#[cfg(not(feature = "self_update"))]
fn snapshot_help_output_no_upgrade() {
    let workspace = BrWorkspace::new();
    let output = run_br(&workspace, ["--help"], "help");
    assert!(output.status.success(), "help failed: {}", output.stderr);
    let stdout = &output.stdout;
    assert!(
        !stdout.contains("upgrade"),
        "help should not list upgrade subcommand without self_update feature"
    );
    for cmd in ["create", "list", "show", "close", "search"] {
        assert!(
            stdout.contains(cmd),
            "help should list core subcommand '{cmd}'"
        );
    }
}

#[test]
fn snapshot_create_help() {
    let workspace = BrWorkspace::new();
    let output = run_br(&workspace, ["create", "--help"], "create_help");
    assert!(
        output.status.success(),
        "create help failed: {}",
        output.stderr
    );
    assert_snapshot!("create_help", normalize_output(&output.stdout));
}

/// `br list` on a workspace with no issues.
///
/// This snapshot recorded ZERO BYTES. That expectation cannot fail: it is
/// equally satisfied by `br list` working correctly, by `br list` dying
/// before it printed anything, by its output going to stderr, and by the
/// subcommand being deleted — every one of those exits 0 and prints nothing
/// to stdout. It sat in the suite looking like a test of `br list`.
///
/// The silence itself is correct and is NOT a bug: `src/cli/commands/list.rs`
/// keeps stdout empty here deliberately, to match the Go `bd` reference
/// implementation (there are four conformance suites riding on that), and
/// `br list | wc -l` scripts depend on it. So the fix is to state the
/// expectation in a form that can fail, not to make the command speak.
///
/// The composed value pins all three facts — exit status, stdout, stderr —
/// each labelled. `stdout: <empty>` is now an assertion. The stderr block
/// pins that the command actually did its work (path validation, auto-import
/// of zero records, blocked-cache rebuild) rather than exiting early, which
/// is what makes "printed nothing" distinguishable from "did nothing".
#[test]
fn snapshot_list_empty() {
    let workspace = init_workspace();
    let output = run_br(&workspace, ["list"], "list_empty");
    assert!(output.status.success(), "list failed: {}", output.stderr);
    assert_snapshot!(
        "list_empty",
        compose_invocation("br list", &output.stdout, &output.stderr, output.status)
    );
}

#[test]
fn snapshot_list_with_issues() {
    let workspace = init_workspace();
    create_issue(&workspace, "Bug: Fix login", "create_bug");
    create_issue(&workspace, "Feature: Add dark mode", "create_feature");
    create_issue(&workspace, "Task: Update docs", "create_task");

    let output = run_br(&workspace, ["list"], "list_with_issues");
    assert!(output.status.success(), "list failed: {}", output.stderr);
    // Issues are created and immediately listed, so the rendered age
    // (`0s`, occasionally `1s` under load) is inherently time-
    // dependent — masked rather than asserted on literally.
    assert_snapshot!(
        "list_with_issues",
        normalize_output_with_age_masking(&output.stdout)
    );
}

/// `br show` on an issue that HAS a description.
///
/// The issue was titled "Test issue with description" and had none: the
/// fixture never passed `-d`, so the recorded value stopped at the metadata
/// header and the description render path — the one place `br show` puts
/// stored prose on screen — was not in any snapshot. That path is exactly
/// where the stray-backslash regression shipped, an over-eager markup escape
/// applied at a sink that does not parse markup, printing `use \[bold]` for
/// a body that says `use [bold]`.
///
/// Both the title and the description now carry bracketed text, so this
/// snapshot pins both directions of the markup bug: content EATEN (the
/// brackets vanishing, as `br search` once did to a `[task]` badge) and
/// content OVER-ESCAPED (a backslash appearing). Either one changes these
/// bytes, and `no_snapshot_records_a_markup_escape_artifact` in
/// tests/snapshot_hygiene.rs fails on the escape direction even if someone
/// re-records.
///
/// The description is deliberately a full open/close tag pair around a word
/// (`[red]errors[/red]`), the shape a markup parser is most likely to
/// consume, while still reading as an ordinary sentence — so the recorded
/// value can be reviewed as correct on its face. A snapshot whose expected
/// text looks half-eaten even when it is right teaches the next reader to
/// shrug at damage.
#[test]
fn snapshot_show_output() {
    let workspace = init_workspace();
    let created = run_br(
        &workspace,
        [
            "create",
            "Test issue with [brackets] in the title",
            "-d",
            "Use [bold] for headings and [red]errors[/red] for failures.",
        ],
        "create_show",
    );
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );
    let id = parse_created_id(&created.stdout);

    let output = run_br(&workspace, ["show", &id], "show_text");
    assert!(output.status.success(), "show failed: {}", output.stderr);
    let normalized = normalize_output(&output.stdout);
    // Belt and braces, in the two directions this has actually failed:
    // asserted here as well as pinned in the snapshot, so a re-record cannot
    // quietly bless either one.
    for expected in ["[brackets]", "[bold]", "[red]errors[/red]"] {
        assert!(
            normalized.contains(expected),
            "bracketed text {expected:?} was eaten between storage and \
             screen:\n{normalized}"
        );
    }
    assert!(
        !normalized.contains("\\["),
        "a markup escape reached the screen; the stored text has no \
         backslash:\n{normalized}"
    );
    assert_snapshot!("show_output", normalized);
}

#[test]
fn snapshot_blocked_output() {
    let workspace = init_workspace();

    // Create dependency chain
    let blocker = create_issue(&workspace, "Database schema", "create_blocker");
    let blocked1 = create_issue(&workspace, "User model", "create_blocked1");
    let blocked2 = create_issue(&workspace, "Auth module", "create_blocked2");

    let _ = run_br(&workspace, ["dep", "add", &blocked1, &blocker], "dep_add1");
    let _ = run_br(&workspace, ["dep", "add", &blocked2, &blocked1], "dep_add2");

    let output = run_br(&workspace, ["blocked"], "blocked_text");
    assert!(output.status.success(), "blocked failed: {}", output.stderr);
    assert_snapshot!("blocked_output", normalize_output(&output.stdout));
}

#[test]
fn snapshot_stats_output() {
    let workspace = init_workspace();

    // Create mixed state issues
    let id1 = create_issue(&workspace, "Open issue 1", "create_open1");
    let id2 = create_issue(&workspace, "Open issue 2", "create_open2");
    let id3 = create_issue(&workspace, "Will close", "create_close");

    // Close one issue
    let _ = run_br(&workspace, ["close", &id3], "close_issue");

    // Add a dependency
    let _ = run_br(&workspace, ["dep", "add", &id2, &id1], "dep_add_stats");

    let output = run_br(&workspace, ["stats"], "stats_text");
    assert!(output.status.success(), "stats failed: {}", output.stderr);
    assert_snapshot!("stats_output", normalize_output(&output.stdout));
}

#[test]
fn snapshot_doctor_output() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let output = run_br(&workspace, ["doctor"], "doctor");
    assert_snapshot!("doctor_output", normalize_output(&output.stdout));
}

#[test]
fn snapshot_version_output() {
    let workspace = BrWorkspace::new();
    let output = run_br(&workspace, ["version"], "version");
    assert_snapshot!("version_output", normalize_output(&output.stdout));
}

#[test]
fn snapshot_reopen_output() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Issue to reopen", "create_for_reopen");

    // Close the issue first
    let close = run_br(&workspace, ["close", &id], "close_for_reopen");
    assert!(close.status.success(), "close failed: {}", close.stderr);

    // Now reopen it
    let output = run_br(&workspace, ["reopen", &id], "reopen");
    assert!(output.status.success(), "reopen failed: {}", output.stderr);
    assert_snapshot!("reopen_output", normalize_output(&output.stdout));
}

#[test]
fn snapshot_search_output() {
    let workspace = init_workspace();

    // Create issues with searchable content
    create_issue(&workspace, "Authentication bug in login", "create_search1");
    create_issue(&workspace, "Payment processing feature", "create_search2");
    create_issue(&workspace, "User login flow improvement", "create_search3");

    // Search for "login"
    let output = run_br(&workspace, ["search", "login"], "search_login");
    assert!(output.status.success(), "search failed: {}", output.stderr);
    // Same age-masking rationale as snapshot_list_with_issues above:
    // `bd search`'s plain-text path shares `format_issue_line_with`.
    //
    // The `[task]` badge in this snapshot used to be an empty gap: search
    // emitted the line through `ctx.print`, which parses markup, and the
    // parser ate `[task]` as a style tag. The recorded snapshot preserved the
    // loss instead of catching it, because nothing here knew what the line was
    // supposed to say. It now matches `bd list`, which never lost the badge —
    // see docs/ARCHITECTURE.md "Output Safety".
    assert_snapshot!(
        "search_output",
        normalize_output_with_age_masking(&output.stdout)
    );
}

#[test]
fn snapshot_count_output() {
    let workspace = init_workspace();

    // Create issues with different statuses and types
    let id1 = create_issue(&workspace, "Bug one", "create_count1");
    let id2 = create_issue(&workspace, "Bug two", "create_count2");
    let id3 = create_issue(&workspace, "Feature one", "create_count3");

    // Update types and close one
    let _ = run_br(
        &workspace,
        ["update", &id1, "--type", "bug"],
        "update_count1",
    );
    let _ = run_br(
        &workspace,
        ["update", &id2, "--type", "bug"],
        "update_count2",
    );
    let _ = run_br(
        &workspace,
        ["update", &id3, "--type", "feature"],
        "update_count3",
    );
    let _ = run_br(&workspace, ["close", &id2], "close_count2");

    let output = run_br(&workspace, ["count"], "count_text");
    assert!(output.status.success(), "count failed: {}", output.stderr);
    assert_snapshot!("count_output", normalize_output(&output.stdout));
}

// NOTE: `snapshot_label_add_list_output` was removed. The `label`
// subcommand (add/remove/list/list-all/rename) was deleted from the CLI
// with no replacement surface, so there is nothing left to snapshot here.
