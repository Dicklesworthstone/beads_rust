//! Robot-docs command implementation.

use crate::cli::{
    OutputFormat, RobotDocsCommands, RobotDocsGuideArgs,
    resolve_output_format_basic_with_outer_mode,
};
use crate::error::Result;
use crate::output::{OutputContext, OutputMode};
use serde::Serialize;

const CONTRACT_VERSION: &str = "br.robot_docs.v1";

const GUIDE: &str = r#"br Agent Guide

Purpose:
  br is a local-first issue tracker. It stores primary state in SQLite and
  exports .beads/issues.jsonl for git-friendly handoff. Normal issue and sync
  paths never run git. Only an explicit br vcs-status request runs bounded,
  read-only Git probes.

Machine-output defaults:
  Use --json or --format json for scripts. Diagnostics and structured errors
  go to stderr. For token-efficient structured output, use --format toon where
  the command supports it.

Start of session:
  br capabilities --format json
  br ready --json
  br coordination status --json
  br show <id> --json

Finding work:
  br ready --json is the single work-discovery entrypoint: it returns
  unblocked, non-deferred, actionable issues. "Ready" defaults to status=open,
  but projects can widen it via workflow.status_groups.ready in
  .beads/policy.yaml (e.g. [open, rework]) so review-returned work resurfaces
  without changing the command. Don't hand-roll status filters like
  `br list -s open -s rework`; call `br ready --json` and let project policy
  define readiness. Returned issues keep their real status (a rework item still
  reports {"status":"rework"}).

Claiming work:
  br update <id> --claim --actor "$AGENT_NAME" --json
  If Agent Mail is down, add a comment naming the intended file scope before
  editing. Treat that comment as advisory, not a lock.

Completing work:
  br close <id> --reason "Completed: <specific proof>" --json
  br sync --flush-only
  Stage code and .beads changes together outside br.

Discovery:
  br schema commands --format json
  br schema all --format json
  br vcs-status --json
  br robot-docs guide

Safety:
  Avoid bare bv in automated sessions; use bv --robot-* flags.
  Use RUST_LOG=error for routine br runs to suppress dependency logs.
  br sync does not commit, push, pull, or install hooks.
  Existing databases are never schema-migrated implicitly. Run
  br doctor migrate-schema plan --json, review the receipt, then apply its
  exact token.
"#;

#[derive(Debug, Serialize)]
struct RobotGuideOutput {
    tool: &'static str,
    version: &'static str,
    contract_version: &'static str,
    title: &'static str,
    line_count: usize,
    guide: &'static str,
    canonical_commands: &'static [CanonicalCommand],
}

#[derive(Debug, Serialize)]
struct CanonicalCommand {
    task: &'static str,
    command: &'static str,
}

const CANONICAL_COMMANDS: &[CanonicalCommand] = &[
    CanonicalCommand {
        task: "discover capabilities",
        command: "br capabilities --format json",
    },
    CanonicalCommand {
        task: "find ready work",
        command: "br ready --json",
    },
    CanonicalCommand {
        task: "diagnose stale claims",
        command: "br coordination status --json",
    },
    CanonicalCommand {
        task: "show issue details",
        command: "br show <id> --json",
    },
    CanonicalCommand {
        task: "inspect JSON contracts",
        command: "br schema commands --format json",
    },
    CanonicalCommand {
        task: "explicitly inspect JSONL Git visibility",
        command: "br vcs-status --json",
    },
    CanonicalCommand {
        task: "review a required schema migration",
        command: "br doctor migrate-schema plan --json",
    },
    CanonicalCommand {
        task: "final JSONL export",
        command: "br sync --flush-only",
    },
];

/// Execute the robot-docs command.
///
/// # Errors
///
/// Returns an error if output serialization fails.
pub fn execute(command: &RobotDocsCommands, outer_ctx: &OutputContext) -> Result<()> {
    match command {
        RobotDocsCommands::Guide(args) => execute_guide(args, outer_ctx),
    }
    Ok(())
}

fn execute_guide(args: &RobotDocsGuideArgs, outer_ctx: &OutputContext) {
    let output_format = resolve_output_format_basic_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        false,
    );
    let quiet = matches!(outer_ctx.mode(), OutputMode::Quiet);
    let ctx = OutputContext::from_output_format(output_format, quiet, true);
    if ctx.is_quiet() {
        return;
    }

    let payload = RobotGuideOutput {
        tool: "br",
        version: env!("CARGO_PKG_VERSION"),
        contract_version: CONTRACT_VERSION,
        title: "br Agent Guide",
        line_count: GUIDE.lines().count(),
        guide: GUIDE,
        canonical_commands: CANONICAL_COMMANDS,
    };

    match output_format {
        OutputFormat::Json => ctx.json_pretty(&payload),
        OutputFormat::Toon => ctx.toon_with_stats(&payload, args.stats),
        OutputFormat::Text | OutputFormat::Csv => print!("{GUIDE}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{CANONICAL_COMMANDS, GUIDE};

    #[test]
    fn guide_scopes_git_authority_and_discovers_vcs_status() {
        assert!(GUIDE.contains("Only an explicit br vcs-status request"));
        assert!(
            CANONICAL_COMMANDS
                .iter()
                .any(|entry| entry.command == "br vcs-status --json")
        );
    }
}
