//! E2E regression tests for the markup-escaping corruption class.
//!
//! Text handed to the rich console is parsed as MARKUP: `[` followed by a
//! letter, `#`, `/` or `@` opens a style tag, and the tag is consumed whether
//! or not it names a real style. So any stored field printed as a plain
//! string through `ctx.print` — a title, a description, a comment body, an
//! author — can have part of its content deleted on the way to the screen,
//! with nothing left behind to say so. A comment authored by `probe` rendered
//! with a blank author; a body reading `use [bold] for headings` rendered as
//! `use  for headings`.
//!
//! The tree had been safe only BY ACCIDENT: it brackets timestamps and glyphs
//! that start with a digit or a symbol (`[2026-01-02]`, `[● P2]`), which the
//! tag pattern declines to match. `[bug]`, the type badge, is NOT safe, and
//! neither is any bracketed word a human types.
//!
//! Every test here asserts rendered output against JSON ground truth, which
//! is the only assertion shape that catches BOTH failure directions:
//!
//! * missing escape at a markup sink  → text disappears;
//! * spurious escape at a sink that parses nothing (`print!`, `Text` spans)
//!   → a literal backslash appears.
//!
//! One assertion pair catches either, which matters because this codebase has
//! now shipped both.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

/// Deliberately contains all three hazards: a real style name (`[bold]`),
/// a bare bracketed word that is not a style at all (`[probe]` — how the bug
/// was originally found), and a closing tag (`[/]`) which makes the parser
/// error rather than consume. Kept short so no listing truncates it.
const HAZARD: &str = "esc [bold] [probe] [/] x";

/// A description carries the same hazards through a different column.
const HAZARD_DESC: &str = "see [red]and [/bold] notes";

fn json_of(stdout: &str) -> Value {
    serde_json::from_str(&extract_json_payload(stdout)).expect("valid json")
}

/// A workspace holding one issue whose title and description are hazardous.
fn workspace_with_hazard() -> (BrWorkspace, String) {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", HAZARD, "-d", HAZARD_DESC, "-t", "bug", "--json"],
        "create_hazard",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created = json_of(&create.stdout);
    let id = created["id"].as_str().expect("created id").to_string();

    // Ground truth: JSON is not rendered, so it shows what was STORED. If
    // these fail, the bug is in storage and every rendering assertion below
    // is meaningless.
    assert_eq!(created["title"], HAZARD, "title must be stored verbatim");
    assert_eq!(
        created["description"], HAZARD_DESC,
        "description must be stored verbatim"
    );
    (workspace, id)
}

/// Assert a rendering neither ate nor escaped a stored value.
///
/// `stored` must appear verbatim, and the line it appears on must carry no
/// backslash: an escape that reaches the screen is the mirror-image bug and
/// is just as wrong as a missing one.
fn assert_rendered_verbatim(rendered: &str, stored: &str, what: &str) {
    assert!(
        rendered.contains(stored),
        "{what}: stored text was altered on the way to the screen.\n  \
         wanted verbatim: {stored:?}\n  in output:\n{rendered}"
    );
    for line in rendered.lines().filter(|line| line.contains(stored)) {
        assert!(
            !line.contains('\\'),
            "{what}: a markup escape reached the screen \u{2014} this sink does \
             not parse markup, so nothing removes the backslash.\n  line: {line:?}"
        );
    }
}

/// `bd search` prints its results through the console as strings, unlike
/// `bd list`, which uses `println!`. Both build the same line, so only one of
/// them was corrupting it: the bracketed words in the title AND the command's
/// own `[bug]` type badge were consumed as style tags.
#[test]
fn search_text_prints_title_and_type_badge_verbatim() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_hazard();

    let search = run_br(&workspace, ["search", "esc"], "search_hazard");
    assert!(search.status.success(), "search failed: {}", search.stderr);
    assert!(
        search.stdout.contains(&id),
        "the hit must name the issue: {}",
        search.stdout
    );
    assert_rendered_verbatim(&search.stdout, HAZARD, "search title");
    // The type badge is this codebase's own text, and `[bug]` is tag-shaped:
    // it went missing from search results while `list` kept it.
    assert!(
        search.stdout.contains("[bug]"),
        "the type badge must survive its own formatting: {}",
        search.stdout
    );
}

/// `bd list` was already safe, because it emits with `println!`. Pin that:
/// the fix for `search` must not be "escape everywhere", which would print
/// backslashes here.
#[test]
fn list_text_prints_title_and_type_badge_verbatim() {
    common::init_test_logging();
    let (workspace, _id) = workspace_with_hazard();

    let list = run_br(&workspace, ["list"], "list_hazard");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    assert_rendered_verbatim(&list.stdout, HAZARD, "list title");
    assert!(
        list.stdout.contains("[bug]"),
        "the type badge must survive: {}",
        list.stdout
    );
}

/// `bd show`'s text view goes to a bare `print!`, which parses nothing. So
/// its job is to print stored text untouched — and NOT to escape it, which
/// is what it did for one release: `bd show` printed `use \[bold] for
/// headings` where the body said `use [bold] for headings`.
#[test]
fn show_text_prints_title_description_and_comment_without_escapes() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_hazard();

    let body = "use [bold] for headings and [red]for errors";
    let add = run_br(
        &workspace,
        ["comments", "add", &id, body],
        "show_comment_add",
    );
    assert!(add.status.success(), "comments add failed: {}", add.stderr);

    // Ground truth for the comment.
    let json = run_br(&workspace, ["comments", &id, "--json"], "show_comment_json");
    let comments = json_of(&json.stdout);
    assert_eq!(comments.as_array().expect("array")[0]["text"], body);

    let show = run_br(&workspace, ["show", &id], "show_hazard");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    assert_rendered_verbatim(&show.stdout, HAZARD, "show title");
    assert_rendered_verbatim(&show.stdout, HAZARD_DESC, "show description");
    assert_rendered_verbatim(&show.stdout, body, "show comment body");
}

/// The comment log is a verbatim, attributed record; both halves have to
/// survive. The author is asserted because the original bug was an author
/// that vanished — and the test that missed it was named
/// `..._shows_author_and_body` while never checking the author.
#[test]
fn comments_text_prints_author_and_markup_body_verbatim() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_hazard();

    let body = "prefer [bold] over [probe] here";
    let add = run_br(
        &workspace,
        ["comments", "add", &id, body, "--json"],
        "comments_add_hazard",
    );
    assert!(add.status.success(), "comments add failed: {}", add.stderr);

    // Ground truth: whatever was stored as author must be what is shown.
    let json = run_br(&workspace, ["comments", &id, "--json"], "comments_json");
    let comments = json_of(&json.stdout);
    let stored = &comments.as_array().expect("array")[0];
    let author = stored["author"].as_str().expect("author").to_string();
    assert_eq!(stored["text"], body);
    assert!(!author.is_empty(), "an unattributed comment is a bug in itself");

    let log = run_br(&workspace, ["comments", &id], "comments_text");
    assert!(log.status.success(), "comments failed: {}", log.stderr);
    assert_rendered_verbatim(&log.stdout, body, "comment body");
    assert!(
        log.stdout.contains(&author),
        "the author must be visible in the log (stored as {author:?}): {}",
        log.stdout
    );
}

/// A comment-sourced search hit prints an extra attribution line as a plain
/// string. Both the author and the quoted snippet are stored text.
#[test]
fn search_comment_match_line_prints_author_and_snippet_verbatim() {
    common::init_test_logging();
    let (workspace, id) = workspace_with_hazard();

    let body = "matchword lives next to [bold] and [probe]";
    let add = run_br(
        &workspace,
        ["comments", "add", &id, body],
        "search_comment_add",
    );
    assert!(add.status.success(), "comments add failed: {}", add.stderr);

    let json = run_br(
        &workspace,
        ["comments", &id, "--json"],
        "search_comment_json",
    );
    let comments = json_of(&json.stdout);
    let author = comments.as_array().expect("array")[0]["author"]
        .as_str()
        .expect("author")
        .to_string();

    let search = run_br(&workspace, ["search", "matchword"], "search_comment_hit");
    assert!(search.status.success(), "search failed: {}", search.stderr);
    let match_line = search
        .stdout
        .lines()
        .find(|line| line.trim_start().starts_with("comment "))
        .unwrap_or_else(|| panic!("no comment match line in:\n{}", search.stdout))
        .to_string();
    assert!(
        match_line.contains(&author),
        "the match line must name the author (stored as {author:?}): {match_line:?}"
    );
    assert!(
        match_line.contains("[bold]") && match_line.contains("[probe]"),
        "the quoted snippet must be verbatim: {match_line:?}"
    );
    assert!(
        !match_line.contains('\\'),
        "no escape may reach the screen: {match_line:?}"
    );
}

/// `bd create --dry-run` echoes what the caller typed back at them through
/// the console. It printed `Title: another  title` for
/// `--title 'another [bold] title'`.
#[test]
fn create_dry_run_echoes_title_verbatim() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "dry_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let dry = run_br(
        &workspace,
        ["create", HAZARD, "-d", HAZARD_DESC, "--dry-run"],
        "dry_run_hazard",
    );
    assert!(dry.status.success(), "dry run failed: {}", dry.stderr);
    assert_rendered_verbatim(&dry.stdout, HAZARD, "dry-run title");
}

/// The confirmation line for a real create is a `success` message. In `Plain`
/// mode it is a `println!` (nothing to escape); in `Rich` mode it is
/// composed into markup and escaped inside `OutputContext::success`. Only the
/// former is reachable from a test process without a terminal, so the Rich
/// composition is pinned by a unit test in `src/output/context.rs`; this
/// asserts the plain half is not "fixed" into printing backslashes.
#[test]
fn create_success_line_names_the_title_verbatim() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "success_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", HAZARD], "success_hazard");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    assert_rendered_verbatim(&create.stdout, HAZARD, "create success line");
}

/// `bd dep list` and `bd dep tree` print their plain views through the
/// console, mixing a stored title with bracket labels of their own
/// (`[P2] [open]` — and `[open]` is tag-shaped).
#[test]
fn dep_list_and_tree_print_title_and_bracket_labels_verbatim() {
    common::init_test_logging();
    let (workspace, blocked) = workspace_with_hazard();

    let create = run_br(
        &workspace,
        ["create", "blocker for the hazard", "--json"],
        "dep_blocker",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let blocker = json_of(&create.stdout)["id"]
        .as_str()
        .expect("blocker id")
        .to_string();

    let dep = run_br(
        &workspace,
        ["dep", "add", &blocked, &blocker],
        "dep_add_hazard",
    );
    assert!(dep.status.success(), "dep add failed: {}", dep.stderr);

    // `dep list` on the blocker shows the hazardous issue as a dependent.
    let list = run_br(
        &workspace,
        ["dep", "list", &blocker, "--direction", "up"],
        "dep_list",
    );
    assert!(list.status.success(), "dep list failed: {}", list.stderr);
    assert_rendered_verbatim(&list.stdout, HAZARD, "dep list title");
    assert!(
        list.stdout.contains("[P2]") && list.stdout.contains("[open]"),
        "the line's own bracket labels must survive: {}",
        list.stdout
    );

    let tree = run_br(&workspace, ["dep", "tree", &blocked], "dep_tree");
    assert!(tree.status.success(), "dep tree failed: {}", tree.stderr);
    assert_rendered_verbatim(&tree.stdout, HAZARD, "dep tree title");
    assert!(
        tree.stdout.contains("[P2]") && tree.stdout.contains("[open]"),
        "the tree's own bracket labels must survive: {}",
        tree.stdout
    );
}

/// Closing a blocker lists what it unblocked, titles and all, through the
/// console.
#[test]
fn close_unblocked_list_prints_titles_verbatim() {
    common::init_test_logging();
    let (workspace, blocked) = workspace_with_hazard();

    let create = run_br(
        &workspace,
        ["create", "blocker to be closed", "--json"],
        "close_blocker",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let blocker = json_of(&create.stdout)["id"]
        .as_str()
        .expect("blocker id")
        .to_string();

    let dep = run_br(
        &workspace,
        ["dep", "add", &blocked, &blocker],
        "close_dep_add",
    );
    assert!(dep.status.success(), "dep add failed: {}", dep.stderr);

    let close = run_br(
        &workspace,
        ["close", &blocker, "--suggest-next"],
        "close_blocker_run",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);
    // Asserted, not tolerated: a test that shrugs when its subject is absent
    // is the failure mode this whole file exists to correct.
    assert!(
        close.stdout.contains("nblocked"),
        "expected an unblocked section naming {blocked}: {}",
        close.stdout
    );
    assert_rendered_verbatim(&close.stdout, HAZARD, "close unblocked title");
}
