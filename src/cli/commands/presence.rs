//! Undocumented `bd working` / `bd idle` commands for lifecycle-hook
//! presence tracking. See beads1-2leux for the design.
//!
//! Both no-op silently when `BD_AGENT_ID` isn't set, so hook configs
//! that fire `bd working &` from any shell are safe.

use crate::config;
use crate::error::Result;
use crate::output::OutputContext;
use crate::storage::presence::PresenceState;
use chrono::Utc;

/// Execute `bd working`.
///
/// # Errors
///
/// Returns an error only if storage open / write fails; missing
/// `BD_AGENT_ID` is a silent success.
pub fn execute_working(cli: &config::CliOverrides, _ctx: &OutputContext) -> Result<()> {
    set_state(PresenceState::Working, cli)
}

/// Execute `bd idle`.
///
/// # Errors
///
/// Returns an error only if storage open / write fails; missing
/// `BD_AGENT_ID` is a silent success.
pub fn execute_idle(cli: &config::CliOverrides, _ctx: &OutputContext) -> Result<()> {
    set_state(PresenceState::Idle, cli)
}

fn set_state(state: PresenceState, cli: &config::CliOverrides) -> Result<()> {
    let Ok(beads_dir) = config::discover_beads_dir_with_cli(cli) else {
        return Ok(());
    };
    // Storage must be opened before identity resolution now — the
    // fallback (BD_AGENT_ID unset) infers identity from the watchers
    // table, which lives in this same DB. Any failure up to and
    // including identity resolution stays a silent no-op, matching
    // this command's "safe from any shell" contract; only a failure
    // in the actual presence write propagates.
    let Ok((mut storage, _paths)) =
        config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)
    else {
        return Ok(());
    };

    let Some(prefix) = config::resolve_agent_identity_with_storage(&storage).ok() else {
        return Ok(());
    };

    storage.set_presence(&prefix, state, Utc::now())?;
    Ok(())
}
