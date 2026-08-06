//! The single process-exit funnel, and the final-line banner every nonzero
//! exit carries.
//!
//! # The bug this exists to close
//!
//! `br`'s stream routing was already correct before this module: a failing
//! command writes **zero bytes to stdout**, the whole error envelope to
//! stderr, and exits nonzero. Piping alone therefore hid nothing — the
//! envelope still reached the terminal.
//!
//! What hid the failure was the combination of `2>&1` (merging the error into
//! the pipe) with a *truncating* filter:
//!
//! ```text
//! $ br create "" --prefix ct --json 2>&1 | tail -3
//!     }
//!   }
//! }
//! ```
//!
//! Every discriminating token — the `"error"` key, the code, the message — is
//! at the **top** of the envelope, and `tail` shows the **bottom**. What
//! survives truncation is closing braces, which is exactly what a *success*
//! envelope also ends in. Three agents (and the author of the bead that asked
//! for this) read those braces as success; one filed a P1 bug that did not
//! exist.
//!
//! So the entire value of this feature is in the *position*. A banner emitted
//! first is the first thing a truncating filter cuts off. It must be **last**:
//!
//! ```text
//! br: FAILED (VALIDATION_FAILED, exit 4)
//! ```
//!
//! # What this does NOT fix — the exit code
//!
//! **The exit code cannot be rescued at the tool layer.** `$?` after a
//! pipeline is the *last* command's status by shell semantics, so
//! `br ... | tail` reports `tail`'s `0` no matter what `br` does. Only
//! `set -o pipefail`, `${PIPESTATUS[0]}`, or not piping recovers it.
//!
//! This module hardens the **text** channel only. That distinction matters,
//! because a visible-failure banner is easy to mistake for having closed the
//! whole hole, and the exit-code half is the half that has bitten hardest.
//!
//! # The governing rule
//!
//! **The banner is a function of the exit status, not of "something bad
//! happened."** Every `std::process::exit` in this crate goes through
//! [`exit_with_status`], which decides from the status alone. Two consequences
//! that are easy to get wrong if the decision is made anywhere else:
//!
//! - The broken-pipe guard in `main.rs` exits **zero** (`br list | head` is a
//!   normal operation, not a failure), so it gets **no banner** — even though
//!   it is reached through a panic.
//! - A panic that *is* fatal exits nonzero, so it gets one.
//!
//! Deriving from the status makes both fall out by construction instead of
//! resting on an exception someone has to remember.
//!
//! # Truthful wording: [`ExitKind`]
//!
//! Not every nonzero exit is a failure. `br version --check` exits 1 to say
//! *"an update is available"* — nothing failed. Printing `FAILED
//! (UPDATE_AVAILABLE, exit 1)` would be a message stating a reason that is not
//! the reason, the exact defect class this feature exists to remove. Exempting
//! it instead would reintroduce "some nonzero exits are silent", which is the
//! hole. So emission stays unconditional and the *wording* tracks reality; see
//! [`ExitKind`].
//!
//! # Design decisions, and why the obvious alternatives are worse
//!
//! - **Unconditional, never gated on `isatty`/pipe-detection.** A
//!   pipe-detected banner behaves differently interactively than in a script,
//!   which manufactures "works when I test it by hand, silent in the script" —
//!   its own bug class, and a worse one than the one being fixed.
//! - **stderr, single line, no markup.** stderr keeps `--json` consumers
//!   byte-clean (they read stdout); one line survives `tail -1`; no ANSI means
//!   an agent's `grep` matches.
//! - **stdout is flushed first.** Rust flushes stdout at process exit, so
//!   without an explicit flush here a buffered partial line could be written
//!   *after* the banner under `2>&1` and steal the last position.
//! - **Silence is never used to mean success.** A `--quiet` contract where
//!   "no output means it worked" is satisfiable by a process that was
//!   `SIGKILL`ed before doing anything — the same observation as success.
//!   Hence a positive marker on failure rather than an absence on success.

use std::io::Write;

/// Fallback name for the banner if `argv[0]` is unusable (empty, or not valid
/// UTF-8). Never observed in practice; present so the banner can always be
/// emitted rather than skipped.
const FALLBACK_NAME: &str = "br";

/// Banner label for a clap usage/parse failure.
///
/// clap's own `Error::exit()` calls `std::process::exit` from inside
/// `Cli::parse()`, which would bypass this funnel entirely, so `main` uses
/// `try_parse` and routes the error here.
pub const USAGE_ERROR: &str = "USAGE_ERROR";

/// Banner label for a fatal panic. See [`panic_exit_status`] for the status.
pub const PANIC: &str = "PANIC";

/// Whether a nonzero exit means the operation *failed*, or merely reports a
/// distinct non-failure outcome.
///
/// The variant is an explicit argument at every call site rather than something
/// inferred from the label, so a future exit path has to *state* which it is
/// instead of inheriting a default that may be a lie.
///
/// ```text
/// Failure: br: FAILED (VALIDATION_FAILED, exit 4)
/// Notice:  br: UPDATE_AVAILABLE (exit 1)
/// ```
///
/// Both shapes are one self-identifying line naming the label and the status —
/// the property that makes them survive `tail -1` — and neither asserts
/// anything untrue. This mirrors the close envelope keying `"error"` at exit 3
/// and `"notice"` at exit 0: the label is not allowed to disagree with what
/// happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    /// The command did not do what it was asked to do.
    Failure,
    /// The command worked; the nonzero status is a *result*, not an error.
    Notice,
}

/// The status a fatal panic ends the process with, as the *shell* sees it.
///
/// The two profiles differ, and a banner naming the wrong number would be
/// worse than one naming none. A wrong conclusion has already been drawn in
/// this repo from testing only one profile (the broken-pipe abort):
///
/// | profile | `panic` setting | how the process dies | `$?` |
/// |---------|-----------------|----------------------|------|
/// | `dev`/`test` (what `cargo test` builds) | `unwind` | unwinds out of `main` | 101 |
/// | `release` (what ships) | `abort` | `abort()` → `SIGABRT` | 134 |
#[must_use]
pub const fn panic_exit_status() -> i32 {
    if cfg!(panic = "abort") { 134 } else { 101 }
}

/// Is a panic in *this* thread guaranteed to determine the process's exit
/// status?
///
/// Under `panic = "abort"` any panic anywhere kills the process, so yes.
/// Under unwinding, only the main thread's panic does: a panicking worker
/// thread (e.g. one of `indicatif`'s tick threads) leaves the process running,
/// and announcing a nonzero exit that is not going to happen would be its own
/// false statement.
#[must_use]
pub fn panic_is_fatal() -> bool {
    cfg!(panic = "abort") || std::thread::current().name() == Some("main")
}

/// The name the user actually typed, for a self-identifying banner.
///
/// `bd` is a symlink to the `br` binary, and an agent skimming scrollback for
/// the command it ran needs to see the name it ran. `argv[0]`'s file stem is
/// the cheapest thing that gets this right for both.
#[must_use]
pub fn invoked_name() -> String {
    std::env::args_os()
        .next()
        .and_then(|arg0| {
            std::path::Path::new(&arg0)
                .file_stem()
                .and_then(|stem| stem.to_str().map(str::to_owned))
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| FALLBACK_NAME.to_string())
}

/// Render the banner line (without its trailing newline).
///
/// Split out from [`emit_exit_banner`] so the exact wording is unit-testable
/// without spawning a process. The e2e tests assert on the wording too, so
/// drift fails in both places.
#[must_use]
pub fn banner_line(name: &str, kind: ExitKind, label: &str, status: i32) -> String {
    match kind {
        ExitKind::Failure => format!("{name}: FAILED ({label}, exit {status})"),
        ExitKind::Notice => format!("{name}: {label} (exit {status})"),
    }
}

/// Write the banner to stderr as the process's final output.
///
/// Flushes stdout first: under `2>&1` both streams share one pipe, and Rust's
/// stdout is a `LineWriter` that is *also* flushed again at process exit, so
/// any buffered partial line would otherwise land **after** this banner and
/// take the last-line slot the banner exists to occupy.
///
/// All write errors are deliberately ignored. If stderr is gone there is
/// nowhere to report that, and a `?`/`unwrap` here would panic on the failed
/// write — which is the very failure mode `install_broken_pipe_guard` in
/// `main.rs` exists to keep from dumping a core.
pub fn emit_exit_banner(kind: ExitKind, label: &str, status: i32) {
    let _ = std::io::stdout().flush();
    let mut stderr = std::io::stderr().lock();
    // One `write_all` of one line: nothing can interleave inside the banner.
    let _ = stderr.write_all(banner_line(&invoked_name(), kind, label, status).as_bytes());
    let _ = stderr.write_all(b"\n");
    let _ = stderr.flush();
}

/// Terminate the process, emitting the banner iff `status` is nonzero.
///
/// **Every** `std::process::exit` in this crate goes through here, so that
/// "nonzero exit ⇒ exactly one final self-identifying line" is an invariant of
/// one function rather than a convention nine call sites have to remember.
///
/// The zero case is passed straight through with no banner. That is not a
/// special case bolted on — it is the governing rule (the banner is a function
/// of the status) and it is what makes `br list | head` stay silent, and what
/// lets callers whose status is *computed* (`br lint`, whose `exit_code()` may
/// legitimately be 0) avoid a branch of their own.
///
/// One exit path deliberately does not call this: `admin watch`'s
/// `exit(0)`, which is unconditionally zero and so has no invariant to
/// protect.
///
/// What cannot be covered, stated rather than left implicit: death by signal
/// (`SIGKILL`, `SIGTERM`) runs no user code at all, so no banner is possible
/// there. Fatal panics *are* covered, from the hook in `main.rs`.
pub fn exit_with_status(status: i32, kind: ExitKind, label: &str) -> ! {
    if status != 0 {
        emit_exit_banner(kind, label, status);
    }
    std::process::exit(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_banner_names_code_and_status_on_one_line() {
        let line = banner_line("br", ExitKind::Failure, "VALIDATION_FAILED", 4);
        assert_eq!(line, "br: FAILED (VALIDATION_FAILED, exit 4)");
        assert!(
            !line.contains('\n'),
            "banner must be a single line so `tail -1` shows all of it"
        );
    }

    #[test]
    fn notice_banner_does_not_claim_failure() {
        let line = banner_line("br", ExitKind::Notice, "UPDATE_AVAILABLE", 1);
        assert_eq!(line, "br: UPDATE_AVAILABLE (exit 1)");
        assert!(
            !line.contains("FAILED"),
            "a non-failure exit must not say FAILED: {line}"
        );
        // Still self-identifying and still names the status, which is what
        // makes it useful after `tail -1`.
        assert!(line.starts_with("br: "));
        assert!(line.contains("exit 1"));
    }

    #[test]
    fn banner_uses_the_name_it_was_invoked_as() {
        // `bd` is a symlink to the `br` binary; the banner has to identify the
        // command the caller actually ran.
        assert!(
            banner_line("bd", ExitKind::Failure, "ISSUE_NOT_FOUND", 3).starts_with("bd: FAILED")
        );
    }

    #[test]
    fn banner_carries_no_console_markup() {
        for kind in [ExitKind::Failure, ExitKind::Notice] {
            let line = banner_line("br", kind, "IO_ERROR", 8);
            assert!(
                !line.contains('\u{1b}'),
                "an ANSI escape would break an agent grepping for the label: {line}"
            );
        }
    }

    #[test]
    fn invoked_name_is_a_bare_stem() {
        let name = invoked_name();
        assert!(!name.is_empty());
        assert!(
            !name.contains('/') && !name.contains('\\'),
            "expected a bare file stem, got {name}"
        );
    }

    #[test]
    fn panic_status_matches_this_profile() {
        // Cargo refuses to apply `panic = "abort"` to the test profile, so
        // unit tests always unwind.
        assert_eq!(panic_exit_status(), 101);
    }

    #[test]
    fn panic_on_the_test_thread_is_not_treated_as_fatal_under_unwind() {
        // A `cargo test` thread is named after its test, not "main", and a
        // panic there is caught by the harness rather than ending the process.
        assert!(!panic_is_fatal());
    }
}
