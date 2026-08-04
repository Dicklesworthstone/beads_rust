//! Capabilities command implementation.

use crate::cli::{
    CapabilitiesArgs, Cli, OutputFormat, resolve_output_format_basic_with_outer_mode,
};
use crate::error::{BeadsError, Result};
use crate::output::{OutputContext, OutputMode};
use clap::{Arg, Command as ClapCommand, CommandFactory};
use serde::Serialize;

const CONTRACT_VERSION: &str = "br.capabilities.v1";

#[derive(Debug, Serialize)]
struct CapabilitiesOutput {
    tool: &'static str,
    version: &'static str,
    contract_version: &'static str,
    features: &'static [FeatureCapability],
    commands: Vec<CommandCapability>,
    global_flags: &'static [FlagCapability],
    output_formats: &'static [&'static str],
    exit_codes: &'static [ExitCodeCapability],
    env_vars: &'static [EnvVarCapability],
    safety: &'static [SafetyCapability],
    recommended_entrypoints: &'static [&'static str],
    #[serde(skip_serializing_if = "Option::is_none")]
    command_detail: Option<CommandDetail>,
}

#[derive(Debug, Serialize)]
struct FeatureCapability {
    name: &'static str,
    description: &'static str,
}

#[derive(Debug, Serialize)]
struct CommandCapability {
    name: String,
    summary: String,
    operation: &'static str,
    workspace: &'static str,
    machine_output: &'static [&'static str],
    examples: &'static [&'static str],
}

#[derive(Debug, Serialize)]
struct FlagCapability {
    flag: &'static str,
    description: &'static str,
}

#[derive(Debug, Serialize)]
struct ExitCodeCapability {
    code: i32,
    category: &'static str,
    description: &'static str,
}

#[derive(Debug, Serialize)]
struct EnvVarCapability {
    name: &'static str,
    description: &'static str,
}

#[derive(Debug, Serialize)]
struct SafetyCapability {
    name: &'static str,
    guarantee: &'static str,
}

#[derive(Debug, Serialize)]
struct CommandDetail {
    path: String,
    name: String,
    summary: String,
    long_about: Option<String>,
    aliases: Vec<String>,
    operation: &'static str,
    workspace: &'static str,
    machine_output: &'static [&'static str],
    examples: &'static [&'static str],
    safety_notes: &'static [&'static str],
    arguments: Vec<ArgumentCapability>,
    subcommands: Vec<SubcommandCapability>,
}

#[derive(Debug, Serialize)]
struct ArgumentCapability {
    id: String,
    kind: &'static str,
    long: Option<String>,
    short: Option<String>,
    aliases: Vec<String>,
    help: Option<String>,
    required: bool,
    action: String,
    value_names: Vec<String>,
    default_values: Vec<String>,
    possible_values: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SubcommandCapability {
    name: String,
    summary: String,
    aliases: Vec<String>,
}

#[derive(Clone, Copy)]
struct CommandContract {
    operation: &'static str,
    workspace: &'static str,
    machine_output: &'static [&'static str],
    examples: &'static [&'static str],
}

const FEATURES: &[FeatureCapability] = &[
    FeatureCapability {
        name: "local_first_issue_tracking",
        description: "Stores issue state locally in SQLite with git-friendly JSONL export.",
    },
    FeatureCapability {
        name: "agent_machine_output",
        description: "Read-side commands expose JSON, TOON, or documented robot surfaces.",
    },
    FeatureCapability {
        name: "schema_export",
        description: "br schema emits JSON schemas and command envelope shapes.",
    },
    FeatureCapability {
        name: "coordination_diagnostics",
        description: "br coordination status diagnoses hidden or stale in-progress claims.",
    },
    FeatureCapability {
        name: "mcp_stdio_optional",
        description: "Binaries built with the mcp feature can serve a stdio MCP API.",
    },
];

const GLOBAL_FLAGS: &[FlagCapability] = &[
    FlagCapability {
        flag: "--json",
        description: "Request machine-readable JSON output when a command supports it.",
    },
    FlagCapability {
        flag: "--no-color",
        description: "Disable ANSI styling for human-readable output.",
    },
    FlagCapability {
        flag: "-q, --quiet",
        description: "Suppress non-error output.",
    },
    FlagCapability {
        flag: "--db <PATH>",
        description: "Override the discovered .beads SQLite database path.",
    },
    FlagCapability {
        flag: "--actor <NAME>",
        description: "Set the actor recorded in mutation audit events.",
    },
];

const OUTPUT_FORMATS: &[&str] = &["text", "json", "toon"];

const EXIT_CODES: &[ExitCodeCapability] = &[
    ExitCodeCapability {
        code: 0,
        category: "success",
        description: "Command completed successfully.",
    },
    ExitCodeCapability {
        code: 1,
        category: "internal",
        description: "Unexpected internal error.",
    },
    ExitCodeCapability {
        code: 2,
        category: "database",
        description: "Database initialization, schema, or storage error.",
    },
    ExitCodeCapability {
        code: 3,
        category: "issue",
        description: "Issue lookup, ambiguity, or issue-state error.",
    },
    ExitCodeCapability {
        code: 4,
        category: "validation",
        description: "Invalid command input, flag value, or required field.",
    },
    ExitCodeCapability {
        code: 5,
        category: "dependency",
        description: "Dependency graph error such as a cycle or self-dependency.",
    },
    ExitCodeCapability {
        code: 6,
        category: "sync_jsonl",
        description: "JSONL import/export, merge, or sync safety error.",
    },
    ExitCodeCapability {
        code: 7,
        category: "config",
        description: "Configuration or workspace discovery error.",
    },
    ExitCodeCapability {
        code: 8,
        category: "io",
        description: "Filesystem or terminal I/O error.",
    },
];

const ENV_VARS: &[EnvVarCapability] = &[
    EnvVarCapability {
        name: "BD_DB / BD_DATABASE",
        description: "Override the SQLite database path.",
    },
    EnvVarCapability {
        name: "BEADS_JSONL",
        description: "Override the JSONL path when explicitly allowed.",
    },
    EnvVarCapability {
        name: "BR_OUTPUT_FORMAT",
        description: "Default output format: text, json, or toon.",
    },
    EnvVarCapability {
        name: "TOON_DEFAULT_FORMAT",
        description: "Fallback default that can select TOON output.",
    },
    EnvVarCapability {
        name: "NO_COLOR",
        description: "Disable colored human-readable output.",
    },
    EnvVarCapability {
        name: "RUST_LOG",
        description: "Set logging verbosity; use RUST_LOG=error for routine agent runs.",
    },
];

const SAFETY: &[SafetyCapability] = &[
    SafetyCapability {
        name: "no_automatic_git_operations",
        guarantee: "br never commits, pushes, pulls, or installs hooks automatically.",
    },
    SafetyCapability {
        name: "sync_path_allowlist",
        guarantee: "sync writes stay inside .beads unless an external JSONL path is explicitly allowed.",
    },
    SafetyCapability {
        name: "write_lock_for_storage_mutations",
        guarantee: "mutating commands acquire the workspace write lock before storage writes.",
    },
    SafetyCapability {
        name: "structured_errors",
        guarantee: "machine-output contexts render structured error envelopes on stderr.",
    },
];

const RECOMMENDED_ENTRYPOINTS: &[&str] = &[
    "br capabilities --format json",
    "br robot-docs guide",
    "br ready --json",
    "br coordination status --json",
    "br schema commands --format json",
];

/// Execute the capabilities command.
///
/// # Errors
///
/// Returns an error if output serialization fails.
pub fn execute(args: &CapabilitiesArgs, outer_ctx: &OutputContext) -> Result<()> {
    let output_format = resolve_output_format_basic_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        false,
    );
    let quiet = matches!(outer_ctx.mode(), OutputMode::Quiet);
    let ctx = OutputContext::from_output_format(output_format, quiet, true);
    if ctx.is_quiet() {
        return Ok(());
    }

    let command_detail = args
        .command
        .as_deref()
        .map(command_detail_for_path)
        .transpose()?;

    let payload = CapabilitiesOutput {
        tool: "br",
        version: env!("CARGO_PKG_VERSION"),
        contract_version: CONTRACT_VERSION,
        features: FEATURES,
        commands: command_capabilities(),
        global_flags: GLOBAL_FLAGS,
        output_formats: OUTPUT_FORMATS,
        exit_codes: EXIT_CODES,
        env_vars: ENV_VARS,
        safety: SAFETY,
        recommended_entrypoints: RECOMMENDED_ENTRYPOINTS,
        command_detail,
    };

    match output_format {
        OutputFormat::Json => ctx.json_pretty(&payload),
        OutputFormat::Toon => ctx.toon_with_stats(&payload, args.stats),
        OutputFormat::Text | OutputFormat::Csv => render_text(&payload, args.command.as_deref()),
    }

    Ok(())
}

fn command_capabilities() -> Vec<CommandCapability> {
    Cli::command()
        .get_subcommands()
        .filter(|command| command.get_name() != "help")
        .map(|command| {
            let contract = command_contract(command.get_name());
            CommandCapability {
                name: command.get_name().to_string(),
                summary: command
                    .get_about()
                    .map(std::string::ToString::to_string)
                    .unwrap_or_default(),
                operation: contract.operation,
                workspace: contract.workspace,
                machine_output: contract.machine_output,
                examples: contract.examples,
            }
        })
        .collect()
}

fn command_detail_for_path(path: &str) -> Result<CommandDetail> {
    let segments = command_path_segments(path)?;
    let root = Cli::command();
    let (canonical_path, command) = find_command_path(&root, &segments).ok_or_else(|| {
        BeadsError::validation(
            "command",
            format!(
                "unknown command path '{path}'. Try one of: {}",
                root.get_subcommands()
                    .map(ClapCommand::get_name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    })?;

    Ok(command_detail(command, &canonical_path))
}

fn command_path_segments(path: &str) -> Result<Vec<&str>> {
    let mut segments = path.split_whitespace().collect::<Vec<_>>();
    if segments.first().is_some_and(|segment| *segment == "br") {
        segments.remove(0);
    }
    if segments.is_empty() {
        return Err(BeadsError::validation(
            "command",
            "command path cannot be empty",
        ));
    }
    Ok(segments)
}

fn find_command_path<'a>(
    root: &'a ClapCommand,
    segments: &[&str],
) -> Option<(Vec<String>, &'a ClapCommand)> {
    let mut current = root;
    let mut canonical_path = Vec::with_capacity(segments.len());

    for segment in segments {
        let next = current
            .get_subcommands()
            .find(|command| command_matches_path_segment(command, segment))?;
        canonical_path.push(next.get_name().to_string());
        current = next;
    }

    Some((canonical_path, current))
}

fn command_matches_path_segment(command: &ClapCommand, segment: &str) -> bool {
    command.get_name() == segment || command.get_all_aliases().any(|alias| alias == segment)
}

fn command_detail(command: &ClapCommand, canonical_path: &[String]) -> CommandDetail {
    let contract_path = canonical_path.join(" ");
    let contract = command_contract(&contract_path);
    let safety_notes = command_safety_notes(&contract_path);
    CommandDetail {
        path: contract_path,
        name: command.get_name().to_string(),
        summary: command
            .get_about()
            .map(std::string::ToString::to_string)
            .unwrap_or_default(),
        long_about: command
            .get_long_about()
            .map(std::string::ToString::to_string),
        aliases: command.get_visible_aliases().map(str::to_string).collect(),
        operation: contract.operation,
        workspace: contract.workspace,
        machine_output: contract.machine_output,
        examples: contract.examples,
        safety_notes,
        arguments: command
            .get_arguments()
            .filter(|argument| !argument.is_hide_set())
            .map(argument_capability)
            .collect(),
        subcommands: command
            .get_subcommands()
            .filter(|subcommand| subcommand.get_name() != "help")
            .map(subcommand_capability)
            .collect(),
    }
}

fn argument_capability(argument: &Arg) -> ArgumentCapability {
    let long = argument.get_long().map(|long| format!("--{long}"));
    let short = argument.get_short().map(|short| format!("-{short}"));
    let kind = if long.is_some() || short.is_some() {
        "option"
    } else {
        "positional"
    };

    ArgumentCapability {
        id: argument.get_id().to_string(),
        kind,
        long,
        short,
        aliases: argument
            .get_visible_aliases()
            .unwrap_or_default()
            .into_iter()
            .map(str::to_string)
            .collect(),
        help: argument.get_help().map(std::string::ToString::to_string),
        required: argument.is_required_set(),
        action: format!("{:?}", argument.get_action()),
        value_names: argument
            .get_value_names()
            .unwrap_or_default()
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        default_values: argument
            .get_default_values()
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect(),
        possible_values: argument
            .get_possible_values()
            .into_iter()
            .map(|value| value.get_name().to_string())
            .collect(),
    }
}

fn subcommand_capability(command: &ClapCommand) -> SubcommandCapability {
    SubcommandCapability {
        name: command.get_name().to_string(),
        summary: command
            .get_about()
            .map(std::string::ToString::to_string)
            .unwrap_or_default(),
        aliases: command.get_visible_aliases().map(str::to_string).collect(),
    }
}

fn parent_examples(name: &str) -> &'static [&'static str] {
    match name {
        "comments" => &[
            "br comments list br-abc --json",
            "br comments add br-abc --message \"Investigation notes\" --json",
            "br comments add br-abc --file notes.md --json",
        ],
        "dep" => &[
            "br dep add br-task br-blocker --type blocks --json",
            "br dep import edges.jsonl --robot",
            "br dep list br-task --direction both --format json",
            "br dep cycles --blocking-only --json",
        ],
        "query" => &[
            "br query list --json",
            "br query save p0-open --priority 0 --status open --description \"P0 open work\" --format json",
            "br query run p0-open --assignee agent-name --format json",
        ],
        "label" => &[
            "br label list --json",
            "br label add br-abc --label needs-review --json",
        ],
        "epic" => &[
            "br epic status --json",
            "br epic close-eligible --dry-run --json",
        ],
        "config" => &["br config get output.format --json"],
        "history" => &["br history list --json"],
        _ => &[],
    }
}

fn command_safety_notes(name: &str) -> &'static [&'static str] {
    match name {
        "comments" | "comments add" | "comments list" => comments_safety_notes(name),
        "dep" | "dep add" | "dep import" | "dep remove" | "dep list" | "dep tree"
        | "dep cycles" => dep_safety_notes(name),
        "query" | "query save" | "query run" | "query list" | "query delete" => {
            query_safety_notes(name)
        }
        "create" => &[
            "Creates a new issue and updates the last-touched issue unless `--dry-run` is supplied.",
            "`--file` bulk import cannot be combined with title arguments, `--external-ref`, or `--dry-run`.",
            "Use `--silent` when automation only needs the created issue ID on stdout.",
        ],
        "q" => &[
            "Quick capture creates an issue from title words and prints only the ID.",
            "Use full `create` when automation needs JSON, parent, deps, due, defer, or external-ref fields.",
        ],
        "update" => &[
            "Pass explicit IDs in automation; without IDs, update uses the last-touched issue.",
            "`--claim` refuses blocked issues unless `--force` is supplied.",
            "Use `--assignee \"\"`, `--owner \"\"`, `--due \"\"`, `--defer \"\"`, or `--parent \"\"` to clear those fields.",
        ],
        "close" => &[
            "Pass explicit IDs in automation; without IDs, close uses the last-touched issue.",
            "`--force` closes even when open dependencies still block the issue.",
            "Use `--suggest-next` only when closing one issue and you want newly unblocked work returned.",
        ],
        "delete" => &[
            "Delete creates tombstones by default; use `--dry-run` before bulk deletes.",
            "`--hard` prunes tombstones from JSONL immediately and should be rare.",
        ],
        "sync" => &[
            "br sync never runs git operations.",
            "External JSONL paths require both `BEADS_JSONL` and `--allow-external-jsonl`.",
            "Use `--status --json` or `--witness` for read-only diagnostics.",
        ],
        "vcs-status" => &[
            "Runs only when explicitly requested; sync and implicit startup paths never invoke it.",
            "Uses bounded, read-only Git plumbing with prompts, hooks, filters, lazy fetch, and optional locks disabled.",
            "The Git executable selected from PATH is trusted; this is not a process sandbox.",
            "External JSONL paths require `--allow-external-jsonl` and are redacted in output.",
        ],
        "doctor" => &[
            "Ordinary commands never cross a schema-version boundary implicitly.",
            "Use `doctor migrate-schema plan --json` before apply; apply requires the exact state-bound plan token.",
            "Migration undo refuses changed post-migration data, quarantines the displaced family without deletion, and is resumable.",
        ],
        "search" => &[
            "Search is read-only; prefer `--format json` or `--format toon` for parsing.",
            "Use list-style filters after the query to narrow result sets.",
        ],
        "count" => &[
            "Count is read-only; grouped output is selected with `--by` or `--by-*` aliases.",
            "Use filters such as `--status`, `--priority`, `--type`, and `--assignee` to match list/search scope.",
        ],
        "ready" => &[
            "Ready is read-only and is the single work-discovery entrypoint; it returns unblocked, non-deferred, actionable issues.",
            "Readiness defaults to status=open but is project-configurable via `workflow.status_groups.ready` in `.beads/policy.yaml` (e.g. `[open, rework]`); returned issues keep their real status.",
            "`--include-deferred` additionally surfaces deferred work and drops the defer-time gate; it never double-counts a status already in the ready group.",
        ],
        "scheduler" => &[
            "Scheduler is read-only and ranks already-ready issues; it does not claim work.",
            "Scheduler honors the same `workflow.status_groups.ready` group as `br ready`.",
            "Use `--limit` for returned recommendations and `--candidate-limit` for the scoring window.",
        ],
        "upgrade" => {
            &["Use `--check` to inspect availability before changing the installed binary."]
        }
        _ => &[],
    }
}

fn comments_safety_notes(name: &str) -> &'static [&'static str] {
    match name {
        "comments" => &[
            "Bare `br comments <id>` lists comments; `comments add` is the mutating subcommand.",
            "Use `--message` or `--file` for scripted comments instead of relying on shell word joining.",
        ],
        "comments add" => &[
            "Adds an audit comment and updates the last-touched issue.",
            "Use exactly one text source: positional words, `--message`, or `--file`.",
            "`--author` overrides the resolved actor for the comment only.",
        ],
        "comments list" => &[
            "Comments list is read-only; use `--wrap` only for human text output.",
            "Use `--json` when automation needs stable comment objects.",
        ],
        _ => &[],
    }
}

fn dep_safety_notes(name: &str) -> &'static [&'static str] {
    match name {
        "dep" => &[
            "`dep add <issue> <depends-on>` means `<issue>` waits on `<depends-on>`; do not reverse the order.",
            "Blocking dependencies reject self-dependencies and cycles.",
            "Read-only inspection lives under `dep list`, `dep tree`, and `dep cycles`.",
        ],
        "dep add" => &[
            "`dep add <issue> <depends-on>` means `<issue>` waits on `<depends-on>`.",
            "Default dependency type is `blocks`; use `--type parent-child` only for parent/child hierarchy.",
            "External dependency IDs must start with `external:` and are not locally resolved.",
        ],
        "dep import" => &[
            "Bulk-imports dependency edges from JSONL in one validated transaction.",
            "Accepts either edge records with `issue_id` + `depends_on_id` or normal issue records with `dependencies` arrays.",
            "Use this for large graphs instead of thousands of `dep add` processes.",
        ],
        "dep remove" => &[
            "Removes the edge from `<issue>` to `<depends-on>`; it does not delete either issue.",
            "JSON output reports `not_found` when the dependency edge was absent.",
        ],
        "dep list" => &[
            "Dependency list is read-only; `--direction down` shows what the issue waits on.",
            "Use `--direction up` for dependents and `--direction both` for a full local neighborhood.",
        ],
        "dep tree" => &[
            "Dependency tree is read-only; `--direction down` follows blockers from the root issue.",
            "Use `--max-depth` to bound large graphs in automation.",
            "Use global `--json` for JSON or `BR_OUTPUT_FORMAT=toon` for TOON; local `--format` selects text or mermaid.",
        ],
        "dep cycles" => &[
            "Cycle detection is read-only.",
            "Use `--blocking-only` when planning ready-work unblock order.",
            "Use global `--json` for JSON or `BR_OUTPUT_FORMAT=toon` for TOON; `dep cycles` does not accept a local `--format` flag.",
        ],
        _ => &[],
    }
}

fn query_safety_notes(name: &str) -> &'static [&'static str] {
    match name {
        "query" => &[
            "Saved queries live in br config storage, not in shell history.",
            "`query run` is read-only; `query save` and `query delete` mutate saved-query config.",
        ],
        "query save" => &[
            "Query names cannot be empty and cannot contain `:` or `/`.",
            "Saving fails if the name already exists; delete the old query first to replace it.",
            "Saved filters use the same filter flags as `br list`.",
        ],
        "query run" => &[
            "Query run is read-only and executes the saved filters through the list command.",
            "Additional CLI filters override or refine the saved filter set for that run.",
        ],
        "query list" => &[
            "Query list is read-only and returns saved-query metadata.",
            "Malformed saved query entries are skipped rather than executed.",
        ],
        "query delete" => &[
            "Deletes only the saved query definition; it never deletes issues.",
            "Deletion fails with a validation error when the saved query name does not exist.",
        ],
        _ => &[],
    }
}

#[allow(clippy::too_many_lines)]
fn command_contract(name: &str) -> CommandContract {
    match name {
        "comments add" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br comments add br-abc --message \"Investigation notes\" --json",
                "br comment add br-abc \"Short note\" --json",
                "br comments add br-abc --file notes.md --author Codex --json",
            ],
        },
        "comments list" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br comments list br-abc --json",
                "br comments br-abc --json",
            ],
        },
        "dep add" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br dep add br-task br-blocker --type blocks --json",
                "br dep add br-child br-parent --type parent-child --json",
                "br dep add br-task external:repo-123 --metadata '{\"repo\":\"other\"}' --json",
            ],
        },
        "dep import" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br dep import edges.jsonl --robot",
                "br dep import .beads/issues.jsonl --robot",
            ],
        },
        "dep remove" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br dep remove br-task br-blocker --json"],
        },
        "dep list" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br dep list br-task --direction down --format json",
                "br dep list br-task --direction both --type blocks --format toon",
            ],
        },
        "dep tree" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br dep tree br-task --direction down --max-depth 5 --json",
                "BR_OUTPUT_FORMAT=toon br dep tree br-task --direction down --max-depth 5",
                "br dep tree br-task --direction up --format mermaid",
            ],
        },
        "dep cycles" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br dep cycles --json",
                "br dep cycles --blocking-only --json",
                "BR_OUTPUT_FORMAT=toon br dep cycles --blocking-only",
            ],
        },
        "query save" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br query save p0-open --priority 0 --status open --description \"P0 open work\" --format json",
                "br query save mine --assignee agent-name --status in_progress --format json",
            ],
        },
        "query run" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "csv", "toon", "text"],
            examples: &[
                "br query run p0-open --format json",
                "br query run p0-open --status open --format toon",
            ],
        },
        "query list" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br query list --json"],
        },
        "query delete" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br query delete stale-filter --json"],
        },
        "label add" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br label add br-abc --label needs-review --json"],
        },
        "label remove" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br label remove br-abc --label needs-review --json"],
        },
        "label list" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br label list --json"],
        },
        "label rename" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br label rename old-label new-label --json"],
        },
        "epic status" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br epic status --json"],
        },
        "epic close-eligible" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br epic close-eligible --dry-run --json"],
        },
        "capabilities" => CommandContract {
            operation: "read",
            workspace: "none",
            machine_output: &["json", "toon", "text"],
            examples: &["br capabilities --format json"],
        },
        "robot-docs" => CommandContract {
            operation: "read",
            workspace: "none",
            machine_output: &["json", "toon", "text"],
            examples: &["br robot-docs guide"],
        },
        "schema" => CommandContract {
            operation: "read",
            workspace: "none",
            machine_output: &["json", "toon", "text"],
            examples: &["br schema commands --format json"],
        },
        "version" => CommandContract {
            operation: "read",
            workspace: "none",
            machine_output: &["json", "toon", "text"],
            examples: &["br version --json"],
        },
        "completions" => CommandContract {
            operation: "read",
            workspace: "none",
            machine_output: &["text"],
            examples: &["br completions zsh"],
        },
        "init" => CommandContract {
            operation: "write",
            workspace: "none",
            machine_output: &["text"],
            examples: &["br init --prefix br"],
        },
        "create" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br create \"Fix login\" --type bug --priority 1 --json",
                "br create --title \"Investigate slow ready\" --slug slow-ready --labels perf,agent-workflow --json",
                "br create --file backlog.md --json",
            ],
        },
        "q" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br q \"Quick note\""],
        },
        "update" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br update br-abc --claim --json",
                "br update br-abc --status in_progress --assignee agent-name --json",
                "br update br-abc --add-label needs-review --json",
            ],
        },
        "close" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br close br-abc --reason \"Completed\" --json",
                "br close br-abc --reason \"Completed\" --suggest-next --json",
                "br close br-abc --agent-name Codex --model gpt-5 --harness local --json",
            ],
        },
        "reopen" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br reopen br-abc --json"],
        },
        "delete" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br delete br-abc --reason duplicate --json"],
        },
        "defer" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br defer br-abc --until tomorrow --json"],
        },
        "undefer" => CommandContract {
            operation: "write",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br undefer br-abc --json"],
        },
        "list" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "csv", "text"],
            examples: &["br list --status open --format json"],
        },
        "ready" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br ready --json"],
        },
        "scheduler" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br scheduler --json",
                "br scheduler --limit 5 --candidate-limit 100 --format json",
            ],
        },
        "coordination" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br coordination status --json"],
        },
        "blocked" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br blocked --json"],
        },
        "show" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br show br-abc --json"],
        },
        "search" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "csv", "text"],
            examples: &[
                "br search \"auth\" --format json",
                "br search \"auth\" --status open --priority 0 --format toon",
            ],
        },
        "count" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br count --by status --json",
                "br count --by-label --status open --json",
            ],
        },
        "stale" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br stale --days 30 --json"],
        },
        "stats" | "status" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &["br stats --format json"],
        },
        "where" | "info" => CommandContract {
            operation: "read",
            workspace: "optional",
            machine_output: &["json", "toon", "text"],
            examples: &["br where --json"],
        },
        "sync" => CommandContract {
            operation: "mixed",
            workspace: "required",
            machine_output: &["json", "text"],
            examples: &["br sync --status --json", "br sync --flush-only"],
        },
        "vcs-status" => CommandContract {
            operation: "read",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[
                "br vcs-status --json",
                "br vcs-status --timeout-ms 5000 --robot",
            ],
        },
        "doctor" => CommandContract {
            operation: "mixed",
            workspace: "optional",
            machine_output: &["json", "text"],
            examples: &[
                "br doctor --json",
                "br doctor migrate-schema plan --json",
                "br doctor migrate-schema apply --plan-token <TOKEN> --json",
                "br doctor migrate-schema undo <RUN_ID> --dry-run --json",
            ],
        },
        "config" => CommandContract {
            operation: "mixed",
            workspace: "required",
            machine_output: &["json", "text"],
            examples: parent_examples(name),
        },
        "history" | "query" | "dep" | "label" | "comments" | "epic" => CommandContract {
            operation: "mixed",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: parent_examples(name),
        },
        "graph" | "orphans" | "changelog" | "lint" | "audit" => CommandContract {
            operation: "mixed",
            workspace: "required",
            machine_output: &["json", "toon", "text"],
            examples: &[],
        },
        "agents" => CommandContract {
            operation: "mixed",
            workspace: "optional",
            machine_output: &["json", "text"],
            examples: &["br agents --check --json"],
        },
        "upgrade" => CommandContract {
            operation: "write",
            workspace: "none",
            machine_output: &["json", "text"],
            examples: &["br upgrade --check --json"],
        },
        _ => CommandContract {
            operation: "unknown",
            workspace: "unknown",
            machine_output: &["text"],
            examples: &[],
        },
    }
}

fn render_text(output: &CapabilitiesOutput, requested_command: Option<&str>) {
    println!(
        "{} {} ({})",
        output.tool, output.version, output.contract_version
    );
    println!();
    println!("Recommended agent entrypoints:");
    for command in output.recommended_entrypoints {
        println!("  {command}");
    }
    println!();
    println!("Commands:");
    for command in &output.commands {
        println!(
            "  {:<16} {:<5} {:<8} {}",
            command.name, command.operation, command.workspace, command.summary
        );
    }
    if let Some(detail) = output.command_detail.as_ref() {
        println!();
        println!(
            "Command detail: {}",
            requested_command.unwrap_or(detail.path.as_str())
        );
        println!(
            "  canonical: {}  operation: {}  workspace: {}",
            detail.path, detail.operation, detail.workspace
        );
        if !detail.examples.is_empty() {
            println!("  examples:");
            for example in detail.examples {
                println!("    {example}");
            }
        }
        if !detail.safety_notes.is_empty() {
            println!("  safety notes:");
            for note in detail.safety_notes {
                println!("    {note}");
            }
        }
        if !detail.arguments.is_empty() {
            println!("  arguments:");
            for argument in &detail.arguments {
                let spellings = [argument.short.as_deref(), argument.long.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", ");
                let name = if spellings.is_empty() {
                    argument.id.as_str()
                } else {
                    spellings.as_str()
                };
                println!(
                    "    {:<22} {:<10} required={} {}",
                    name,
                    argument.kind,
                    argument.required,
                    argument.help.as_deref().unwrap_or_default()
                );
            }
        }
        if !detail.subcommands.is_empty() {
            println!("  subcommands:");
            for subcommand in &detail.subcommands {
                println!("    {:<22} {}", subcommand.name, subcommand.summary);
            }
        }
    }
    println!();
    println!("Safety:");
    for item in output.safety {
        println!("  {}: {}", item.name, item.guarantee);
    }
}

#[cfg(test)]
mod tests {
    use super::{command_contract, command_detail_for_path};

    #[test]
    fn vcs_status_capability_is_explicit_read_only_and_machine_discoverable() {
        let contract = command_contract("vcs-status");
        assert_eq!(contract.operation, "read");
        assert_eq!(contract.workspace, "required");
        assert_eq!(contract.machine_output, ["json", "toon", "text"]);
        assert!(
            contract
                .examples
                .iter()
                .any(|example| example.contains("br vcs-status --json"))
        );

        let detail = command_detail_for_path("vcs-status").expect("vcs-status detail");
        assert_eq!(detail.operation, "read");
        assert!(
            detail
                .safety_notes
                .iter()
                .any(|note| note.contains("selected from PATH is trusted"))
        );
    }

    #[test]
    fn doctor_capability_exposes_the_reviewed_schema_migration_workflow() {
        let contract = command_contract("doctor");
        assert_eq!(contract.operation, "mixed");
        assert!(
            contract
                .examples
                .iter()
                .any(|example| example.contains("migrate-schema plan --json"))
        );

        let detail = command_detail_for_path("doctor").expect("doctor detail");
        assert!(
            detail
                .safety_notes
                .iter()
                .any(|note| note.contains("never cross a schema-version boundary"))
        );
    }
}
