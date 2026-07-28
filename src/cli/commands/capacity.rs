//! Workflow capacity management commands (`br capacity`), GitHub #384
//! phase 4: audited issue-specific capacity exemptions.
//!
//! `br capacity exempt <id> --status <name> --provider <p> --reason <r>`
//! grants a narrowly scoped exemption: one issue, one named capacity.
//! `renew` extends an active exemption's expiry, `revoke` withdraws it, and
//! `exemptions` lists state plus the append-only audit history.
//!
//! Exemption records are auxiliary, project-local metadata (like gate
//! results): they are not synced through JSONL. Enforcement lives in the
//! storage counting engine — an active, authorized exemption excludes its
//! issue from the named capacity's counted total while keeping it visible
//! in queue metrics, and leaving the applicable status ends the exemption.

use super::{
    RoutedWorkspaceWriteLock, acquire_routed_workspace_write_lock,
    auto_import_storage_ctx_if_stale, resolve_issue_id,
};
use crate::cli::{
    CapacityCommands, CapacityExemptArgs, CapacityExemptionsArgs, CapacityRenewArgs,
    CapacityRevokeArgs,
};
use crate::close_policy::{CapacityExemptionHistoryRecord, CapacityExemptionRecord};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::output::OutputContext;
use crate::util::id::{IdResolver, ResolverConfig};
use crate::util::time::parse_flexible_timestamp;
use serde::Serialize;
use std::path::Path;

/// JSON payload for `br capacity exemptions`.
#[derive(Debug, Serialize)]
struct CapacityExemptionsOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    issue_id: Option<String>,
    exemptions: Vec<CapacityExemptionRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    history: Option<Vec<CapacityExemptionHistoryRecord>>,
}

/// Execute the capacity command.
///
/// # Errors
///
/// Returns an error if database operations fail, the issue cannot be
/// resolved, or the request violates the exemption policy.
pub fn execute(
    command: &CapacityCommands,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    match command {
        CapacityCommands::Exempt(args) => execute_exempt(args, cli, ctx, &beads_dir),
        CapacityCommands::Renew(args) => execute_renew(args, cli, ctx, &beads_dir),
        CapacityCommands::Revoke(args) => execute_revoke(args, cli, ctx, &beads_dir),
        CapacityCommands::Exemptions(args) => execute_exemptions(args, cli, ctx, &beads_dir),
    }
}

/// Map the mutually exclusive `--status` / `--group` flags onto the stored
/// capacity identity. Exactly one must be present.
fn resolve_capacity_flags(
    status: Option<&str>,
    group: Option<&str>,
) -> Result<(&'static str, String)> {
    match (status, group) {
        (Some(status), None) => Ok(("status", status.to_string())),
        (None, Some(group)) => Ok(("group", group.to_string())),
        _ => Err(BeadsError::validation(
            "capacity",
            "pass exactly one of --status <name> or --group <name> to identify the capacity",
        )),
    }
}

fn parse_expires(expires: Option<&str>) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    expires
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| parse_flexible_timestamp(value, "expires"))
        .transpose()
}

/// Shared preamble for the mutating subcommands: lock the workspace, open
/// storage (which injects the validated workflow policy), and resolve the
/// issue ID plus acting identity.
struct MutationSetup {
    storage_ctx: config::OpenStorageResult,
    issue_id: String,
    actor: String,
}

fn mutation_setup(
    raw_id: &str,
    cli: &config::CliOverrides,
    beads_dir: &Path,
) -> Result<(MutationSetup, RoutedWorkspaceWriteLock)> {
    let lock = acquire_routed_workspace_write_lock(beads_dir, false, cli.lock_timeout)?;
    let mut storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;
    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);
    let resolver = build_resolver(&config_layer);
    let issue_id = resolve_issue_id(&storage_ctx.storage, &resolver, raw_id)?;
    Ok((
        MutationSetup {
            storage_ctx,
            issue_id,
            actor,
        },
        lock,
    ))
}

fn emit_exemption_record(
    ctx: &OutputContext,
    robot: bool,
    record: &CapacityExemptionRecord,
    action: &str,
) {
    if ctx.is_toon() {
        ctx.toon(record);
    } else if robot || ctx.is_json() {
        ctx.json_pretty(record);
    } else {
        let expiry = record
            .expires_at
            .as_deref()
            .map_or_else(|| "no expiry".to_string(), |raw| format!("expires {raw}"));
        ctx.success(&format!(
            "{action} capacity exemption for {} on {} '{}' (provider {}, {expiry})",
            sanitize_terminal_inline(&record.issue_id),
            sanitize_terminal_inline(&record.capacity_kind),
            sanitize_terminal_inline(&record.capacity_name),
            sanitize_terminal_inline(&record.provider),
        ));
    }
}

fn execute_exempt(
    args: &CapacityExemptArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
) -> Result<()> {
    let (kind, name) = resolve_capacity_flags(args.status.as_deref(), args.group.as_deref())?;
    let expires_at = parse_expires(args.expires.as_deref())?;
    let (setup, _lock) = mutation_setup(&args.id, cli, beads_dir)?;
    let record = setup.storage_ctx.storage.grant_capacity_exemption(
        &setup.issue_id,
        kind,
        &name,
        &args.provider,
        &args.reason,
        expires_at,
        &setup.actor,
    )?;
    crate::util::set_last_touched_id(beads_dir, &setup.issue_id);
    emit_exemption_record(ctx, args.robot, &record, "Granted");
    Ok(())
}

fn execute_renew(
    args: &CapacityRenewArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
) -> Result<()> {
    let (kind, name) = resolve_capacity_flags(args.status.as_deref(), args.group.as_deref())?;
    let expires_at = parse_expires(args.expires.as_deref())?;
    let (setup, _lock) = mutation_setup(&args.id, cli, beads_dir)?;
    let record = setup.storage_ctx.storage.renew_capacity_exemption(
        &setup.issue_id,
        kind,
        &name,
        &args.provider,
        args.reason.as_deref(),
        expires_at,
        &setup.actor,
    )?;
    crate::util::set_last_touched_id(beads_dir, &setup.issue_id);
    emit_exemption_record(ctx, args.robot, &record, "Renewed");
    Ok(())
}

fn execute_revoke(
    args: &CapacityRevokeArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
) -> Result<()> {
    let (kind, name) = resolve_capacity_flags(args.status.as_deref(), args.group.as_deref())?;
    let (setup, _lock) = mutation_setup(&args.id, cli, beads_dir)?;
    let record = setup.storage_ctx.storage.revoke_capacity_exemption(
        &setup.issue_id,
        kind,
        &name,
        &args.provider,
        args.reason.as_deref(),
        &setup.actor,
    )?;
    crate::util::set_last_touched_id(beads_dir, &setup.issue_id);
    emit_exemption_record(ctx, args.robot, &record, "Revoked");
    Ok(())
}

fn execute_exemptions(
    args: &CapacityExemptionsArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
) -> Result<()> {
    let storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    let issue_id = args
        .id
        .as_deref()
        .map(|raw| {
            let config_layer = storage_ctx.load_config(cli)?;
            let resolver = build_resolver(&config_layer);
            resolve_issue_id(&storage_ctx.storage, &resolver, raw)
        })
        .transpose()?;

    let exemptions = storage_ctx
        .storage
        .list_capacity_exemptions(issue_id.as_deref())?;
    let history = if args.history {
        Some(
            storage_ctx
                .storage
                .get_capacity_exemption_history(issue_id.as_deref())?,
        )
    } else {
        None
    };

    let output = CapacityExemptionsOutput {
        issue_id,
        exemptions,
        history,
    };

    if ctx.is_toon() {
        ctx.toon(&output);
    } else if args.robot || ctx.is_json() {
        ctx.json_pretty(&output);
    } else {
        print_exemptions_human(ctx, &output);
    }
    Ok(())
}

fn print_exemptions_human(ctx: &OutputContext, output: &CapacityExemptionsOutput) {
    if output.exemptions.is_empty() {
        let scope = output.issue_id.as_deref().map_or_else(
            || "this workspace".to_string(),
            |id| sanitize_terminal_inline(id).to_string(),
        );
        ctx.info(&format!("No capacity exemptions recorded for {scope}."));
    } else {
        ctx.print_line("Capacity exemptions:");
        for record in &output.exemptions {
            let expiry = record
                .expires_at
                .as_deref()
                .map_or_else(|| "no expiry".to_string(), |raw| format!("expires {raw}"));
            let mut line = format!(
                "  {} on {} '{}': {} (provider {}, {expiry})",
                sanitize_terminal_inline(&record.issue_id),
                sanitize_terminal_inline(&record.capacity_kind),
                sanitize_terminal_inline(&record.capacity_name),
                sanitize_terminal_inline(&record.state),
                sanitize_terminal_inline(&record.provider),
            );
            if !record.reason.is_empty() {
                line.push_str(&format!(" — {}", sanitize_terminal_inline(&record.reason)));
            }
            ctx.print_line(&line);
        }
    }

    if let Some(history) = &output.history {
        ctx.newline();
        if history.is_empty() {
            ctx.info("No exemption history recorded.");
        } else {
            ctx.print_line("Exemption history (append-only):");
            for entry in history {
                let mut line = format!(
                    "  {} {} on {} '{}' by {} (provider {})",
                    sanitize_terminal_inline(&entry.recorded_at),
                    sanitize_terminal_inline(&entry.action),
                    sanitize_terminal_inline(&entry.capacity_kind),
                    sanitize_terminal_inline(&entry.capacity_name),
                    sanitize_terminal_inline(&entry.actor),
                    sanitize_terminal_inline(&entry.provider),
                );
                if let Some(reason) = &entry.reason {
                    line.push_str(&format!(" — {}", sanitize_terminal_inline(reason)));
                }
                ctx.print_line(&line);
            }
        }
    }
}

fn build_resolver(config_layer: &config::ConfigLayer) -> IdResolver {
    let id_config = config::id_config_from_layer(config_layer);
    IdResolver::new(ResolverConfig::with_prefix(id_config.prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::close_policy;
    use crate::config::CliOverrides;
    use crate::model::{Issue, IssueType, Priority, Status};
    use std::fs;
    use tempfile::TempDir;

    fn open_storage(beads_dir: &Path) -> config::OpenStorageResult {
        config::open_storage_with_cli(beads_dir, &CliOverrides::default()).expect("storage")
    }

    fn write_exemption_policy(beads_dir: &Path) {
        fs::write(
            beads_dir.join(close_policy::POLICY_FILE_NAME),
            r"workflow:
  statuses: [open, in_progress, closed]
  capacity:
    statuses:
      in_progress:
        hard: 1
    exemptions:
      providers:
        - operator
",
        )
        .expect("write exemption policy");
    }

    fn make_issue(id: &str, status: Status) -> Issue {
        let now = chrono::Utc::now();
        Issue {
            id: id.to_string(),
            title: format!("issue {id}"),
            status,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: now,
            updated_at: now,
            ..Issue::default()
        }
    }

    fn setup_workspace() -> (TempDir, std::path::PathBuf) {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        write_exemption_policy(&beads_dir);
        {
            let mut ctx = open_storage(&beads_dir);
            ctx.storage
                .create_issue(&make_issue("bd-1", Status::Open), "tester")
                .unwrap();
        }
        (temp, beads_dir)
    }

    #[test]
    fn exempt_grants_and_exemptions_lists_it_back() {
        let (_temp, beads_dir) = setup_workspace();
        let ctx = OutputContext::from_flags(true, false, true);
        let grant = CapacityExemptArgs {
            id: "bd-1".to_string(),
            status: Some("in_progress".to_string()),
            group: None,
            provider: "operator".to_string(),
            reason: "long-lived external blocker".to_string(),
            expires: Some("+7d".to_string()),
            robot: true,
        };
        execute_exempt(&grant, &CliOverrides::default(), &ctx, &beads_dir).unwrap();

        let storage_ctx = open_storage(&beads_dir);
        let records = storage_ctx
            .storage
            .list_capacity_exemptions(Some("bd-1"))
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].capacity_kind, "status");
        assert_eq!(records[0].capacity_name, "in_progress");
        assert_eq!(records[0].state, "active");
        assert!(records[0].expires_at.is_some());
        let history = storage_ctx
            .storage
            .get_capacity_exemption_history(Some("bd-1"))
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].action, "grant");
    }

    #[test]
    fn exempt_rejects_unauthorized_provider_and_missing_capacity_flag() {
        let (_temp, beads_dir) = setup_workspace();
        let ctx = OutputContext::from_flags(true, false, true);
        let unauthorized = CapacityExemptArgs {
            id: "bd-1".to_string(),
            status: Some("in_progress".to_string()),
            group: None,
            provider: "intruder".to_string(),
            reason: "should fail".to_string(),
            expires: None,
            robot: true,
        };
        let error = execute_exempt(&unauthorized, &CliOverrides::default(), &ctx, &beads_dir)
            .expect_err("unauthorized provider must be rejected");
        assert!(error.to_string().contains("not authorized"));

        let no_capacity = CapacityExemptArgs {
            id: "bd-1".to_string(),
            status: None,
            group: None,
            provider: "operator".to_string(),
            reason: "should fail".to_string(),
            expires: None,
            robot: true,
        };
        let error = execute_exempt(&no_capacity, &CliOverrides::default(), &ctx, &beads_dir)
            .expect_err("a capacity flag is required");
        assert!(error.to_string().contains("--status"));
    }

    #[test]
    fn revoke_ends_exemption_and_appends_history() {
        let (_temp, beads_dir) = setup_workspace();
        let ctx = OutputContext::from_flags(true, false, true);
        let grant = CapacityExemptArgs {
            id: "bd-1".to_string(),
            status: Some("in_progress".to_string()),
            group: None,
            provider: "operator".to_string(),
            reason: "temporary".to_string(),
            expires: None,
            robot: true,
        };
        execute_exempt(&grant, &CliOverrides::default(), &ctx, &beads_dir).unwrap();

        let revoke = CapacityRevokeArgs {
            id: "bd-1".to_string(),
            status: Some("in_progress".to_string()),
            group: None,
            provider: "operator".to_string(),
            reason: Some("no longer needed".to_string()),
            robot: true,
        };
        execute_revoke(&revoke, &CliOverrides::default(), &ctx, &beads_dir).unwrap();

        let storage_ctx = open_storage(&beads_dir);
        let records = storage_ctx
            .storage
            .list_capacity_exemptions(Some("bd-1"))
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, "revoked");
        let history = storage_ctx
            .storage
            .get_capacity_exemption_history(Some("bd-1"))
            .unwrap();
        assert_eq!(history.len(), 2, "grant + revoke, append-only");
        assert_eq!(history[1].action, "revoke");
        // Release the held workspace/database authority before invoking the
        // next command: since the retained write-authority model landed, a
        // caller-held OpenStorageResult blocks a nested same-process open
        // (real CLI runs are one command per process, so sequential commands
        // never contend like this).
        drop(storage_ctx);

        let renew = CapacityRenewArgs {
            id: "bd-1".to_string(),
            status: Some("in_progress".to_string()),
            group: None,
            provider: "operator".to_string(),
            expires: Some("+1d".to_string()),
            reason: None,
            robot: true,
        };
        let error = execute_renew(&renew, &CliOverrides::default(), &ctx, &beads_dir)
            .expect_err("a revoked exemption cannot be renewed");
        assert!(error.to_string().contains("revoked"), "{error}");
    }
}
