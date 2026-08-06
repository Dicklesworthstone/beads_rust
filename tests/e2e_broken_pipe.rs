//! E2E test for broken-pipe handling.
//!
//! `bd list | head` is the most ordinary thing an operator or an agent does.
//! Before the fix it crashed the process every time: the Rust runtime ignores
//! `SIGPIPE`, so the write to the closed pipe returned `EPIPE`, `println!`
//! panicked on the failed stdout write, and `panic = "abort"` turned that into
//! `SIGABRT` plus a multi-megabyte core dump.
//!
//! The crash was invisible in normal use — a pipeline reports the *last*
//! command's status, so the shell said 0 while the process was dying — which
//! is precisely why it survived long enough to litter a machine with 100+
//! cores. A test that merely runs `br list` to completion cannot catch this;
//! the bug only exists when the reader leaves early. So this test closes the
//! read end and asserts on how the child *died*, not on what it printed.

mod common;

use common::cli::{BrWorkspace, run_br};
use std::io::Read;
use std::process::{Command, Output, Stdio};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

/// Enough issues that the output comfortably exceeds a 64 KiB pipe buffer,
/// so the child is still writing when the reader goes away.
const ISSUE_COUNT: usize = 200;

fn seed(workspace: &BrWorkspace) {
    run_br(workspace, ["init"], "init");
    for i in 0..ISSUE_COUNT {
        run_br(
            workspace,
            [
                "create",
                &format!(
                    "issue {i} with a deliberately long title so the listing \
                     outgrows the pipe buffer and the writer is still going \
                     when the reader hangs up"
                ),
                "--prefix",
                "bp",
                "--type",
                "task",
            ],
            "seed",
        );
    }
}

/// Spawn `br <args>`, read a single line, then drop the read end and report
/// how the child terminated.
fn run_and_hang_up(workspace: &BrWorkspace, args: &[&str]) -> std::process::ExitStatus {
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .args(args)
        .current_dir(&workspace.root)
        .env("BD_AGENT_ID", "bptest")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn br");

    {
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut one = [0u8; 128];
        // Read a little, proving the child is producing output, then drop the
        // handle. Dropping closes our read end; the child's next write to a
        // pipe with no reader is what used to kill it.
        let _ = stdout.read(&mut one);
    }

    child.wait().expect("wait for br")
}

/// The regression test proper.
///
/// Asserts the process did not die *abnormally*. Both failure shapes must be
/// caught, because the panic manifests differently per profile and a test that
/// only knows one of them is worthless in the other:
///
/// | profile | `panic` setting | broken-pipe death |
/// |---------|-----------------|-------------------|
/// | `dev` (what `cargo test` builds) | `unwind` | exit code **101** |
/// | `release` (what ships) | `abort` | **SIGABRT** + core dump |
///
/// An earlier draft of this test asserted only "not SIGABRT" and therefore
/// passed happily with the fix removed — under `cargo test` the bug never
/// takes the abort path. Asserting on 101 as well is what makes it a real
/// regression test. Verified by hand: it fails on both counts with
/// `install_broken_pipe_guard()` commented out, and passes with it in.
#[cfg(unix)]
#[test]
fn list_does_not_die_abnormally_when_the_reader_hangs_up() {
    let workspace = BrWorkspace::new();
    seed(&workspace);

    let status = run_and_hang_up(&workspace, &["list", "--limit", "1000"]);
    assert_clean_pipe_death(status, "br list");
}

/// `search` streams through the same print path and had the same bug.
#[cfg(unix)]
#[test]
fn search_does_not_die_abnormally_when_the_reader_hangs_up() {
    let workspace = BrWorkspace::new();
    seed(&workspace);

    let status = run_and_hang_up(&workspace, &["search", "deliberately"]);
    assert_clean_pipe_death(status, "br search");
}

/// Shared assertion: a vanished reader must never produce an abnormal death.
#[cfg(unix)]
fn assert_clean_pipe_death(status: std::process::ExitStatus, what: &str) {
    assert_ne!(
        status.signal(),
        Some(SIGABRT),
        "{what} aborted (SIGABRT) on a broken pipe: {status:?}. \
         A panic on a failed stdout write is being turned into abort() by \
         panic=\"abort\", which also dumps a multi-megabyte core."
    );

    assert_ne!(
        status.code(),
        Some(101),
        "{what} panicked (exit 101) on a broken pipe: {status:?}. \
         `println!` treats the failed stdout write as unrecoverable; the \
         broken-pipe guard in src/main.rs is missing or no longer matches \
         std's panic message."
    );

    if let Some(sig) = status.signal() {
        assert_eq!(
            sig, SIGPIPE,
            "{what} died by an unexpected signal {sig}; only SIGPIPE is \
             acceptable here"
        );
    }
}

/// A vanished reader must stay SILENT — no failure banner.
///
/// `br` now writes a self-identifying final line to stderr on every nonzero
/// exit (`src/exit.rs`), and the broken-pipe guard above is reached *through a
/// panic*. If that banner were emitted for "a panic happened" rather than for
/// the status being exited with, `br list | head -1` — the most common pipeline
/// in this fleet — would start announcing `br: FAILED (PANIC, ...)` on a
/// completely normal operation. That would be a louder regression than the
/// silent-failure bug the banner exists to fix.
///
/// The guard exits **zero**, so by the rule that the banner is a function of the
/// exit status it must produce nothing at all. Asserted on stderr being *empty*,
/// not merely banner-free, because that is the observable an operator or an
/// agent actually reacts to.
#[cfg(unix)]
#[test]
fn a_vanished_reader_prints_nothing_at_all() {
    let workspace = BrWorkspace::new();
    seed(&workspace);

    // `sh` rather than the harness: the point is a real `| head -1`, and the
    // harness sets RUST_LOG=debug, which would fill stderr with tracing output
    // and make "stderr is empty" untestable.
    let out: Output = Command::new("sh")
        .arg("-c")
        .arg(r#""$BR" list --limit 1000 | head -1 >/dev/null; echo "status=$?""#)
        .current_dir(&workspace.root)
        .env("BR", assert_cmd::cargo::cargo_bin!("br"))
        .env("HOME", &workspace.root)
        .env("NO_COLOR", "1")
        .env("BD_AGENT_ID", "bptest")
        .env_remove("RUST_LOG")
        .output()
        .expect("run sh");

    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        stderr.is_empty(),
        "`br list | head -1` is a normal operation and must print nothing to \
         stderr; got {stderr:?}"
    );
    assert!(
        !stderr.contains("FAILED"),
        "the failure banner must never fire on a broken pipe: {stderr:?}"
    );

    // The pipeline's own status is `head`'s, so this asserts only that the shell
    // saw a normal completion — `br`'s own status is what the assertions above
    // cover.
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout.contains("status=0"), "unexpected: {stdout:?}");
}

/// Abort. Hardcoded rather than pulled from `libc`, which this crate does not
/// depend on (it forbids `unsafe`, so it has no use for it).
#[cfg(unix)]
const SIGABRT: i32 = 6;

/// Broken pipe.
#[cfg(unix)]
const SIGPIPE: i32 = 13;
