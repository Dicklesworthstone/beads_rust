//! Structured doctor exit-code dictionary (R-004).
//!
//! These exit codes are stable contract surface — CI, agent scripts, and
//! the playbook all parse them. Numeric values match the world-class
//! doctor methodology and the project's `safety_envelope.md`. They are
//! distinct from the [`crate::error::ErrorCode::exit_code`] dictionary
//! used by ordinary `br` commands; they are wider because the doctor
//! has additional refusal modes (concurrency-lost, unsafe-precondition,
//! online-required).
//!
//! The semantics are paraphrased here; the canonical reference is
//! [`refuse_gates`](super::refuse_gates) and the methodology document
//! `references/methodology/MUTATE-CHOKEPOINT.md`.
//!
//! - **0** — `Healthy` — every check passed, no findings.
//! - **1** — `FindingsPresent` — diagnostic mode found problems but
//!   `--repair` was not requested. Default soft-fail for CI.
//! - **2** — `FixPartial` — `--repair` ran; some fixers succeeded,
//!   others failed. The actions.jsonl line records `ok: false` for the
//!   failed fixer.
//! - **3** — `FixFailedRolledBack` — `--repair` ran a fixer that hit a
//!   fault during atomic execution; the run was rolled back from the
//!   verbatim backup. Workspace state is unchanged.
//! - **4** — `RefusedUnsafe` — a precondition gate refused. Examples:
//!   write outside [`safety_envelope.md` §2 scopes][1], schema-version
//!   downgrade, recovery-fingerprint mismatch.
//! - **5** — `ConcurrencyLost` — could not acquire `.write.lock`
//!   within the 5 s envelope.
//! - **6** — `OnlineRequired` — a network-using detector requires
//!   `--online` (not yet wired but reserved).
//! - **64** — `UsageError` — clap rejected the invocation. Mirrors
//!   `EX_USAGE` from `<sysexits.h>`.
//! - **66** — `NoInput` — required input missing (e.g., the workspace
//!   was never `br init`-ed). Mirrors `EX_NOINPUT`.
//! - **73** — `CannotCreateOutput` — could not create the run-dir or
//!   actions.jsonl. Mirrors `EX_CANTCREAT`.
//! - **74** — `IoError` — generic I/O fault during a non-mutating
//!   operation. Mirrors `EX_IOERR`.
//!
//! [1]: <https://example.invalid/safety-envelope> "see analysis/safety_envelope.md"

#![allow(dead_code)] // WP1 foundation; consumed by WP3-WP12.

/// Structured doctor exit code.
///
/// Numeric values are stable contract; do **not** change them. Adding
/// new variants is fine if a fresh number is chosen — agent scripts
/// that mask `match c { 0 => .., _ => bail }` cope safely.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DoctorExitCode {
    /// Every check passed.
    Healthy = 0,
    /// Diagnostic mode found findings; `--repair` was not requested.
    FindingsPresent = 1,
    /// `--repair` ran; some fixers failed.
    FixPartial = 2,
    /// `--repair` attempted, faulted, and rolled back from backup.
    FixFailedRolledBack = 3,
    /// A precondition gate refused (scope, schema, fingerprint, …).
    RefusedUnsafe = 4,
    /// Workspace lock unavailable.
    ConcurrencyLost = 5,
    /// Network-using detector needs `--online`.
    OnlineRequired = 6,
    /// CLI parsing error (mirrors `EX_USAGE`).
    UsageError = 64,
    /// Required input missing (mirrors `EX_NOINPUT`).
    NoInput = 66,
    /// Could not create output artifact (mirrors `EX_CANTCREAT`).
    CannotCreateOutput = 73,
    /// Generic I/O error (mirrors `EX_IOERR`).
    IoError = 74,
}

impl DoctorExitCode {
    /// Numeric value of this exit code.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Stable kebab-case name for JSON output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::FindingsPresent => "findings_present",
            Self::FixPartial => "fix_partial",
            Self::FixFailedRolledBack => "fix_failed_rolled_back",
            Self::RefusedUnsafe => "refused_unsafe",
            Self::ConcurrencyLost => "concurrency_lost",
            Self::OnlineRequired => "online_required",
            Self::UsageError => "usage_error",
            Self::NoInput => "no_input",
            Self::CannotCreateOutput => "cannot_create_output",
            Self::IoError => "io_error",
        }
    }

    /// Short human-facing description.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Healthy => "every check passed",
            Self::FindingsPresent => "findings present; --repair not requested",
            Self::FixPartial => "--repair ran; some fixers failed",
            Self::FixFailedRolledBack => "--repair attempted, rolled back from backup",
            Self::RefusedUnsafe => "refused: precondition gate failed",
            Self::ConcurrencyLost => "refused: workspace lock unavailable",
            Self::OnlineRequired => "refused: --online flag required",
            Self::UsageError => "CLI usage error",
            Self::NoInput => "required input missing",
            Self::CannotCreateOutput => "could not create output artifact",
            Self::IoError => "I/O error during non-mutating operation",
        }
    }

    /// All variants in declaration order. Useful for emitting the
    /// dictionary in capabilities JSON.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::Healthy,
            Self::FindingsPresent,
            Self::FixPartial,
            Self::FixFailedRolledBack,
            Self::RefusedUnsafe,
            Self::ConcurrencyLost,
            Self::OnlineRequired,
            Self::UsageError,
            Self::NoInput,
            Self::CannotCreateOutput,
            Self::IoError,
        ]
    }
}

impl From<DoctorExitCode> for i32 {
    fn from(code: DoctorExitCode) -> Self {
        code.as_i32()
    }
}

/// Terminate the process with the given doctor exit code.
///
/// Reserved for the eventual `--repair` driver; current call sites still
/// flow through [`std::process::exit`] in [`crate::cli::commands::doctor`].
/// Documented here so the WP3-WP12 migration has a single helper to
/// switch to.
pub fn exit_with(code: DoctorExitCode) -> ! {
    std::process::exit(code.as_i32())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Numeric values are contract; freezing them in a test prevents
    /// accidental renumbering in future PRs.
    #[test]
    fn exit_codes_match_contract_values() {
        assert_eq!(DoctorExitCode::Healthy.as_i32(), 0);
        assert_eq!(DoctorExitCode::FindingsPresent.as_i32(), 1);
        assert_eq!(DoctorExitCode::FixPartial.as_i32(), 2);
        assert_eq!(DoctorExitCode::FixFailedRolledBack.as_i32(), 3);
        assert_eq!(DoctorExitCode::RefusedUnsafe.as_i32(), 4);
        assert_eq!(DoctorExitCode::ConcurrencyLost.as_i32(), 5);
        assert_eq!(DoctorExitCode::OnlineRequired.as_i32(), 6);
        assert_eq!(DoctorExitCode::UsageError.as_i32(), 64);
        assert_eq!(DoctorExitCode::NoInput.as_i32(), 66);
        assert_eq!(DoctorExitCode::CannotCreateOutput.as_i32(), 73);
        assert_eq!(DoctorExitCode::IoError.as_i32(), 74);
    }

    #[test]
    fn as_str_round_trips_via_all() {
        for code in DoctorExitCode::all() {
            // Names are non-empty, lowercase ASCII, and match a stable
            // pattern. We check the most basic shape only.
            let name = code.as_str();
            assert!(!name.is_empty());
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "name `{name}` must be snake_case"
            );
        }
    }

    #[test]
    fn from_doctor_exit_code_into_i32() {
        let n: i32 = DoctorExitCode::RefusedUnsafe.into();
        assert_eq!(n, 4);
    }

    #[test]
    fn description_is_non_empty_for_every_variant() {
        for code in DoctorExitCode::all() {
            assert!(!code.description().is_empty());
        }
    }
}
