//! `bd admin reload` — politely ask all running `bd watch` processes
//! to exit so they can be re-launched against a freshly-installed
//! binary.
//!
//! Mechanism: write the current unix timestamp to `config.reload_at`
//! and a spread window to `config.reload_spread`. Every `bd watch`
//! records `reload_at` at startup and re-reads it on each poll tick;
//! when the stored value is newer, the watcher rolls a random
//! `0..spread` second sleep, then prints `BD_RELOAD` and exits.
//!
//! The jittered exit is critical when N agents share one harness:
//! without it they all re-spawn within the same second and trip the
//! LLM API rate-limit. With a default 30s spread, ten agents land
//! ~3s apart on average.

use crate::cli::ReloadArgs;
use crate::config;
use crate::error::Result;
use crate::output::OutputContext;
use chrono::Utc;

/// Config key holding the most recent reload-request unix timestamp.
pub const RELOAD_KEY: &str = "reload_at";

/// Config key holding the spread (seconds) chosen for that reload.
pub const RELOAD_SPREAD_KEY: &str = "reload_spread";

/// Default spread when the operator omits `--spread` (or when an old
/// `bd admin reload` writes only `reload_at` and not the spread key).
/// Picked to keep ten agents > 2s apart on average.
pub const DEFAULT_SPREAD_SECS: u64 = 30;

/// Execute `bd admin reload`.
///
/// # Errors
///
/// Returns an error if storage open or the config write fails.
pub fn execute(
    args: &ReloadArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let now = Utc::now();
    let ts = now.timestamp();
    storage_ctx.storage.set_config(RELOAD_KEY, &ts.to_string())?;
    storage_ctx
        .storage
        .set_config(RELOAD_SPREAD_KEY, &args.spread.to_string())?;

    let n = storage_ctx
        .storage
        .active_watcher_prefixes(now, crate::storage::watchers::WATCHER_TTL_SECONDS)?
        .len();

    if ctx.is_json() {
        ctx.json_pretty(&serde_json::json!({
            "reload_at": ts,
            "spread_secs": args.spread,
            "active_watchers": n,
        }));
    } else {
        let spread_note = if args.spread == 0 {
            "no spread, immediate".to_string()
        } else {
            format!("spread over {}s", args.spread)
        };
        ctx.success(&format!(
            "reload requested at {} ({} active watchers, {})",
            now.to_rfc3339(),
            n,
            spread_note,
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

/// Read the spread (seconds) the operator picked for the most recent
/// reload. Falls back to `DEFAULT_SPREAD_SECS` when the row is
/// missing (so old reloads still get jitter under the new bd watch).
///
/// # Errors
///
/// Returns an error if the DB query itself fails.
pub fn read_spread(storage: &crate::storage::SqliteStorage) -> Result<u64> {
    Ok(storage
        .get_config(RELOAD_SPREAD_KEY)?
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SPREAD_SECS))
}
