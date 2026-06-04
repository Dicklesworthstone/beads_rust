//! `bd admin reload` — politely ask all running `bd watch` processes
//! to exit so they can be re-launched against a freshly-installed
//! binary.
//!
//! Mechanism: write the current unix timestamp to `config.reload_at`.
//! Every `bd watch` records the value at startup and re-reads it on
//! each poll tick; if the stored value is newer than its own startup
//! snapshot, the watcher prints a `BD_RELOAD` line and exits cleanly.
//!
//! The harness still has to relaunch the watcher — the `bdwatch` skill
//! documents how agents should react to a `BD_RELOAD` notification.

use crate::config;
use crate::error::Result;
use crate::output::OutputContext;
use chrono::Utc;

/// Config key holding the most recent reload-request unix timestamp.
pub const RELOAD_KEY: &str = "reload_at";

/// Execute `bd admin reload`.
///
/// # Errors
///
/// Returns an error if storage open or the config write fails.
pub fn execute(cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let now = Utc::now();
    let ts = now.timestamp();
    storage_ctx
        .storage
        .set_config(RELOAD_KEY, &ts.to_string())?;

    let n = storage_ctx
        .storage
        .active_watcher_prefixes(now, crate::storage::watchers::WATCHER_TTL_SECONDS)?
        .len();

    if ctx.is_json() {
        ctx.json_pretty(&serde_json::json!({
            "reload_at": ts,
            "active_watchers": n,
        }));
    } else {
        ctx.success(&format!(
            "reload requested at {} ({} active watchers will exit on their next tick)",
            now.to_rfc3339(),
            n
        ));
    }
    Ok(())
}

/// Read the current `reload_at` value from config, or 0 if unset / unparsable.
///
/// # Errors
///
/// Returns an error if the DB query itself fails.
pub fn read_generation(storage: &crate::storage::SqliteStorage) -> Result<i64> {
    Ok(storage
        .get_config(RELOAD_KEY)?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0))
}
