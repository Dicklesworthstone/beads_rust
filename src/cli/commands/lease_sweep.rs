//! Lease sweeper command implementation.

use crate::cli::LeaseSweepArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::output::OutputContext;
use crate::storage::LeaseSweepSummary;
use chrono::{Duration, Utc};
use serde::Serialize;
use std::thread;
use std::time::Duration as StdDuration;
use tracing::info;

#[derive(Serialize)]
struct LeaseSweepOutput {
    expired: usize,
    stale_marked: usize,
    orphaned_marked: usize,
    reclaimed_leases: usize,
    swept_at: String,
}

fn run_once(args: &LeaseSweepArgs, cli: &config::CliOverrides) -> Result<LeaseSweepSummary> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let actor = config::resolve_actor(&config_layer);

    let now = Utc::now();
    let stale_after = Duration::minutes(args.stale_after_minutes);
    let orphan_after = Duration::minutes(args.orphan_after_minutes);

    let summary =
        storage_ctx
            .storage
            .sweep_expired_leases(&actor, now, stale_after, orphan_after)?;

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(summary)
}

/// Execute the lease sweeper command.
///
/// # Errors
///
/// Returns an error if validation or database operations fail.
pub fn execute(
    args: &LeaseSweepArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    if args.interval_seconds == 0 {
        return Err(BeadsError::validation(
            "interval",
            "interval must be a positive number of seconds",
        ));
    }
    if args.stale_after_minutes <= 0 {
        return Err(BeadsError::validation(
            "stale-after",
            "stale-after must be a positive number of minutes",
        ));
    }
    if args.orphan_after_minutes <= args.stale_after_minutes {
        return Err(BeadsError::validation(
            "orphan-after",
            "orphan-after must be greater than stale-after",
        ));
    }

    loop {
        let now = Utc::now();
        let summary = run_once(args, cli)?;

        info!(
            expired = summary.expired,
            stale_marked = summary.stale_marked,
            orphaned_marked = summary.orphaned_marked,
            reclaimed = summary.reclaimed_leases,
            "lease sweep complete"
        );

        if ctx.is_json() {
            let output = LeaseSweepOutput {
                expired: summary.expired,
                stale_marked: summary.stale_marked,
                orphaned_marked: summary.orphaned_marked,
                reclaimed_leases: summary.reclaimed_leases,
                swept_at: now.to_rfc3339(),
            };
            ctx.json_pretty(&output);
        } else {
            println!(
                "Lease sweep: expired={} stale_marked={} orphaned_marked={} reclaimed={}",
                summary.expired,
                summary.stale_marked,
                summary.orphaned_marked,
                summary.reclaimed_leases
            );
        }

        if !args.daemon {
            break;
        }

        thread::sleep(StdDuration::from_secs(args.interval_seconds));
    }

    Ok(())
}
