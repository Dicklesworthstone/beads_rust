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
    let Some(prefix) = config::resolve_agent_identity().ok() else {
        return Ok(());
    };

    let beads_dir = match config::discover_beads_dir_with_cli(cli) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    let (mut storage, _paths) =
        config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;
    storage.set_presence(&prefix, state, Utc::now())?;
    Ok(())
}
