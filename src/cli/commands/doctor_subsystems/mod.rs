//! Doctor subsystems — foundation for the WP1 (chokepoint) refactor of
//! `br doctor --repair`.
//!
//! These modules are landed standalone and do **not** yet rewire the
//! existing [`crate::cli::commands::doctor`] `repair_*` call sites — that
//! migration happens in WP3–WP12 of the world-class doctor program. WP1
//! lands the *foundation*:
//!
//! - [`mutate`] — the single chokepoint every disk write under
//!   `--repair` will eventually flow through. Implements the 8-step
//!   contract (lock → before-hash → preconditions → verbatim backup →
//!   plan → atomic execute → after-hash → record).
//! - [`run_dir`] — `.doctor/runs/<run-id>/` artifact directory used to
//!   hold backups, `actions.jsonl`, `report.json`, and `undo.sh`.
//! - [`exit_codes`] — the structured doctor exit-code dictionary
//!   (0 / 1 / 2 / 3 / 4 / 5 / 6 / 64 / 66 / 73 / 74) per the
//!   project's safety envelope.
//! - [`capabilities_doctor`] — emits `br.doctor.capabilities.v1`
//!   JSON describing the doctor's contract to AI agents.
//! - [`refuse_gates`] — refuse-unsafe gates that run before any
//!   `--repair` execution (schema downgrade, recovery-fingerprint
//!   integrity).
//!
//! The chokepoint forbids file deletion (per AGENTS.md). Anything that
//! "needs to delete" must use [`mutate::Op::Rename`] to move into the
//! per-run quarantine area. See `safety_envelope.md` for the full list
//! of invariants.

pub mod capabilities_doctor;
pub mod exit_codes;
pub mod mutate;
pub mod refuse_gates;
pub mod run_dir;
pub mod schema_migration;
pub mod selftest;
pub mod surface;
