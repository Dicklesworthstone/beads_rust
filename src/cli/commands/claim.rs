//! Claim command implementation.

use crate::cli::ClaimArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::util::id::{IdResolver, ResolverConfig};
use crate::util::lease::{generate_lease_id, lease_expires_at};
use chrono::{DateTime, Utc};
use serde::Serialize;

/// JSON output structure for claim results.
#[derive(Serialize)]
struct ClaimOutput {
    id: String,
    lease_id: String,
    lease_owner: String,
    lease_expires_at: DateTime<Utc>,
    lease_heartbeat_at: DateTime<Utc>,
}

/// Execute the claim command.
///
/// # Errors
///
/// Returns an error if the lease cannot be acquired or database operations fail.
pub fn execute(args: &ClaimArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let actor = config::resolve_actor(&config_layer);

    let resolver = build_resolver(&config_layer, &storage_ctx.storage);
    let resolved_ids = resolve_target_ids(args, &beads_dir, &resolver, &storage_ctx.storage)?;

    if args.lease_id.is_some() && resolved_ids.len() > 1 {
        return Err(BeadsError::validation(
            "lease_id",
            "lease_id can only be provided when claiming a single issue",
        ));
    }

    if args.ttl_seconds <= 0 {
        return Err(BeadsError::validation(
            "ttl",
            "lease TTL must be a positive number of seconds",
        ));
    }

    let mut outputs = Vec::new();
    let storage = &mut storage_ctx.storage;

    for id in &resolved_ids {
        let lease_id = args.lease_id.clone().unwrap_or_else(generate_lease_id);
        let now = Utc::now();
        let expires_at = lease_expires_at(now, args.ttl_seconds);

        storage.claim_issue(id, &actor, &lease_id, expires_at, now)?;
        crate::util::set_last_touched_id(&beads_dir, id);

        if ctx.is_json() {
            outputs.push(ClaimOutput {
                id: id.clone(),
                lease_id: lease_id.clone(),
                lease_owner: actor.clone(),
                lease_expires_at: expires_at,
                lease_heartbeat_at: now,
            });
        } else {
            println!(
                "Claimed {id} lease_id={lease_id} expires_at={}",
                expires_at.to_rfc3339()
            );
        }
    }

    if ctx.is_json() {
        ctx.json_pretty(&outputs);
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}

fn build_resolver(config_layer: &config::ConfigLayer, _storage: &SqliteStorage) -> IdResolver {
    let id_config = config::id_config_from_layer(config_layer);
    IdResolver::new(ResolverConfig::with_prefix(id_config.prefix))
}

fn resolve_target_ids(
    args: &ClaimArgs,
    beads_dir: &std::path::Path,
    resolver: &IdResolver,
    storage: &SqliteStorage,
) -> Result<Vec<String>> {
    let mut ids = args.ids.clone();
    if ids.is_empty() {
        let last_touched = crate::util::get_last_touched_id(beads_dir);
        if last_touched.is_empty() {
            return Err(BeadsError::validation(
                "ids",
                "no issue IDs provided and no last-touched issue",
            ));
        }
        ids.push(last_touched);
    }

    let resolved_ids = resolver.resolve_all(
        &ids,
        |id| storage.id_exists(id).unwrap_or(false),
        |hash| storage.find_ids_by_hash(hash).unwrap_or_default(),
    )?;

    Ok(resolved_ids.into_iter().map(|r| r.id).collect())
}
