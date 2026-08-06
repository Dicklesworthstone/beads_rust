//! E2E tests for `bd msg` body input: the stdin-marker fix and the new
//! `-f/--file` flag.
//!
//! Two defects, one surface, because they share the same clap definitions:
//!
//! 1. `bd msg <target> -` used to be parsed as the *literal* two-byte
//!    string `"-"` rather than the conventional stdin marker, even though
//!    stdin was not a tty and had real content waiting. Three real sends
//!    were lost to this before anyone noticed (found by sorting an outbox
//!    export by body length and reading the short tail).
//! 2. `bd msg` had no way to hand it a body without going through the
//!    shell, unlike `bd comments add -f`. A body containing backticks or
//!    `$(...)` got partially executed by the shell before `bd` ever saw
//!    the string.
//!
//! Every source (`-f <file>`, `-f -`, `-`, and stdin via an omitted body)
//! is required to agree on trailing-newline handling -- see
//! `messaging::resolve_body`'s doc comment for why that is a deliberate
//! divergence from `bd comments add`'s raw `-f` read, not an oversight.

mod common;

use common::cli::{
    BrRun, BrWorkspace, extract_json_payload, run_br_with_env, run_br_with_env_and_stdin,
};
use serde_json::Value;
use std::process::{Command, Stdio};

/// Every send in this file needs a resolvable sender identity; `bd msg`
/// (unlike `bd comments add`) has no config-default fallback and errors
/// out without one.
const AGENT: &str = "msgtest-agent";

fn env() -> [(&'static str, &'static str); 1] {
    [("BD_AGENT_ID", AGENT)]
}

fn init(workspace: &BrWorkspace) {
    let init = run_br_with_env(workspace, ["init"], env(), "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
}

fn msg(workspace: &BrWorkspace, args: &[&str], label: &str) -> BrRun {
    run_br_with_env(workspace, args.to_vec(), env(), label)
}

fn msg_stdin(workspace: &BrWorkspace, args: &[&str], input: &str, label: &str) -> BrRun {
    run_br_with_env_and_stdin(workspace, args.to_vec(), env(), input, label)
}

fn outbox_json(workspace: &BrWorkspace, to: &str) -> BrRun {
    run_br_with_env(workspace, ["outbox", "--to", to, "--json"], env(), "outbox_check")
}

/// The single sent message to `to`, or `None` if nothing reached it.
/// Distinguishes "no message" from "a message with an empty body" by
/// relying on `bd outbox --json`'s own explicit-empty-array shape for the
/// no-messages case (verified below in
/// `outbox_json_reports_true_empty_distinctly_from_an_empty_body`).
fn sent_body(workspace: &BrWorkspace, to: &str) -> Option<String> {
    let out = outbox_json(workspace, to);
    assert!(out.status.success(), "outbox failed: {}", out.stderr);
    let payload = extract_json_payload(&out.stdout);
    if payload.trim() == "[]" {
        return None;
    }
    // Exactly one line per message; these tests only ever send at most
    // one, to a recipient unique to the test.
    let line = payload
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("expected a JSON object line, got: {payload:?}"));
    let v: Value = serde_json::from_str(line).expect("valid json line");
    Some(v["body"].as_str().expect("body is a string").to_string())
}

fn assert_nothing_sent(workspace: &BrWorkspace, to: &str) {
    assert_eq!(
        sent_body(workspace, to),
        None,
        "recipient {to:?} must not have received anything"
    );
}

/// Confirms the negative-result detector used by `assert_nothing_sent`
/// actually distinguishes "no messages" from "a message with an empty
/// body" -- otherwise a `None` here could just as easily mean the outbox
/// JSON shape changed under us and every `assert_nothing_sent` call would
/// pass regardless of whether a send happened.
#[test]
fn outbox_json_reports_true_empty_distinctly_from_an_empty_body() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    assert_eq!(sent_body(&workspace, "nobody-ever-sent-here"), None);

    // A real send with a non-empty body must read back as Some(..), not
    // be confused with the no-message case.
    let send = msg(&workspace, &["msg", "real-recipient", "hello", "--force"], "seed_send");
    assert!(send.status.success(), "send failed: {}", send.stderr);
    assert_eq!(
        sent_body(&workspace, "real-recipient"),
        Some("hello".to_string())
    );
}

/// Literal `< file` shell redirection: attaches stdin directly to a real
/// file descriptor, which is the exact mechanism (not a pipe standing in
/// for it) that item 4 of the task requires to keep working unchanged.
fn run_msg_with_file_redirected_stdin(
    workspace: &BrWorkspace,
    args: &[&str],
    stdin_path: &std::path::Path,
) -> (bool, String, String) {
    let bin = assert_cmd::cargo::cargo_bin!("br");
    let file = std::fs::File::open(stdin_path).expect("open stdin file");
    let mut cmd = Command::new(bin);
    cmd.current_dir(&workspace.root)
        .args(args)
        .env("NO_COLOR", "1")
        .env("HOME", &workspace.root)
        .env("BD_AGENT_ID", AGENT)
        .stdin(Stdio::from(file))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().expect("run br with redirected stdin");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

// =============================================================================
// Defect 1: `-` must mean "read stdin", never a literal dash
// =============================================================================

/// The exact reproduction from the defect report: piping a body and
/// passing a bare `-` must store the piped text, not the two-byte string
/// `"-"`. This is the regression this whole task exists to close -- three
/// real sends were silently reduced to a dash before it was caught.
#[test]
fn dash_reads_piped_stdin_not_a_literal_dash() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let send = msg_stdin(
        &workspace,
        &["msg", "beads1", "-", "--force"],
        "THIS BODY CAME FROM STDIN",
        "dash_send",
    );
    assert!(send.status.success(), "send failed: {}", send.stderr);

    let body = sent_body(&workspace, "beads1").expect("a message was sent");
    assert_eq!(
        body, "THIS BODY CAME FROM STDIN",
        "the piped body must survive; got {body:?} instead"
    );
    assert_ne!(body, "-", "defect 1 reproduced: body was destroyed to a literal dash");
}

/// A multi-word positional body that merely *starts* with `-` is text,
/// exactly like `bd comments add`'s treatment of a leading-dash body --
/// only a *sole* `-` is special.
#[test]
fn dash_prefixed_multiword_body_is_literal_text_not_a_stdin_request() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let send = msg(
        &workspace,
        &["msg", "target", "-", "decided", "to", "ship", "--force"],
        "dash_prefixed_send",
    );
    assert!(send.status.success(), "send failed: {}", send.stderr);
    assert_eq!(
        sent_body(&workspace, "target"),
        Some("- decided to ship".to_string())
    );
}

// =============================================================================
// Byte-exactness across all four body sources
// =============================================================================

/// One fixture body -- backticks, `$(...)`, quotes, a trailing newline,
/// and a non-ASCII character -- sent through all four ways of getting a
/// body into `bd msg` without shell interpolation touching it: `-f
/// <file>`, `-f -`, a bare `-`, and the pre-existing implicit-stdin path
/// (`< file`, via a real file descriptor, not a pipe standing in for
/// one). All four must store byte-identical text.
#[test]
fn body_is_byte_exact_across_file_stdin_flag_and_dash() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let fixture = "line one `cmd` $(sub) \"quoted\" caf\u{e9}\n";
    let path = workspace.root.join("fixture_body.txt");
    std::fs::write(&path, fixture).expect("write fixture");
    let path_str = path.to_str().unwrap();

    // -f <file>
    let via_file = msg(&workspace, &["msg", "via-file", "-f", path_str, "--force"], "via_file");
    assert!(via_file.status.success(), "{}", via_file.stderr);

    // -f -
    let via_file_stdin = msg_stdin(
        &workspace,
        &["msg", "via-file-stdin", "-f", "-", "--force"],
        fixture,
        "via_file_stdin",
    );
    assert!(via_file_stdin.status.success(), "{}", via_file_stdin.stderr);

    // bare -
    let via_dash = msg_stdin(&workspace, &["msg", "via-dash", "-", "--force"], fixture, "via_dash");
    assert!(via_dash.status.success(), "{}", via_dash.stderr);

    // < file (real file descriptor, the historic implicit path)
    let (ok, stdout, stderr) =
        run_msg_with_file_redirected_stdin(&workspace, &["msg", "via-redirect", "--force"], &path);
    assert!(ok, "redirected-stdin send failed: stdout={stdout} stderr={stderr}");

    let expected = fixture.trim_end_matches('\n');
    let got_file = sent_body(&workspace, "via-file").expect("via-file sent");
    let got_file_stdin = sent_body(&workspace, "via-file-stdin").expect("via-file-stdin sent");
    let got_dash = sent_body(&workspace, "via-dash").expect("via-dash sent");
    let got_redirect = sent_body(&workspace, "via-redirect").expect("via-redirect sent");

    assert_eq!(
        got_file, expected,
        "-f <file> must strip the trailing newline like every other source"
    );
    assert_eq!(got_file_stdin, expected, "-f - must match -f <file> byte-for-byte");
    assert_eq!(got_dash, expected, "- must match -f <file> byte-for-byte");
    assert_eq!(
        got_redirect, expected,
        "< file (the pre-existing path) must be unchanged and must match the other three"
    );

    // And the metacharacters themselves must have survived untouched --
    // this is the actual defect-2 payoff, not just newline bookkeeping.
    for body in [&got_file, &got_file_stdin, &got_dash, &got_redirect] {
        assert!(body.contains('`'), "backtick lost: {body:?}");
        assert!(body.contains("$(sub)"), "command substitution text lost: {body:?}");
        assert!(body.contains('"'), "quote lost: {body:?}");
        assert!(body.contains('\u{e9}'), "non-ASCII character lost: {body:?}");
    }
}

/// The one deliberate divergence from `bd comments add`: `-f <file>`
/// strips a trailing newline so it agrees with `bd msg`'s own long-
/// standing bare-stdin path, whereas `bd comments add -f` reads raw.
/// Documented here as a directly observable behavior, not just in prose.
#[test]
fn file_flag_strips_trailing_newline_documented_divergence_from_comments_add() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let path = workspace.root.join("body.txt");
    std::fs::write(&path, "payload\n\n").expect("write"); // two trailing newlines

    let send = msg(
        &workspace,
        &["msg", "trimmed", "-f", path.to_str().unwrap(), "--force"],
        "trim_send",
    );
    assert!(send.status.success(), "{}", send.stderr);
    // trim_end_matches strips ALL trailing newlines, matching the
    // pre-existing bare-stdin behavior exactly.
    assert_eq!(sent_body(&workspace, "trimmed"), Some("payload".to_string()));
}

// =============================================================================
// Empty body: refused from every source, by name, sending nothing
// =============================================================================

#[test]
fn empty_literal_body_is_refused() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let send = msg(&workspace, &["msg", "e-literal", "", "--force"], "empty_literal");
    assert!(!send.status.success(), "an empty literal body must be refused");
    let combined = format!("{}{}", send.stdout, send.stderr);
    assert!(
        combined.to_lowercase().contains("empty"),
        "error must say the body is empty: {combined:?}"
    );
    assert_nothing_sent(&workspace, "e-literal");
}

#[test]
fn empty_file_body_is_refused_and_names_the_source() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let path = workspace.root.join("empty.txt");
    std::fs::write(&path, "").expect("write empty file");

    let send = msg(
        &workspace,
        &["msg", "e-file", "-f", path.to_str().unwrap(), "--force"],
        "empty_file",
    );
    assert!(!send.status.success(), "an empty file body must be refused");
    let combined = format!("{}{}", send.stdout, send.stderr);
    assert!(combined.to_lowercase().contains("empty"), "got: {combined:?}");
    assert!(
        combined.contains("--file"),
        "error must name the --file source, got: {combined:?}"
    );
    assert_nothing_sent(&workspace, "e-file");
}

#[test]
fn empty_stdin_via_bare_dash_is_refused() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let send = msg_stdin(&workspace, &["msg", "e-dash", "-", "--force"], "", "empty_dash");
    assert!(!send.status.success(), "empty stdin via - must be refused");
    let combined = format!("{}{}", send.stdout, send.stderr);
    assert!(combined.to_lowercase().contains("empty"), "got: {combined:?}");
    assert!(
        combined.to_lowercase().contains("stdin"),
        "error must name stdin as the source, got: {combined:?}"
    );
    assert_nothing_sent(&workspace, "e-dash");
}

#[test]
fn empty_stdin_via_file_flag_dash_is_refused() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let send = msg_stdin(
        &workspace,
        &["msg", "e-file-dash", "-f", "-", "--force"],
        "",
        "empty_file_dash",
    );
    assert!(!send.status.success(), "empty stdin via -f - must be refused");
    assert_nothing_sent(&workspace, "e-file-dash");
}

/// The composition the leader specifically flagged: a body of *only*
/// newlines trims to the empty string, and the empty-body refusal must
/// still fire -- two independently correct rules (trim, then refuse-
/// empty) must actually compose, not just each pass in isolation.
/// Exercised from all three stream sources separately.
#[test]
fn newline_only_body_trims_to_empty_and_is_refused_via_dash() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let send = msg_stdin(&workspace, &["msg", "e-nl-dash", "-", "--force"], "\n\n\n", "nl_only_dash");
    assert!(!send.status.success(), "a newline-only body must trim to empty and be refused");
    assert_nothing_sent(&workspace, "e-nl-dash");
}

#[test]
fn newline_only_body_trims_to_empty_and_is_refused_via_file_flag_dash() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let send = msg_stdin(
        &workspace,
        &["msg", "e-nl-file-dash", "-f", "-", "--force"],
        "\n\n\n",
        "nl_only_file_dash",
    );
    assert!(
        !send.status.success(),
        "a newline-only body via -f - must trim to empty and be refused"
    );
    assert_nothing_sent(&workspace, "e-nl-file-dash");
}

#[test]
fn newline_only_body_trims_to_empty_and_is_refused_via_file() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let path = workspace.root.join("nl_only.txt");
    std::fs::write(&path, "\n\n\n").expect("write");

    let send = msg(
        &workspace,
        &["msg", "e-nl-file", "-f", path.to_str().unwrap(), "--force"],
        "nl_only_file",
    );
    assert!(
        !send.status.success(),
        "a newline-only file body must trim to empty and be refused"
    );
    assert_nothing_sent(&workspace, "e-nl-file");
}

// =============================================================================
// -f error shapes and the conflict decisions (item 7)
// =============================================================================

#[test]
fn file_flag_naming_a_nonexistent_path_refuses_and_names_it() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let send = msg(
        &workspace,
        &["msg", "e-missing", "-f", "/no/such/path/for/this/test.txt", "--force"],
        "missing_file",
    );
    assert!(!send.status.success(), "a nonexistent --file path must be refused");
    let combined = format!("{}{}", send.stdout, send.stderr);
    assert!(
        combined.contains("/no/such/path/for/this/test.txt"),
        "error must name the missing path, got: {combined:?}"
    );
    assert_nothing_sent(&workspace, "e-missing");
}

/// Passing both a positional body and `--file` is a usage error -- never
/// a silent pick of one over the other.
#[test]
fn positional_body_and_file_flag_together_is_refused() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let path = workspace.root.join("body.txt");
    std::fs::write(&path, "from the file").expect("write");

    let send = msg(
        &workspace,
        &["msg", "e-both", "inline text", "-f", path.to_str().unwrap(), "--force"],
        "both_body_and_file",
    );
    assert!(!send.status.success(), "body + --file together must be refused");
    let combined = format!("{}{}", send.stdout, send.stderr);
    assert!(
        combined.to_lowercase().contains("not both"),
        "expected a not-both usage error, got: {combined:?}"
    );
    assert_nothing_sent(&workspace, "e-both");
}

/// The conflict decided for item 7: `-f <path>` naming a REAL file wins
/// over any content sitting unread on stdin -- the explicit flag is
/// used, the pipe is never consulted. Positive control: stdin carries
/// content that differs from the file, so a pass here can only mean the
/// file won, not that the two happened to agree.
#[test]
fn file_flag_wins_silently_over_ignored_stdin_content() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    init(&workspace);

    let path = workspace.root.join("real_body.txt");
    std::fs::write(&path, "the real file content").expect("write");

    let send = msg_stdin(
        &workspace,
        &["msg", "e-flag-wins", "-f", path.to_str().unwrap(), "--force"],
        "this stdin content must be ignored",
        "flag_wins",
    );
    assert!(send.status.success(), "{}", send.stderr);
    assert_eq!(
        sent_body(&workspace, "e-flag-wins"),
        Some("the real file content".to_string()),
        "the explicit --file must win over unread stdin content"
    );
}

// =============================================================================
// `-` on a TTY: error, not a hang and not a literal dash (item 8)
// =============================================================================

/// `script -qec` allocates a genuine pseudo-terminal for the child's
/// stdin (verified independently: `python3 -c "import
/// sys;print(sys.stdin.isatty())"` under the same wrapper prints `True`
/// even though this test harness itself is headless), so this observes
/// the real TTY branch rather than a piped stand-in for one. `-e`
/// (`--return`) makes `script` propagate the child's exit status instead
/// of always reporting 0, which it does unconditionally otherwise --
/// confirmed separately: `script -qec false /dev/null; echo $?` prints
/// `0` without `-e` and `1` with it.
#[test]
fn dash_on_a_real_tty_errors_instead_of_hanging_or_sending_a_dash() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();

    if Command::new("script").arg("--version").output().is_err() {
        eprintln!("SKIPPED: `script` (util-linux) is not on PATH; cannot allocate a pty for this test");
        return;
    }

    init(&workspace);

    let bin = assert_cmd::cargo::cargo_bin!("br");
    let inner = format!("{} msg tty-target - --force", bin.to_string_lossy());

    let mut cmd = Command::new("script");
    cmd.current_dir(&workspace.root)
        .args(["-qec", &inner, "/dev/null"])
        .env("NO_COLOR", "1")
        .env("HOME", &workspace.root)
        .env("BD_AGENT_ID", AGENT)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Bounded wait: a hang here (the exact defect this guards against)
    // must fail the test loudly rather than stall the suite forever.
    let mut child = cmd.spawn().expect("spawn script");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!(
                "`bd msg tty-target -` under a real pty did not exit within 10s -- \
                 it hung instead of refusing"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read as _;
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read as _;
        let _ = err.read_to_string(&mut stderr);
    }

    assert!(
        !status.success(),
        "a bare `-` on a real terminal must error, not succeed. stdout={stdout:?} stderr={stderr:?}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.to_lowercase().contains("terminal"),
        "expected an error naming the terminal as the problem, got: {combined:?}"
    );
    assert_nothing_sent(&workspace, "tty-target");
}
