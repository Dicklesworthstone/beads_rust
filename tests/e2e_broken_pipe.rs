//! A downstream reader that closes the pipe early (`br list | head`) must end
//! `br` the way it ends every other Unix filter (#434).
//!
//! Text output dies quietly by `SIGPIPE`; structured JSON output keeps the
//! broken-pipe-is-not-an-error contract from `d14ec978` and exits 0. Neither
//! may abort: before the fix the text path panicked inside `println!`, and
//! `panic = "abort"` turned that into `SIGABRT` plus a core dump.
#![cfg(unix)]

mod common;

use common::cli::{BrWorkspace, run_br};
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};

/// `SIGPIPE` on every Unix target this crate builds for.
const SIGPIPE: i32 = 13;
/// `SIGABRT` — the status the bug produced (core dump, exit 134).
const SIGABRT: i32 = 6;

/// Run `br` with a stdout pipe whose read end is already closed, so the
/// child's very first write hits `EPIPE` regardless of scheduling or output
/// size, and return its exit status plus captured stderr.
fn run_br_with_closed_stdout(workspace: &BrWorkspace, args: &[&str]) -> (ExitStatus, String) {
    let (reader, writer) = io::pipe().expect("create stdout pipe");
    drop(reader);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_br"));
    cmd.args(args)
        .current_dir(&workspace.root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped());
    // Mirror the harness environment: hermetic HOME, no colour, no inherited
    // br/beads configuration that could change the output mode under test.
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("BD_")
            || name.starts_with("BEADS_")
            || matches!(
                name.as_ref(),
                "BR_DISABLE_READ_ONLY_FAST_OPEN"
                    | "BR_OUTPUT_FORMAT"
                    | "TOON_DEFAULT_FORMAT"
                    | "TOON_STATS"
            )
        {
            cmd.env_remove(&key);
        }
    }
    cmd.env("HOME", &workspace.root)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "error");

    let output = cmd.output().expect("spawn br with closed stdout");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_ne!(
        output.status.signal(),
        Some(SIGABRT),
        "br aborted on a closed pipe; stderr: {stderr}"
    );
    (output.status, stderr)
}

fn seeded_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = run_br(
        &workspace,
        ["create", "Survive a closed pipe", "-p", "2"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    workspace
}

#[test]
fn text_list_terminates_by_sigpipe_when_reader_closes_early() {
    let workspace = seeded_workspace();

    let (status, stderr) = run_br_with_closed_stdout(&workspace, &["list"]);

    assert_eq!(
        status.signal(),
        Some(SIGPIPE),
        "text output should die by SIGPIPE like any Unix filter, got {status:?}; stderr: {stderr}"
    );
}

#[test]
fn text_ready_terminates_by_sigpipe_when_reader_closes_early() {
    let workspace = seeded_workspace();

    let (status, stderr) = run_br_with_closed_stdout(&workspace, &["ready"]);

    assert_eq!(
        status.signal(),
        Some(SIGPIPE),
        "text output should die by SIGPIPE like any Unix filter, got {status:?}; stderr: {stderr}"
    );
}

#[test]
fn json_list_swallows_broken_pipe_and_exits_zero() {
    let workspace = seeded_workspace();

    let (status, stderr) = run_br_with_closed_stdout(&workspace, &["list", "--json"]);

    assert!(
        status.success(),
        "JSON output classifies a broken pipe as a non-error (d14ec978), got {status:?}; stderr: {stderr}"
    );
}

#[test]
fn robot_ready_swallows_broken_pipe_and_exits_zero() {
    let workspace = seeded_workspace();

    let (status, stderr) = run_br_with_closed_stdout(&workspace, &["ready", "--robot"]);

    assert!(
        status.success(),
        "robot JSON output classifies a broken pipe as a non-error, got {status:?}; stderr: {stderr}"
    );
}
