//! E2E tests for the nonzero-exit failure banner.
//!
//! # What is being defended
//!
//! `br`'s stream routing was already correct: a failing command writes zero
//! bytes to stdout, the whole error envelope to stderr, and exits nonzero.
//! Piping alone hid nothing. The failure needed **both** `2>&1` (merging the
//! error into the pipe) **and** a truncating filter:
//!
//! ```text
//! $ br create "" --prefix ct --json 2>&1 | tail -3
//!     }
//!   }
//! }
//! ```
//!
//! Every discriminating token — the `"error"` key, the code, the message — is
//! at the *top* of the envelope, and `tail` shows the *bottom*. What survives
//! is closing braces, which is also how a *success* envelope ends. Three agents
//! read those braces as success; one filed a P1 bug that did not exist.
//!
//! So these tests are about **position**, not about presence. A banner emitted
//! first is the first thing cut off, and a test that only greps stderr for the
//! banner would pass just as happily with the useless version. Every assertion
//! below that matters therefore goes through a real shell pipeline ending in
//! `tail -1`.
//!
//! # What is explicitly NOT defended: the exit code
//!
//! `$?` after a pipeline is the *last* command's status by shell semantics, so
//! `br ... | tail` reports `tail`'s 0 no matter what `br` does. Nothing here
//! can change that; only `set -o pipefail`, `${PIPESTATUS[0]}`, or not piping
//! recovers it. These tests harden the **text** channel, and they measure `br`'s
//! own status with *redirection* rather than a pipe for exactly that reason.

mod common;

use common::cli::{BrWorkspace, run_br};
use std::path::Path;
use std::process::{Command, Output};

/// Run a shell script with `$BR` bound to the binary under test.
///
/// A real `sh` is unavoidable here: the whole point is the ordering of two file
/// descriptors merged by `2>&1` into one pipe, which cannot be reproduced by
/// capturing stdout and stderr separately the way the normal harness does.
fn sh(workspace: &BrWorkspace, script: &str) -> Output {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);
    cmd.current_dir(&workspace.root);
    cmd.env("BR", assert_cmd::cargo::cargo_bin!("br"));
    cmd.env("HOME", &workspace.root);
    cmd.env("NO_COLOR", "1");
    // Keep the ambient agent identity and routing out of the fixture, as
    // `run_br` does.
    for key in [
        "BD_AGENT_ID",
        "BD_ISSUE_PREFIX",
        "BEADS_DIR",
        "BEADS_JSONL",
        "BR_OUTPUT_FORMAT",
        "TOON_DEFAULT_FORMAT",
        "RUST_LOG",
    ] {
        cmd.env_remove(key);
    }
    cmd.output().expect("run sh")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn last_line(out: &Output) -> String {
    stdout_of(out).trim_end_matches('\n').to_string()
}

/// Create an issue and return its id.
fn create_issue(workspace: &BrWorkspace, title: &str) -> String {
    let out = run_br(workspace, ["create", title, "--json"], "create");
    assert!(out.status.success(), "create failed: {}", out.stderr);
    let value: serde_json::Value =
        serde_json::from_str(&common::cli::extract_json_payload(&out.stdout)).expect("create json");
    value["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no id in create output: {value}"))
        .to_string()
}

fn init(workspace: &BrWorkspace) {
    let init = run_br(workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
}

/// THE TEST THAT DEFINES DONE.
///
/// A failing command through `2>&1 | tail -1`: the single surviving line must
/// name the failure. Before the banner existed this line was `}`.
#[test]
fn the_only_surviving_line_names_the_failure() {
    let workspace = BrWorkspace::new();
    init(&workspace);

    let out = sh(
        &workspace,
        r#""$BR" create "" --prefix ct --json 2>&1 | tail -1"#,
    );

    assert_eq!(
        last_line(&out),
        "br: FAILED (VALIDATION_FAILED, exit 4)",
        "the last surviving line of a truncated failure must name the error \
         code and the exit status; a closing brace is indistinguishable from \
         the tail of a success envelope"
    );
}

/// `tail -3` too: the banner is the last of the three, not merely present.
///
/// This is the check that distinguishes a real fix from a banner emitted first
/// (which `tail` deletes) — the closing braces are still there, but no longer
/// last.
#[test]
fn the_banner_is_last_not_merely_present() {
    let workspace = BrWorkspace::new();
    init(&workspace);

    let out = sh(
        &workspace,
        r#""$BR" create "" --prefix ct --json 2>&1 | tail -3"#,
    );
    let text = stdout_of(&out);
    let lines: Vec<&str> = text.trim_end_matches('\n').lines().collect();

    assert_eq!(lines.len(), 3, "expected 3 lines, got {text:?}");
    assert_eq!(
        lines[2], "br: FAILED (VALIDATION_FAILED, exit 4)",
        "the banner must be the FINAL line under 2>&1, not somewhere in the \
         middle: {text:?}"
    );
    assert!(
        !lines[0].contains("FAILED"),
        "banner appears above the tail of the envelope, i.e. it was emitted \
         early and a deeper truncation would delete it: {text:?}"
    );
}

/// The sharpest ordering case: a command that writes to **stdout** and *then*
/// exits nonzero.
///
/// `br doctor --json` prints its whole report to stdout before exiting 1, so
/// under `2>&1` the banner has to come after a stream it does not control.
/// This is also a different exit path from the one above — `doctor` calls
/// `exit` itself and never reaches `handle_error`.
#[test]
fn banner_is_last_even_when_stdout_carried_the_payload() {
    let workspace = BrWorkspace::new();
    init(&workspace);
    // Make a check fail: delete the database that `init` created.
    remove_db(&workspace);

    let out = sh(&workspace, r#""$BR" doctor --json 2>&1 | tail -1"#);

    assert_eq!(
        last_line(&out),
        "br: FAILED (DOCTOR_CHECKS_FAILED, exit 1)",
        "doctor's JSON report is written to stdout before it exits; the banner \
         must still land after it under 2>&1"
    );
}

/// Requirement 4: the banner must not corrupt a `--json` consumer.
///
/// `br doctor --json` is the case that can actually be broken — it emits JSON
/// on stdout *and* exits nonzero, so it is simultaneously a banner-emitting run
/// and a JSON-producing one. Piping stdout to `jq` must still parse; the banner
/// is on stderr and is asserted to have been emitted in the same run, so this
/// cannot pass vacuously by the banner being absent.
#[test]
fn jq_still_parses_stdout_while_the_banner_is_emitted() {
    let workspace = BrWorkspace::new();
    init(&workspace);
    remove_db(&workspace);

    // `jq -e .ok` exits nonzero if the value is false, so ask for a shape
    // assertion instead: `.checks | length > 0` proves the document parsed and
    // is the report we expected.
    let out = sh(
        &workspace,
        r#""$BR" doctor --json 2>banner.txt | jq -e '.checks | length > 0' >jq.out; echo "jq_status=$?"; cat jq.out"#,
    );
    let text = stdout_of(&out);

    assert!(
        text.contains("jq_status=0"),
        "jq failed to parse br's stdout while the banner was being emitted: \
         {text:?}"
    );
    assert!(
        text.contains("true"),
        "jq parsed something, but not the doctor report: {text:?}"
    );

    let banner = std::fs::read_to_string(workspace.root.join("banner.txt")).expect("banner.txt");
    assert!(
        banner.contains("br: FAILED (DOCTOR_CHECKS_FAILED, exit 1)"),
        "the banner was not emitted in this run, so the jq assertion above \
         proved nothing: {banner:?}"
    );
}

/// The recipes in `docs/agent/ERRORS.md` are executed here, not just written.
///
/// A documentation fix for a parsing hazard is worth nothing if the replacement
/// recipe is also broken, and one of the obvious replacements *is*:
/// `sed -n '/{/,$p'` is unbounded at the end, so it survives leading warnings
/// and then hands `jq` the trailing banner line. It works on every release
/// before the banner, which is exactly the kind of recipe that gets copied.
///
/// So each documented form runs against a real failing invocation with the full
/// noise profile — two contention warnings, the envelope, the banner — and the
/// right ones must extract the code while the wrong ones must fail. If a future
/// `jq` starts tolerating trailing data, or the envelope stops being
/// pretty-printed, this test says so instead of the docs quietly rotting.
#[test]
fn the_documented_envelope_recipes_work_and_the_documented_traps_fail() {
    let workspace = BrWorkspace::new();
    init(&workspace);

    // The worst realistic shape: repeated ids produce repeated warnings, and a
    // blocked id makes the whole thing exit nonzero so the banner is present.
    let blocker = create_issue(&workspace, "blocker");
    let blocked = create_issue(&workspace, "blocked");
    let closed = create_issue(&workspace, "already closed");
    let script = format!(
        r#"set -e
"$BR" dep add {blocked} {blocker} >/dev/null 2>&1
"$BR" close {closed} >/dev/null 2>&1
set +e
"$BR" close {closed} {closed} {blocked} --json >/dev/null 2>err.json
echo "br_status=$?"
echo "warnings=$(grep -c '^warning:' err.json)"
echo "last=$(tail -1 err.json)"

# documented as CORRECT.
#
# The assignment form is load-bearing: `x=$(cmd)` propagates cmd's status,
# while `echo "x=$(cmd)"` reports *echo's* 0 and throws the real status away.
# Written the echo way, right_*_status could never be nonzero and the check
# was dead -- which mattered, because jq prints the extracted code and *then*
# dies on the trailing byte, so the value alone does not discriminate.
right_sed=$(sed -n '/{{/,/^}}$/p' err.json | jq -r .error.code); echo "right_sed_status=$?"
echo "right_sed=$right_sed"
right_python=$(python3 -c 'import json,sys
s=sys.stdin.read()
print(json.JSONDecoder().raw_decode(s[s.index("{{"):])[0]["error"]["code"])' < err.json); echo "right_python_status=$?"
echo "right_python=$right_python"

# documented as WRONG
jq . err.json >/dev/null 2>&1;                       echo "wrong_whole=$?"
sed -n '/{{/,$p' err.json | jq . >/dev/null 2>&1;      echo "wrong_unbounded=$?"
tail -n +2 err.json | jq . >/dev/null 2>&1;          echo "wrong_skip_one=$?"
grep -v '^warning:' err.json | jq . >/dev/null 2>&1; echo "wrong_case_strip=$?"
"#
    );
    let out = sh(&workspace, &script);
    let text = stdout_of(&out);
    let field = |key: &str| -> String {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("no {key} in recipe output: {text}"))
            .to_string()
    };

    // The fixture really is the hostile shape, or the rest proves nothing.
    assert_eq!(field("br_status"), "3", "fixture exit code: {text}");
    assert_eq!(
        field("warnings"),
        "2",
        "fixture must carry TWO leading warnings, which is what defeats a \
         fixed-line skip: {text}"
    );
    assert!(
        field("last").contains("br: FAILED (NOTHING_TO_DO, exit 3)"),
        "fixture must carry the trailing banner, which is what defeats an \
         end-unbounded scan: {text}"
    );

    // The documented recipes extract the code.
    assert_eq!(
        field("right_sed"),
        "NOTHING_TO_DO",
        "sed window form: {text}"
    );
    assert_eq!(field("right_sed_status"), "0", "sed window status: {text}");
    assert_eq!(
        field("right_python"),
        "NOTHING_TO_DO",
        "raw_decode form: {text}"
    );
    assert_eq!(
        field("right_python_status"),
        "0",
        "raw_decode status: {text}"
    );

    // And the documented traps really are traps. Asserted nonzero rather than
    // 5 so this does not become a test of jq's exit-code scheme.
    for trap in [
        "wrong_whole",
        "wrong_unbounded",
        "wrong_skip_one",
        "wrong_case_strip",
    ] {
        assert_ne!(
            field(trap),
            "0",
            "{trap} is documented as WRONG but succeeded; if a tool got more \
             tolerant, the docs should say so rather than warn about a hazard \
             that no longer exists: {text}"
        );
    }
}

/// Requirement 5, non-`handle_error` paths: a clap usage error.
///
/// `br list --nope` exits 2 from inside clap. `Cli::parse()` would call
/// `std::process::exit` there and bypass the funnel entirely, which is why
/// `main` uses `try_parse`.
#[test]
fn usage_errors_get_the_banner_too() {
    let workspace = BrWorkspace::new();

    let out = sh(&workspace, r#""$BR" list --nope 2>&1 | tail -1"#);

    assert_eq!(
        last_line(&out),
        "br: FAILED (USAGE_ERROR, exit 2)",
        "a clap usage failure is the most common failure of all and must not \
         be the one path with no banner"
    );
}

/// Requirement 5, another self-exiting path: `br where` with no `.beads`.
#[test]
fn where_without_a_beads_dir_gets_the_banner() {
    let workspace = BrWorkspace::new();
    // Deliberately NOT initialized.

    let out = sh(&workspace, r#""$BR" where 2>&1 | tail -1"#);

    assert_eq!(last_line(&out), "br: FAILED (NO_BEADS_DIR, exit 1)");
}

/// Requirement 5, `br config get` on a key that is not set.
///
/// A quiet exit-1 path with no envelope at all: it prints a one-line message
/// and exits. Nothing about it resembles the JSON failures above, which is
/// exactly why it needs its own assertion rather than an assumption that
/// "`handle_error` covers everything".
#[test]
fn config_get_on_a_missing_key_gets_the_banner() {
    let workspace = BrWorkspace::new();
    init(&workspace);

    let out = sh(&workspace, r#""$BR" config get nosuchkey 2>&1 | tail -1"#);

    assert_eq!(last_line(&out), "br: FAILED (CONFIG_KEY_NOT_FOUND, exit 1)");
}

/// Requirement 5, `br lint`'s computed exit code.
#[test]
fn lint_warnings_get_the_banner() {
    let workspace = BrWorkspace::new();
    init(&workspace);
    let create = run_br(
        &workspace,
        ["create", "bug with no template sections", "--type", "bug"],
        "create_bug",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let out = sh(&workspace, r#""$BR" lint 2>&1 | tail -1"#);

    assert_eq!(last_line(&out), "br: FAILED (LINT_WARNINGS, exit 1)");
}

/// A different error code and status, so the banner is proven to *report* the
/// failure rather than print a constant.
#[test]
fn banner_reports_the_actual_code_and_status() {
    let workspace = BrWorkspace::new();
    init(&workspace);

    let out = sh(&workspace, r#""$BR" show ct-nosuch --json 2>&1 | tail -1"#);
    assert_eq!(last_line(&out), "br: FAILED (ISSUE_NOT_FOUND, exit 3)");

    // And the status itself is unchanged — measured with redirection, never a
    // pipe, because a pipeline would report `tail`'s status instead.
    let status = sh(
        &workspace,
        r#""$BR" show ct-nosuch --json >/dev/null 2>/dev/null; echo "status=$?""#,
    );
    assert!(
        stdout_of(&status).contains("status=3"),
        "the banner must not disturb the exit status: {:?}",
        stdout_of(&status)
    );
}

/// Requirement 1: unconditional. No isatty gating, no "works by hand, silent in
/// the script".
///
/// Neither this test nor any other here runs on a tty, so a pipe-gated
/// implementation would pass them all — this asserts the *other* direction:
/// stderr redirected straight to a file (no pipe anywhere, nothing for a
/// pipe-detector to detect) still carries the banner.
#[test]
fn banner_is_not_gated_on_being_piped() {
    let workspace = BrWorkspace::new();
    init(&workspace);

    let out = sh(
        &workspace,
        r#""$BR" create "" --prefix ct 2>err.txt >out.txt; tail -1 err.txt"#,
    );

    assert_eq!(
        last_line(&out),
        "br: FAILED (VALIDATION_FAILED, exit 4)",
        "the banner must not depend on how the caller happens to be plumbed"
    );
    let stdout = std::fs::read_to_string(workspace.root.join("out.txt")).expect("out.txt");
    assert!(
        stdout.is_empty(),
        "a failure must still write nothing to stdout: {stdout:?}"
    );

    // Exactly ONE banner. Nine call sites funnel into one function, and a
    // second one creeping in (an exit path that emits and then exits through
    // the funnel again) would be invisible to `tail -1` — it looks identical.
    let stderr = std::fs::read_to_string(workspace.root.join("err.txt")).expect("err.txt");
    let banners = stderr
        .lines()
        .filter(|line| line.starts_with("br: ") && line.contains("exit 4"))
        .count();
    assert_eq!(
        banners, 1,
        "expected exactly one banner line, found {banners} in {stderr:?}"
    );
}

/// The success path gains nothing.
///
/// NOTE ON FAIL-BEFORE-PASS: this test passes on the pre-change binary too —
/// it is a no-regression assertion, and it cannot be made to fail by removing
/// the feature. It earns its place by *mutation* instead: make the banner
/// unconditional (drop the `status != 0` check in `exit_with_status`) or send it
/// to stdout, and it fails.
#[test]
fn success_gains_no_banner_and_stdout_is_untouched() {
    let workspace = BrWorkspace::new();
    init(&workspace);
    let create = run_br(&workspace, ["create", "a perfectly fine issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // stdout captured on its own, and again with stderr merged in. If anything
    // new were being written to stdout on success, or if a banner were being
    // emitted at all, these two would differ.
    let out = sh(
        &workspace,
        r#""$BR" list --json >clean.txt 2>err.txt; echo "status=$?"; "$BR" list --json 2>&1 >merged.txt; cmp -s clean.txt merged.txt && echo identical || echo differs; cat err.txt"#,
    );
    let text = stdout_of(&out);

    assert!(text.contains("status=0"), "list should succeed: {text:?}");
    assert!(
        text.contains("identical"),
        "success stdout changed between plain and 2>&1 runs: {text:?}"
    );
    assert!(
        !text.contains("FAILED"),
        "a successful command must not emit a failure banner: {text:?}"
    );
    let merged = std::fs::read_to_string(workspace.root.join("merged.txt")).expect("merged.txt");
    assert!(
        serde_json::from_str::<serde_json::Value>(merged.trim()).is_ok(),
        "success stdout must remain parseable JSON: {merged:?}"
    );
}

/// `--help` and `--version` exit zero through the same clap `Err` path the
/// usage error takes. They must stay silent, and on stdout.
#[test]
fn help_and_version_stay_silent() {
    let workspace = BrWorkspace::new();

    for flag in ["--help", "--version"] {
        let out = sh(
            &workspace,
            &format!(r#""$BR" {flag} >out.txt 2>err.txt; echo "status=$?"; cat err.txt"#),
        );
        let text = stdout_of(&out);
        assert!(text.contains("status=0"), "{flag}: {text:?}");
        assert!(
            !text.contains("FAILED"),
            "{flag} exits zero and must get no banner: {text:?}"
        );
        let stdout = std::fs::read_to_string(workspace.root.join("out.txt")).expect("out.txt");
        assert!(!stdout.is_empty(), "{flag} should still print to stdout");
    }
}

/// A fatal panic is a nonzero exit and gets the banner.
///
/// Reached through `BD_PANIC_FOR_TEST`, a `debug_assertions`-only trigger in
/// `main.rs` that exists solely for this test: nothing in `br`'s own inputs
/// panics, and this is the exit path where a confusing scrollback costs the
/// most, so it is the last one that should rest on inspection alone.
///
/// The status differs by profile — 101 unwinding (what `cargo test` builds),
/// 134 (`SIGABRT`) under the release profile's `panic = "abort"`. The release
/// half is verified by hand, since the trigger cannot exist in a real release
/// build; see `docs/` and the PR description for the command.
#[test]
fn a_fatal_panic_still_names_itself() {
    let workspace = BrWorkspace::new();

    let out = sh(
        &workspace,
        r#"BD_PANIC_FOR_TEST=1 "$BR" list 2>&1 | tail -1"#,
    );

    assert_eq!(
        last_line(&out),
        format!(
            "br: FAILED (PANIC, exit {})",
            beads_rust::exit::panic_exit_status()
        ),
        "a panic is the most catastrophic exit and the most likely to leave a \
         truncated scrollback; it must name itself last too"
    );
}

/// Pending stdout bytes must be flushed BEFORE the banner is written.
///
/// stdout is a `LineWriter`, so it only ever holds bytes when a printer stopped
/// mid-line — and no command in the tree does that *and then fails*, so the
/// flush inside `emit_exit_banner` is unobservable through any real command.
/// That is exactly the kind of "it's obviously fine" code this feature exists to
/// distrust, so `BD_PANIC_FOR_TEST=partial-stdout` manufactures the condition:
/// an unterminated stdout line, then a fatal panic.
///
/// Without the flush, std's exit cleanup emits those bytes *after* the banner
/// under `2>&1` and the surviving line is the fragment instead. `ends_with`
/// rather than `==` because the fragment has no newline, so the banner
/// legitimately continues that same line — the assertion is about which bytes
/// come last, which is the whole point.
#[test]
fn pending_stdout_is_flushed_before_the_banner() {
    let workspace = BrWorkspace::new();

    let out = sh(
        &workspace,
        r#"BD_PANIC_FOR_TEST=partial-stdout "$BR" list 2>&1 | tail -1"#,
    );
    let line = last_line(&out);
    let banner = format!(
        "br: FAILED (PANIC, exit {})",
        beads_rust::exit::panic_exit_status()
    );

    assert!(
        line.ends_with(&banner),
        "the banner must be the last bytes on the stream even when stdout was \
         left mid-line; got {line:?}"
    );
    assert!(
        line.starts_with("PARTIAL-STDOUT-NO-NEWLINE"),
        "the partial stdout line should have been flushed onto the stream just \
         before the banner, not lost and not emitted after it; got {line:?}"
    );
}

/// The banner identifies the name it was *invoked* as, not a hardcoded one.
///
/// `bd` is a symlink to the `br` binary in every deployment in this fleet, and
/// an agent skimming scrollback for the command it ran needs to see that name.
#[test]
fn banner_uses_the_invoked_name() {
    let workspace = BrWorkspace::new();
    init(&workspace);

    // Same binary, reached through a differently named symlink.
    let out = sh(
        &workspace,
        r#"ln -sf "$BR" ./bd && ./bd create "" --prefix ct --json 2>&1 | tail -1"#,
    );

    assert_eq!(
        last_line(&out),
        "bd: FAILED (VALIDATION_FAILED, exit 4)",
        "the banner must self-identify as the command the caller actually ran"
    );
}

/// Delete the database `init` created so `doctor` has a check to fail.
fn remove_db(workspace: &BrWorkspace) {
    let beads = workspace.root.join(".beads");
    let entries = std::fs::read_dir(&beads).expect("read .beads");
    let mut removed = 0;
    for entry in entries.flatten() {
        let path: std::path::PathBuf = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("db") {
            std::fs::remove_file(&path).expect("remove db");
            removed += 1;
        }
    }
    assert!(
        removed > 0,
        "expected a .db file to remove in {}; without one `doctor` would pass \
         and the ordering assertions would be vacuous",
        Path::new(&beads).display()
    );
}
