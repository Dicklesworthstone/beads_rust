//! Workflow gate engine commands (`br gate`), issue #312 layer 2.
//!
//! `br gate report <id> --gate <name> --provider <name> --status pass|fail`
//! records a gate result; `br gate list <id>` shows recorded results and, when
//! the project configures `workflow.gates`, the computed required-gate status
//! for each guarded transition out of the issue's current status.
//!
//! Gate history is portable through the `issues.history.jsonl` companion;
//! ordinary issue JSONL does not contain it. Reporting and transition checks
//! use the shared storage policy. Actor/provider names are declared identities,
//! not authenticated principals.

use super::{
    acquire_routed_workspace_write_lock, auto_import_storage_ctx_if_stale, resolve_issue_id,
};
use crate::cli::{GateCommands, GateListArgs, GateReportArgs, GateStatus};
use crate::close_policy::{self, GateOverride, GateResult, GateResultRecord, Workflow};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::output::OutputContext;
use crate::util::id::{IdResolver, ResolverConfig};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// JSON payload for `br gate report`.
#[derive(Debug, Serialize)]
struct GateReportOutput {
    id: i64,
    issue_id: String,
    from_status: String,
    to_status: String,
    status_revision: i64,
    gate: String,
    provider: String,
    passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    recorded_by: Option<String>,
    recorded_at: String,
    identity_kind: &'static str,
    assignee_at_report: Option<String>,
    override_reason: Option<String>,
}

/// Computed status of one required gate for a guarded transition.
#[derive(Debug, Serialize)]
struct RequiredGateStatus {
    gate: String,
    satisfied: bool,
}

/// One guarded transition out of the issue's current status, with the
/// computed satisfaction of each gate it requires.
#[derive(Debug, Serialize)]
struct GatedTransitionStatus {
    from: String,
    to: String,
    gates: Vec<RequiredGateStatus>,
    /// True when every required gate for this transition is satisfied.
    satisfied: bool,
}

/// JSON payload for `br gate list`.
#[derive(Debug, Serialize)]
struct GateListOutput {
    issue_id: String,
    current_status: Option<String>,
    identity_kind: &'static str,
    history: Vec<GateResultRecord>,
    /// Pre-v15 unscoped verdicts retained for audit display only. They never
    /// satisfy a transition.
    legacy_results: Vec<GateResult>,
    /// Gated transitions out of `current_status` and their satisfaction. Empty
    /// when the project has no `workflow.gates` config or the current status
    /// could not be resolved.
    gated_transitions: Vec<GatedTransitionStatus>,
}

/// Execute the gate command.
///
/// # Errors
///
/// Returns an error if database operations fail or the issue cannot be
/// resolved.
pub fn execute(
    command: &GateCommands,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    match command {
        GateCommands::Report(args) => execute_report(args, cli, ctx, &beads_dir),
        GateCommands::List(args) => execute_list(args, cli, ctx, &beads_dir),
    }
}

fn execute_report(
    args: &GateReportArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
) -> Result<()> {
    let gate = args.gate.trim();
    let provider = args.provider.trim();
    if gate.is_empty() {
        return Err(BeadsError::validation("gate", "--gate must not be empty"));
    }
    if provider.is_empty() {
        return Err(BeadsError::validation(
            "provider",
            "--provider must not be empty",
        ));
    }
    let gate_override = failure_override(args)?;

    // Gate results are portable audit evidence; take the workspace write lock
    // so a concurrent flush/import doesn't race the write, mirroring the other
    // mutating commands.
    let _lock = acquire_routed_workspace_write_lock(beads_dir, false, cli.lock_timeout)?;
    let mut storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, cli)?;

    let config_layer = storage_ctx.load_config(cli)?;
    let actor = config::resolve_actor(&config_layer);
    let resolver = build_resolver(&config_layer);
    let issue_id = resolve_issue_id(&storage_ctx.storage, &resolver, &args.id)?;

    let issue =
        storage_ctx
            .storage
            .get_issue(&issue_id)?
            .ok_or_else(|| BeadsError::IssueNotFound {
                id: issue_id.clone(),
            })?;
    let policy = close_policy::load_for_beads_dir(beads_dir)?;
    storage_ctx
        .storage
        .set_workflow_policy(policy.workflow.clone());
    let labels = storage_ctx.storage.get_labels(&issue_id)?;
    let from_status = issue.status.as_str();
    let status_revision = storage_ctx.storage.status_revision(&issue_id)?;
    let to_status = resolve_gate_target(
        &policy.workflow,
        from_status,
        args.to.as_deref(),
        gate,
        &labels,
        issue.priority.0,
    )?;

    let passed = matches!(args.status, GateStatus::Pass);
    let note = args
        .note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let record = storage_ctx.storage.record_scoped_gate_result(
        &issue_id,
        from_status,
        status_revision,
        &to_status,
        gate,
        provider,
        passed,
        note,
        &actor,
        gate_override.as_ref(),
    )?;

    crate::util::set_last_touched_id(beads_dir, &issue_id);
    // The verdict transaction already marked the portable audit as dirty.
    storage_ctx.flush_no_db_if_dirty()?;
    storage_ctx.auto_flush_if_enabled()?;

    let output = GateReportOutput {
        id: record.id,
        issue_id: issue_id.clone(),
        from_status: record.from_status,
        to_status: record.to_status,
        status_revision: record.status_revision,
        gate: gate.to_string(),
        provider: provider.to_string(),
        passed,
        note: note.map(str::to_string),
        recorded_by: record.recorded_by,
        recorded_at: record.recorded_at,
        identity_kind: "declared",
        assignee_at_report: record.assignee_at_report,
        override_reason: record.override_reason,
    };

    if ctx.is_toon() {
        ctx.toon(&output);
    } else if args.robot || ctx.is_json() {
        ctx.json_pretty(&output);
    } else {
        let verdict = if passed { "pass" } else { "fail" };
        ctx.success(&format!(
            "Recorded gate '{}' = {} (provider {}, declared actor {}) for {} [{} -> {}, revision {}, history #{}]",
            sanitize_terminal_inline(gate),
            verdict,
            sanitize_terminal_inline(provider),
            sanitize_terminal_inline(&actor),
            sanitize_terminal_inline(&issue_id),
            sanitize_terminal_inline(&output.from_status),
            sanitize_terminal_inline(&output.to_status),
            output.status_revision,
            output.id,
        ));
        if let Some(reason) = &output.override_reason {
            ctx.info(&format!(
                "Failure override: {}",
                sanitize_terminal_inline(reason)
            ));
        }
    }
    Ok(())
}

fn failure_override(args: &GateReportArgs) -> Result<Option<GateOverride>> {
    match (args.override_failure, args.override_reason.as_deref()) {
        (None, None) => Ok(None),
        (Some(expected_history_id), Some(reason)) => {
            if args.status != GateStatus::Pass {
                return Err(BeadsError::validation(
                    "override_failure",
                    "--override-failure requires --status pass",
                ));
            }
            if expected_history_id <= 0 {
                return Err(BeadsError::validation(
                    "override_failure",
                    "--override-failure must name a positive history ID",
                ));
            }
            let reason = reason.trim();
            if reason.is_empty() {
                return Err(BeadsError::validation(
                    "override_reason",
                    "--override-reason must not be empty",
                ));
            }
            Ok(Some(GateOverride {
                expected_history_id,
                reason: reason.to_owned(),
            }))
        }
        _ => Err(BeadsError::validation(
            "override_failure",
            "--override-failure and --override-reason require each other",
        )),
    }
}

fn resolve_gate_target(
    workflow: &Workflow,
    from_status: &str,
    requested_to: Option<&str>,
    gate: &str,
    labels: &[String],
    priority: i32,
) -> Result<String> {
    let gate_is_required = |to: &str| {
        workflow
            .required_gates_for(from_status, to, labels, priority)
            .iter()
            .any(|spec| spec.id().eq_ignore_ascii_case(gate))
    };

    if let Some(requested) = requested_to {
        let requested = requested.trim();
        if requested.is_empty() {
            return Err(BeadsError::validation("to", "--to must not be empty"));
        }
        if !gate_is_required(requested) {
            return Err(BeadsError::validation(
                "to",
                format!(
                    "workflow does not require gate '{gate}' for transition '{from_status} -> {requested}'"
                ),
            ));
        }
        return Ok(requested.to_string());
    }

    let mut targets = BTreeMap::new();
    for key in workflow.gates.keys() {
        let Some((rule_from, rule_to)) = key.split_once("->") else {
            continue;
        };
        let rule_from = rule_from.trim();
        let rule_to = rule_to.trim();
        if rule_from.eq_ignore_ascii_case(from_status) && gate_is_required(rule_to) {
            targets
                .entry(rule_to.to_ascii_lowercase())
                .or_insert_with(|| rule_to.to_string());
        }
    }

    match targets.len() {
        1 => Ok(targets.into_values().next().unwrap_or_default()),
        0 => Err(BeadsError::validation(
            "to",
            format!(
                "no configured transition from '{from_status}' requires gate '{gate}'; add a workflow.gates rule or pass a gate name required by the current workflow"
            ),
        )),
        _ => Err(BeadsError::validation(
            "to",
            format!(
                "gate '{gate}' is required by multiple transitions from '{from_status}' ({}); pass --to <status> to select the intended target",
                targets.into_values().collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}

fn execute_list(
    args: &GateListArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
) -> Result<()> {
    let storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    let config_layer = storage_ctx.load_config(cli)?;
    let resolver = build_resolver(&config_layer);
    let issue_id = resolve_issue_id(&storage_ctx.storage, &resolver, &args.id)?;

    let issue = storage_ctx.storage.get_issue(&issue_id)?;
    let current_status = issue.as_ref().map(|i| i.status.as_str().to_string());
    let history = storage_ctx.storage.get_gate_result_history(&issue_id)?;
    // GitHub #466: `gate_results` now mirrors every scoped report's latest
    // (gate, provider) verdict. Rows whose (gate, provider) pair already
    // appears in the scoped history are that mirror, not legacy data — keep
    // only genuinely legacy rows (written by pre-#388 versions with no
    // matching history entry) in the audit-only section.
    let legacy_results: Vec<_> = storage_ctx
        .storage
        .get_legacy_gate_results(&issue_id)?
        .into_iter()
        .filter(|legacy| {
            !history
                .iter()
                .any(|h| h.gate == legacy.gate && h.provider == legacy.provider)
        })
        .collect();

    // Compute required-gate status for each guarded transition out of the
    // current status, using the project's workflow.gates config. Absent
    // config / status leaves this empty (backward compatible).
    let policy = close_policy::load_for_beads_dir(beads_dir)?;
    let labels = storage_ctx.storage.get_labels(&issue_id)?;
    let priority = issue.as_ref().map_or(0, |i| i.priority.0);
    let mut results_by_target = BTreeMap::new();
    if let Some(from) = current_status.as_deref() {
        for key in policy.workflow.gates.keys() {
            let Some((rule_from, rule_to)) = key.split_once("->") else {
                continue;
            };
            if rule_from.trim().eq_ignore_ascii_case(from) {
                results_by_target.insert(
                    rule_to.trim().to_ascii_lowercase(),
                    storage_ctx.storage.get_scoped_gate_result_history(
                        &issue_id,
                        from,
                        rule_to.trim(),
                    )?,
                );
            }
        }
    }
    let assignees = issue
        .as_ref()
        .and_then(|issue| issue.assignee.as_deref())
        .into_iter()
        .collect::<Vec<_>>();
    let gated_transitions = compute_gated_transitions(
        &policy.workflow,
        &issue_id,
        current_status.as_deref(),
        &labels,
        priority,
        &results_by_target,
        &assignees,
    );

    let output = GateListOutput {
        issue_id: issue_id.clone(),
        current_status,
        identity_kind: "declared",
        history,
        legacy_results,
        gated_transitions,
    };

    if ctx.is_toon() {
        ctx.toon(&output);
    } else if args.robot || ctx.is_json() {
        ctx.json_pretty(&output);
    } else {
        print_gate_list_human(ctx, &output);
    }
    Ok(())
}

/// Compute, for each `workflow.gates` rule whose `from` matches the issue's
/// current status, the satisfaction of every gate that rule requires for this
/// issue. Returns an empty vec when gates are not configured or the current
/// status is unknown.
fn compute_gated_transitions(
    workflow: &Workflow,
    issue_id: &str,
    current_status: Option<&str>,
    labels: &[String],
    priority: i32,
    results_by_target: &BTreeMap<String, Vec<GateResultRecord>>,
    assignees: &[&str],
) -> Vec<GatedTransitionStatus> {
    let Some(from) = current_status else {
        return Vec::new();
    };
    if workflow.gates.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    // Iterate the configured rules; only surface those whose `from` matches the
    // issue's current status (the transitions the issue could attempt next).
    for key in workflow.gates.keys() {
        let Some((rule_from, rule_to)) = key.split_once("->") else {
            continue;
        };
        let rule_from = rule_from.trim();
        let rule_to = rule_to.trim();
        if !rule_from.eq_ignore_ascii_case(from) {
            continue;
        }
        let results = results_by_target
            .get(&rule_to.to_ascii_lowercase())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let violations = close_policy::evaluate_scoped_gates(
            workflow, issue_id, from, rule_to, labels, priority, results, assignees,
        );
        let required = workflow.required_gates_for(from, rule_to, labels, priority);
        let gates: Vec<RequiredGateStatus> = required
            .iter()
            .map(|spec| {
                let id = spec.id();
                let satisfied = !violations.iter().any(|v| v.gate == format!("gate_{id}"));
                RequiredGateStatus {
                    gate: id.to_string(),
                    satisfied,
                }
            })
            .collect();
        out.push(GatedTransitionStatus {
            from: from.to_string(),
            to: rule_to.to_string(),
            satisfied: violations.is_empty(),
            gates,
        });
    }
    out
}

fn print_gate_list_human(ctx: &OutputContext, output: &GateListOutput) {
    let id = sanitize_terminal_inline(&output.issue_id);
    if output.history.is_empty() && output.legacy_results.is_empty() {
        ctx.info(&format!("No gate results recorded for {id}."));
    } else {
        ctx.print_line(&format!("Scoped gate history for {id}:"));
        for result in &output.history {
            let verdict = if result.passed { "pass" } else { "fail" };
            let gate = sanitize_terminal_inline(&result.gate);
            let provider = sanitize_terminal_inline(&result.provider);
            let mut line = format!(
                "  #{} {gate} [{provider}]: {verdict} ({} -> {}, revision {}, declared actor {})",
                result.id,
                sanitize_terminal_inline(&result.from_status),
                sanitize_terminal_inline(&result.to_status),
                result.status_revision,
                sanitize_terminal_inline(result.recorded_by.as_deref().unwrap_or("unknown")),
            );
            if let Some(note) = &result.note {
                line.push_str(&format!(" — {}", sanitize_terminal_inline(note)));
            }
            if let Some(reason) = &result.override_reason {
                line.push_str(&format!(
                    " [failure override: {}]",
                    sanitize_terminal_inline(reason)
                ));
            }
            ctx.print_line(&line);
        }
        if !output.legacy_results.is_empty() {
            ctx.warning(
                "Legacy unscoped gate results follow; they are audit-only and cannot satisfy a transition.",
            );
            for result in &output.legacy_results {
                let verdict = if result.passed { "pass" } else { "fail" };
                ctx.print_line(&format!(
                    "  {} [{}]: {} (legacy, unscoped)",
                    sanitize_terminal_inline(&result.gate),
                    sanitize_terminal_inline(&result.provider),
                    verdict,
                ));
            }
        }
    }

    if !output.gated_transitions.is_empty() {
        ctx.newline();
        ctx.print_line("Required gates for next transitions:");
        for transition in &output.gated_transitions {
            let marker = if transition.satisfied {
                "OK"
            } else {
                "BLOCKED"
            };
            ctx.print_line(&format!(
                "  {} -> {} [{}]",
                sanitize_terminal_inline(&transition.from),
                sanitize_terminal_inline(&transition.to),
                marker,
            ));
            for gate in &transition.gates {
                let status = if gate.satisfied {
                    "satisfied"
                } else {
                    "missing"
                };
                ctx.print_line(&format!(
                    "    {}: {}",
                    sanitize_terminal_inline(&gate.gate),
                    status
                ));
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
    use crate::close_policy::{ConditionalGate, GateReporterPolicy, GateRule, GateSpec};
    use crate::config::CliOverrides;
    use crate::model::{Issue, IssueType, Priority, Status};
    use std::fs;
    use tempfile::TempDir;

    fn open_storage(beads_dir: &Path) -> config::OpenStorageResult {
        config::open_storage_with_cli(beads_dir, &CliOverrides::default()).expect("storage")
    }

    fn write_gate_policy(beads_dir: &Path) {
        fs::write(
            beads_dir.join(close_policy::POLICY_FILE_NAME),
            r#"workflow:
  strict: true
  gates:
    "open -> closed":
      require_all:
        - ci_green
"#,
        )
        .expect("write gate policy");
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

    fn make_gate_record(
        issue_id: &str,
        id: i64,
        gate: &str,
        provider: &str,
        actor: &str,
        passed: bool,
    ) -> GateResultRecord {
        GateResultRecord {
            id,
            issue_id: issue_id.to_owned(),
            from_status: "in_review".to_owned(),
            to_status: "closed".to_owned(),
            status_revision: 0,
            gate: gate.to_owned(),
            provider: provider.to_owned(),
            passed,
            note: None,
            recorded_by: Some(actor.to_owned()),
            recorded_at: "2026-11-12T00:00:00Z".to_owned(),
            assignee_at_report: Some(String::new()),
            override_reason: None,
        }
    }

    #[test]
    fn report_records_result_and_list_reads_it_back() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        write_gate_policy(&beads_dir);
        {
            let mut ctx = open_storage(&beads_dir);
            ctx.storage
                .create_issue(&make_issue("bd-1", Status::Open), "tester")
                .unwrap();
        }

        let report = GateReportArgs {
            id: "bd-1".to_string(),
            gate: "ci_green".to_string(),
            provider: "ci".to_string(),
            status: GateStatus::Pass,
            to: Some("closed".to_string()),
            note: Some("build #42".to_string()),
            override_failure: None,
            override_reason: None,
            robot: true,
        };
        let ctx = OutputContext::from_flags(true, false, true);
        execute_report(&report, &CliOverrides::default(), &ctx, &beads_dir).unwrap();

        let storage_ctx = open_storage(&beads_dir);
        let results = storage_ctx
            .storage
            .get_scoped_gate_results("bd-1", "open", "closed")
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].gate, "ci_green");
        assert_eq!(results[0].provider, "ci");
        assert!(results[0].passed);
        assert_eq!(results[0].note.as_deref(), Some("build #42"));
    }

    #[test]
    fn report_overwrites_same_provider_and_gate() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        write_gate_policy(&beads_dir);
        {
            let mut ctx = open_storage(&beads_dir);
            ctx.storage
                .create_issue(&make_issue("bd-1", Status::Open), "tester")
                .unwrap();
        }
        let ctx = OutputContext::from_flags(true, false, true);
        let base = GateReportArgs {
            id: "bd-1".to_string(),
            gate: "ci_green".to_string(),
            provider: "ci".to_string(),
            status: GateStatus::Fail,
            to: Some("closed".to_string()),
            note: None,
            override_failure: None,
            override_reason: None,
            robot: true,
        };
        execute_report(&base, &CliOverrides::default(), &ctx, &beads_dir).unwrap();
        let pass = GateReportArgs {
            status: GateStatus::Pass,
            ..base.clone()
        };
        execute_report(&pass, &CliOverrides::default(), &ctx, &beads_dir).unwrap();

        let storage_ctx = open_storage(&beads_dir);
        let results = storage_ctx
            .storage
            .get_scoped_gate_results("bd-1", "open", "closed")
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "only the latest scoped verdict is effective"
        );
        assert!(results[0].passed);
        let history = storage_ctx.storage.get_gate_result_history("bd-1").unwrap();
        assert_eq!(history.len(), 2, "gate history must remain append-only");
        assert!(!history[0].passed);
        assert!(history[1].passed);
    }

    #[test]
    fn report_rejects_empty_gate_and_provider() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let ctx = OutputContext::from_flags(true, false, true);
        let empty_gate = GateReportArgs {
            id: "bd-1".to_string(),
            gate: "   ".to_string(),
            provider: "ci".to_string(),
            status: GateStatus::Pass,
            to: None,
            note: None,
            override_failure: None,
            override_reason: None,
            robot: true,
        };
        assert!(execute_report(&empty_gate, &CliOverrides::default(), &ctx, &beads_dir).is_err());
    }

    #[test]
    fn report_rejects_malformed_overrides_before_opening_storage() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let ctx = OutputContext::from_flags(true, false, true);
        let base = GateReportArgs {
            id: "bd-1".to_owned(),
            gate: "ci_green".to_owned(),
            provider: "verifier".to_owned(),
            status: GateStatus::Pass,
            to: Some("closed".to_owned()),
            note: None,
            override_failure: Some(1),
            override_reason: Some("reviewed false negative".to_owned()),
            robot: true,
        };
        for invalid in [
            GateReportArgs {
                status: GateStatus::Fail,
                ..base.clone()
            },
            GateReportArgs {
                override_failure: Some(0),
                ..base.clone()
            },
            GateReportArgs {
                override_reason: Some("  ".to_owned()),
                ..base.clone()
            },
            GateReportArgs {
                override_failure: None,
                ..base.clone()
            },
            GateReportArgs {
                override_reason: None,
                ..base.clone()
            },
        ] {
            assert!(execute_report(&invalid, &CliOverrides::default(), &ctx, &beads_dir).is_err());
            assert!(
                !beads_dir.exists(),
                "invalid override must fail before storage opens"
            );
        }
        let parsed = failure_override(&base).unwrap().unwrap();
        assert_eq!(parsed.expected_history_id, 1);
        assert_eq!(parsed.reason, "reviewed false negative");
    }

    #[test]
    fn report_target_must_disambiguate_multiple_guarded_transitions() {
        let rule = GateRule {
            require_all: vec![GateSpec::Named("ci_green".to_string())],
            ..Default::default()
        };
        let workflow = Workflow {
            strict: true,
            gates: BTreeMap::from([
                ("open -> in_review".to_string(), rule.clone()),
                ("open -> closed".to_string(), rule),
            ]),
            ..Default::default()
        };

        let error = resolve_gate_target(&workflow, "open", None, "ci_green", &[], 2)
            .expect_err("ambiguous gate target must be rejected");
        assert!(error.to_string().contains("pass --to <status>"));

        let target = resolve_gate_target(&workflow, "OPEN", Some("CLOSED"), "CI_GREEN", &[], 2)
            .expect("an explicit target removes the ambiguity");
        assert_eq!(target, "CLOSED");
    }

    #[test]
    fn list_computes_required_gate_status() {
        let mut gates = std::collections::BTreeMap::new();
        gates.insert(
            "in_review -> closed".to_string(),
            GateRule {
                require_all: vec![
                    GateSpec::Named("ci_green".to_string()),
                    GateSpec::MinReviewers(1),
                ],
                require_if: vec![ConditionalGate {
                    label: Some("security-sensitive".to_string()),
                    gate: GateSpec::Named("security_sign_off".to_string()),
                    ..Default::default()
                }],
            },
        );
        let workflow = Workflow {
            strict: true,
            gates,
            ..Default::default()
        };

        // No results yet: ci_green + min_reviewers required, security not (no label).
        let no_results = BTreeMap::new();
        let transitions = compute_gated_transitions(
            &workflow,
            "bd-1",
            Some("in_review"),
            &[],
            2,
            &no_results,
            &[],
        );
        assert_eq!(transitions.len(), 1);
        let t = &transitions[0];
        assert_eq!(t.to, "closed");
        assert!(!t.satisfied);
        let gate_ids: Vec<&str> = t.gates.iter().map(|g| g.gate.as_str()).collect();
        assert!(gate_ids.contains(&"ci_green"));
        assert!(gate_ids.contains(&"min_reviewers"));
        assert!(!gate_ids.contains(&"security_sign_off"));

        // With ci pass + one reviewer pass, both satisfied.
        let results = vec![
            make_gate_record("bd-1", 1, "ci_green", "ci", "ci", true),
            make_gate_record("bd-1", 2, "min_reviewers", "reviewer:alice", "alice", true),
        ];
        let results_by_target = BTreeMap::from([("closed".to_string(), results)]);
        let transitions = compute_gated_transitions(
            &workflow,
            "bd-1",
            Some("in_review"),
            &[],
            2,
            &results_by_target,
            &[],
        );
        assert!(transitions[0].satisfied);
    }

    #[test]
    fn list_min_reviewers_uses_maximum_matching_threshold() {
        let yaml = r#"
strict: true
gates:
  "in_review -> closed":
    require_all:
      - min_reviewers: 1
    require_if:
      - label: auth
        gate: {min_reviewers: 2}
      - label: security
        gate: {min_reviewers: 3}
"#;
        let labels = vec!["auth".to_string(), "security".to_string()];
        let reviews: Vec<GateResultRecord> = ["reviewer:alice", "reviewer:bob", "reviewer:carol"]
            .into_iter()
            .zip(1_i64..)
            .map(|(provider, id)| {
                make_gate_record("bd-513", id, "min_reviewers", provider, provider, true)
            })
            .collect();
        for reverse in [false, true] {
            let mut workflow: Workflow = serde_yml::from_str(yaml).unwrap();
            if reverse {
                workflow
                    .gates
                    .get_mut("in_review -> closed")
                    .unwrap()
                    .require_if
                    .reverse();
            }
            for count in 0..=reviews.len() {
                let results_by_target =
                    BTreeMap::from([("closed".to_string(), reviews[..count].to_vec())]);
                let transitions = compute_gated_transitions(
                    &workflow,
                    "bd-513",
                    Some("in_review"),
                    &labels,
                    2,
                    &results_by_target,
                    &[],
                );
                assert_eq!(transitions.len(), 1);
                let transition = &transitions[0];
                assert_eq!(transition.from, "in_review");
                assert_eq!(transition.to, "closed");
                assert_eq!(transition.gates.len(), 1);
                assert_eq!(transition.gates[0].gate, "min_reviewers");
                assert_eq!(
                    transition.gates[0].satisfied,
                    count == 3,
                    "{count} reviewers, reversed rules={reverse}"
                );
                assert_eq!(transition.satisfied, count == 3);
            }
            let baseline_results = BTreeMap::from([("closed".to_string(), reviews[..1].to_vec())]);
            let baseline = compute_gated_transitions(
                &workflow,
                "bd-513",
                Some("in_review"),
                &[],
                2,
                &baseline_results,
                &[],
            );
            assert_eq!(baseline.len(), 1);
            assert!(baseline[0].gates[0].satisfied);
            assert!(baseline[0].satisfied);
        }
    }

    #[test]
    fn list_is_empty_without_gate_config() {
        let workflow = Workflow::default();
        let transitions = compute_gated_transitions(
            &workflow,
            "bd-1",
            Some("in_review"),
            &[],
            2,
            &BTreeMap::new(),
            &[],
        );
        assert!(transitions.is_empty());
    }

    #[test]
    fn list_protected_gate_uses_reporter_identity_and_complete_history() {
        let workflow = Workflow {
            strict: true,
            gates: BTreeMap::from([(
                "in_review -> closed".to_owned(),
                GateRule {
                    require_all: vec![GateSpec::Named("independent_validation".to_owned())],
                    ..Default::default()
                },
            )]),
            gate_policies: BTreeMap::from([(
                "independent_validation".to_owned(),
                GateReporterPolicy {
                    allowed_reporters: Some(vec![
                        "worker".to_owned(),
                        "verifier".to_owned(),
                        "lead".to_owned(),
                    ]),
                    forbid_self_approval: true,
                    override_reporters: vec!["lead".to_owned()],
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let satisfied = |records: Vec<GateResultRecord>| {
            let by_target = BTreeMap::from([("closed".to_owned(), records)]);
            let transitions = compute_gated_transitions(
                &workflow,
                "bd-602",
                Some("in_review"),
                &[],
                2,
                &by_target,
                &["worker"],
            );
            assert_eq!(transitions.len(), 1);
            assert_eq!(transitions[0].gates.len(), 1);
            assert_eq!(transitions[0].satisfied, transitions[0].gates[0].satisfied);
            transitions[0].satisfied
        };
        let self_pass = make_gate_record(
            "bd-602",
            1,
            "independent_validation",
            "worker",
            "worker",
            true,
        );
        assert!(!satisfied(vec![self_pass]));
        let pass = make_gate_record(
            "bd-602",
            2,
            "independent_validation",
            "verifier",
            "verifier",
            true,
        );
        assert!(satisfied(vec![pass.clone()]));
        let failed = make_gate_record(
            "bd-602",
            3,
            "independent_validation",
            "verifier",
            "verifier",
            false,
        );
        let repeated_pass = make_gate_record(
            "bd-602",
            4,
            "independent_validation",
            "verifier",
            "verifier",
            true,
        );
        let mut records = vec![pass, failed, repeated_pass];
        assert!(!satisfied(records.clone()));
        let mut override_pass =
            make_gate_record("bd-602", 5, "independent_validation", "lead", "lead", true);
        override_pass.override_reason = Some("Reviewed false negative".to_owned());
        records.push(override_pass);
        assert!(satisfied(records));
    }
}
