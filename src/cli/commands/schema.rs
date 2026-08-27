//! Schema command implementation.
//!
//! Emits two complementary descriptions of br's machine-readable surface:
//!
//! 1. **Per-row JSON Schemas** for the data types that show up inside
//!    command outputs (`Issue`, `IssueWithCounts`, `IssueDetails`, …).
//! 2. **Per-command output-envelope shapes** that tell an agent how to
//!    reach those rows from a command's `--json` payload (`.[0]` for
//!    `show`, `.issues[]` for `list`, `.[]` for the array commands, …).
//!
//! Intended for AI agents and tooling that want stable schemas without
//! reading source code. The CLI surface marks `br schema` as
//! not-yet-stable; agents should re-call across release boundaries.

use crate::cli::commands::vcs::VcsExportStatus;
use crate::cli::{
    OutputFormat, SchemaArgs, SchemaTarget, resolve_output_format_basic_with_outer_mode,
};
use crate::coordination::{CoordinationClaimRow, CoordinationStatusOutput};
use crate::error::Result;
use crate::format::{
    BlockedIssueOutput, BlockedPage, IssueDetails, IssueWithCounts, ReadyIssue, StaleIssue,
    Statistics,
};
use crate::model::Issue;
use crate::output::{OutputContext, OutputMode};
use crate::sync::AdditiveReconcileReceipt;
use crate::{config, output};
use chrono::{DateTime, Utc};
use schemars::{JsonSchema, Schema, generate::SchemaSettings};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ErrorBody {
    /// Machine-readable error code (SCREAMING_SNAKE_CASE)
    code: String,
    /// Human-readable message
    message: String,
    /// Optional hint for remediation
    hint: Option<String>,
    /// Whether the operation can be retried
    retryable: bool,
    /// Additional context for debugging (arbitrary JSON)
    context: Option<serde_json::Value>,
}

/// Row emitted by `br dep tree --json`.
///
/// Keep this in sync with `cli::commands::dep::TreeNode`. The command emits a
/// compact traversal node, not the older `format::output::TreeNode` shape that
/// flattened a full `Issue`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct TreeNode {
    /// Stable issue ID, external dependency ID, or missing-issue placeholder ID.
    id: String,
    /// Human-readable title used in text/tree output.
    title: String,
    /// Depth from the requested root issue. The root is depth 0.
    depth: usize,
    /// Parent node issue ID, or null for the root.
    parent_id: Option<String>,
    /// Numeric issue priority used for sibling sorting.
    priority: i32,
    /// Issue status string, or synthesized status for external/missing nodes.
    status: String,
    /// True when the node has children omitted by `--max-depth`.
    truncated: bool,
}

/// Row emitted inside `br count --by <field> --json` under `groups[]`.
///
/// Keep this in sync with `cli::commands::count::CountGroup`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
struct CountGroup {
    /// Group label, for example a status, priority, type, assignee, or label.
    group: String,
    /// Number of matching issues in this group.
    count: usize,
}

#[derive(Debug, Serialize)]
struct SchemaOutput {
    tool: &'static str,
    generated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    schemas: BTreeMap<&'static str, Schema>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    commands: BTreeMap<&'static str, CommandShape>,
}

/// Per-command output-envelope description for `br schema commands`.
///
/// This describes the *top-level JSON shape* a command emits with `--json`,
/// not the per-row schema (which lives in `schemas`). Agents can use the
/// `jq_filter` to extract individual items uniformly across commands without
/// hard-coding per-command knowledge.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
struct CommandShape {
    /// Top-level JSON shape: "array" | "object" | "scalar".
    shape: &'static str,
    /// jq filter that extracts the useful payload from the command's
    /// `--json` output. For iterable commands this yields one item per
    /// invocation (e.g. `.[]` or `.issues[]`). For aggregate commands
    /// it extracts the value of interest — `.` for whole-object outputs
    /// like `stats`/`info`, or a specific path like `.count` to dig out
    /// a single scalar.
    jq_filter: &'static str,
    /// jq path to the iterable items. `"."` means the top-level value
    /// itself is the array; `".issues"` means items live at `.issues`.
    /// `None` for non-iterable outputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    items_at: Option<&'static str>,
    /// Name of the schema each item conforms to. References a key in
    /// the `schemas` map (e.g. `"Issue"`, `"IssueWithCounts"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    item_schema: Option<&'static str>,
    /// Whether machine-mode failures write an `ErrorEnvelope` to stderr.
    /// False means callers must consult the command notes for the actual
    /// stream; the current top-level CLI handler emits structured errors on
    /// stdout for the two safety command shapes below.
    error_envelope_on_stderr: bool,
    /// Free-form notes describing quirks of the envelope.
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<&'static str>,
}

/// Execute the schema command.
///
/// Builds the per-row schemas and the per-command envelope map appropriate
/// for `args.target` and prints a single bundled `SchemaOutput` payload in
/// the resolved output format. `Quiet` mode short-circuits before any work.
///
/// # Errors
///
/// Returns an error if output cannot be written.
#[allow(clippy::missing_panics_doc)]
pub fn execute(
    args: &SchemaArgs,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
) -> Result<()> {
    let output_format = resolve_output_format_basic_with_outer_mode(
        args.format,
        outer_ctx.inherited_output_mode(),
        false,
    );
    let quiet = cli.quiet.unwrap_or(false);

    // Schema output is always machine-readable; for text mode we print pretty JSON.
    let ctx = output::OutputContext::from_output_format(output_format, quiet, true);
    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    let schemas = build_schemas(args.target);
    let commands = build_commands(args.target);
    let payload = SchemaOutput {
        tool: "br",
        generated_at: Utc::now(),
        schemas,
        commands,
    };

    match output_format {
        OutputFormat::Toon => {
            ctx.toon_with_stats(&payload, args.stats);
        }
        OutputFormat::Json => {
            ctx.json_pretty(&payload);
        }
        OutputFormat::Text | OutputFormat::Csv => {
            // Text mode: still emit JSON Schema; don't require callers to pass --json.
            let json = serde_json::to_string_pretty(&payload).expect("schema payload is JSON");
            println!("{json}");
        }
    }

    Ok(())
}

fn build_schemas(target: SchemaTarget) -> BTreeMap<&'static str, Schema> {
    let mut schemas = BTreeMap::new();

    match target {
        SchemaTarget::All => {
            schemas.insert("Issue", schema_for_output::<Issue>());
            schemas.insert("IssueWithCounts", schema_for_output::<IssueWithCounts>());
            schemas.insert("IssueDetails", schema_for_output::<IssueDetails>());
            schemas.insert("ReadyIssue", schema_for_output::<ReadyIssue>());
            schemas.insert("StaleIssue", schema_for_output::<StaleIssue>());
            schemas.insert("BlockedIssue", schema_for_output::<BlockedIssueOutput>());
            schemas.insert("BlockedPage", schema_for_output::<BlockedPage>());
            schemas.insert("TreeNode", schema_for_output::<TreeNode>());
            schemas.insert("CountGroup", schema_for_output::<CountGroup>());
            schemas.insert("Statistics", schema_for_output::<Statistics>());
            schemas.insert(
                "CoordinationStatusOutput",
                schema_for_output::<CoordinationStatusOutput>(),
            );
            schemas.insert(
                "CoordinationClaimRow",
                schema_for_output::<CoordinationClaimRow>(),
            );
            schemas.insert(
                "SyncReconcileReceipt",
                schema_for_output::<crate::cli::commands::sync::SyncReconcileReceipt>(),
            );
            schemas.insert(
                "AdditiveReconcileReceipt",
                schema_for_output::<AdditiveReconcileReceipt>(),
            );
            schemas.insert("VcsExportStatus", schema_for_output::<VcsExportStatus>());
            schemas.insert("ErrorEnvelope", schema_for_output::<ErrorEnvelope>());
        }
        SchemaTarget::Issue => {
            schemas.insert("Issue", schema_for_output::<Issue>());
        }
        SchemaTarget::IssueWithCounts => {
            schemas.insert("IssueWithCounts", schema_for_output::<IssueWithCounts>());
        }
        SchemaTarget::IssueDetails => {
            schemas.insert("IssueDetails", schema_for_output::<IssueDetails>());
        }
        SchemaTarget::ReadyIssue => {
            schemas.insert("ReadyIssue", schema_for_output::<ReadyIssue>());
        }
        SchemaTarget::StaleIssue => {
            schemas.insert("StaleIssue", schema_for_output::<StaleIssue>());
        }
        SchemaTarget::BlockedIssue => {
            schemas.insert("BlockedIssue", schema_for_output::<BlockedIssueOutput>());
            schemas.insert("BlockedPage", schema_for_output::<BlockedPage>());
        }
        SchemaTarget::TreeNode => {
            schemas.insert("TreeNode", schema_for_output::<TreeNode>());
        }
        SchemaTarget::Statistics => {
            schemas.insert("Statistics", schema_for_output::<Statistics>());
        }
        SchemaTarget::CoordinationStatus => {
            schemas.insert(
                "CoordinationStatusOutput",
                schema_for_output::<CoordinationStatusOutput>(),
            );
            schemas.insert(
                "CoordinationClaimRow",
                schema_for_output::<CoordinationClaimRow>(),
            );
        }
        SchemaTarget::AdditiveReconciliation => {
            schemas.insert(
                "AdditiveReconcileReceipt",
                schema_for_output::<AdditiveReconcileReceipt>(),
            );
        }
        SchemaTarget::VcsStatus => {
            schemas.insert("VcsExportStatus", schema_for_output::<VcsExportStatus>());
        }
        SchemaTarget::Error => {
            schemas.insert("ErrorEnvelope", schema_for_output::<ErrorEnvelope>());
        }
        SchemaTarget::Commands => {
            // Only the command-shape map is requested; no per-row schemas.
        }
    }

    schemas
}

fn schema_for_output<T: JsonSchema>() -> Schema {
    SchemaSettings::default()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<T>()
}

/// Build the per-command output-envelope map.
///
/// Empty for targets that only request a per-row schema. The `All` and
/// `Commands` targets populate the full map. Entries are hand-curated;
/// the `command_shapes_have_consistent_invariants` test below is the
/// regression net that catches structural drift (unknown shapes,
/// non-jq paths, item_schema names that aren't in the schemas catalog).
fn build_commands(target: SchemaTarget) -> BTreeMap<&'static str, CommandShape> {
    let mut commands = BTreeMap::new();
    if !matches!(target, SchemaTarget::All | SchemaTarget::Commands) {
        return commands;
    }

    insert_issue_command_shapes(&mut commands);
    insert_comment_command_shapes(&mut commands);
    insert_dependency_command_shapes(&mut commands);
    insert_aggregate_command_shapes(&mut commands);
    insert_label_command_shapes(&mut commands);
    commands.insert(
        "sync --reconcile-additive",
        CommandShape {
            shape: "object",
            jq_filter: ".",
            items_at: None,
            item_schema: Some("AdditiveReconcileReceipt"),
            error_envelope_on_stderr: false,
            notes: Some(
                "Dry-run by default. Apply requires the exact plan_sha256 from an \
                 identically configured reviewed dry-run via --expect-plan-sha256. \
                 Structured machine-mode errors are currently emitted on stdout.",
            ),
        },
    );
    commands.insert(
        "vcs-status",
        CommandShape {
            shape: "object",
            jq_filter: ".",
            items_at: None,
            item_schema: Some("VcsExportStatus"),
            error_envelope_on_stderr: false,
            notes: Some(
                "Explicit bounded read-only Git diagnostic. It is isolated from sync, \
                 retains HEAD/index evidence when worktree comparison is unavailable, \
                 never executes Git content filters, and reports sequential evidence with \
                 observation_atomic=false. Structured machine-mode errors are currently \
                 emitted on stdout.",
            ),
        },
    );

    commands
}

fn insert_issue_command_shapes(commands: &mut BTreeMap<&'static str, CommandShape>) {
    commands.insert(
        "show",
        CommandShape {
            shape: "array",
            jq_filter: ".[0]",
            items_at: Some("."),
            item_schema: Some("IssueDetails"),
            error_envelope_on_stderr: true,
            notes: Some(
                "Always a single-element array on success (wrapped for shape consistency \
                 with list-style commands). On a missing id, an ErrorEnvelope is written \
                 to stderr and exit code is non-zero.",
            ),
        },
    );
    commands.insert(
        "list",
        CommandShape {
            shape: "object",
            jq_filter: ".issues[]",
            items_at: Some(".issues"),
            item_schema: Some("IssueWithCounts"),
            error_envelope_on_stderr: false,
            notes: Some(
                "Wrapper object with pagination metadata; iterate with `.issues[]`. \
                 The wrapper exists so totals/cursors can ride alongside the rows.",
            ),
        },
    );
    commands.insert(
        "ready",
        CommandShape {
            shape: "array",
            jq_filter: ".[]",
            items_at: Some("."),
            item_schema: Some("ReadyIssue"),
            error_envelope_on_stderr: false,
            notes: None,
        },
    );
    commands.insert(
        "blocked",
        CommandShape {
            shape: "object",
            jq_filter: ".issues[]",
            items_at: Some(".issues"),
            item_schema: Some("BlockedIssue"),
            error_envelope_on_stderr: false,
            notes: Some(
                "Wrapper object with `total`, `limit`, `offset`, and `has_more`; iterate with `.issues[]`.",
            ),
        },
    );
    commands.insert(
        "stale",
        CommandShape {
            shape: "array",
            jq_filter: ".[]",
            items_at: Some("."),
            item_schema: Some("StaleIssue"),
            error_envelope_on_stderr: false,
            notes: None,
        },
    );
    commands.insert(
        "search",
        CommandShape {
            shape: "object",
            jq_filter: ".issues[]",
            items_at: Some(".issues"),
            item_schema: Some("IssueWithCounts"),
            error_envelope_on_stderr: false,
            notes: Some(
                "Stable wrapper object `{\"issues\": [...], \
                 \"hidden_closed_count\": N}`; iterate with `.issues[]`. The count \
                 is zero when no closed matches were hidden or the selected corpus \
                 already includes closed issues. Pagination metadata also reports \
                 `limit`, `offset`, and `has_more`; `has_more` discloses truncation.",
            ),
        },
    );
}

fn insert_comment_command_shapes(commands: &mut BTreeMap<&'static str, CommandShape>) {
    commands.insert(
        "comments list",
        CommandShape {
            shape: "array",
            jq_filter: ".[]",
            items_at: Some("."),
            item_schema: None,
            error_envelope_on_stderr: true,
            notes: Some(
                "Empty array if no comments. ErrorEnvelope on stderr if the issue id \
                 cannot be resolved.",
            ),
        },
    );
}

fn insert_dependency_command_shapes(commands: &mut BTreeMap<&'static str, CommandShape>) {
    commands.insert(
        "dep tree",
        CommandShape {
            shape: "array",
            jq_filter: ".[]",
            items_at: Some("."),
            item_schema: Some("TreeNode"),
            error_envelope_on_stderr: true,
            notes: Some("Pre-order traversal; each node carries a `depth` field."),
        },
    );
    commands.insert(
        "dep list",
        CommandShape {
            shape: "array",
            jq_filter: ".[]",
            items_at: Some("."),
            item_schema: None,
            error_envelope_on_stderr: true,
            notes: None,
        },
    );
}

fn insert_aggregate_command_shapes(commands: &mut BTreeMap<&'static str, CommandShape>) {
    // `br status` is documented as an alias for `br stats` and dispatches
    // through the same handler, so the envelope is identical. We list both
    // keys (with differentiated `notes`) so an agent looking up either name
    // gets the right shape without an indirection step, but can still tell
    // which name is canonical.
    let stats_base = CommandShape {
        shape: "object",
        jq_filter: ".",
        items_at: None,
        item_schema: Some("Statistics"),
        error_envelope_on_stderr: false,
        notes: Some("Single aggregate object."),
    };
    let mut status_alias = stats_base.clone();
    status_alias.notes = Some("Alias for `stats` — identical envelope.");
    commands.insert("stats", stats_base);
    commands.insert("status", status_alias);

    commands.insert(
        "count",
        CommandShape {
            shape: "object",
            jq_filter: ".count",
            items_at: None,
            item_schema: None,
            error_envelope_on_stderr: false,
            notes: Some(
                "Ungrouped `br count --json`; scalar count under `.count`. \
                 Grouped variants use the `count --by` entry.",
            ),
        },
    );
    commands.insert(
        "count --by",
        CommandShape {
            shape: "object",
            jq_filter: ".groups[]",
            items_at: Some(".groups"),
            item_schema: Some("CountGroup"),
            error_envelope_on_stderr: false,
            notes: Some(
                "For `br count --by <status|priority|type|assignee|label> --json`. \
                 The wrapper object also includes `total`.",
            ),
        },
    );
    commands.insert(
        "info",
        CommandShape {
            shape: "object",
            jq_filter: ".",
            items_at: None,
            item_schema: None,
            error_envelope_on_stderr: false,
            notes: Some("Workspace info object (paths, mode, config snapshot)."),
        },
    );
    commands.insert(
        "capabilities",
        CommandShape {
            shape: "object",
            jq_filter: ".",
            items_at: None,
            item_schema: None,
            error_envelope_on_stderr: false,
            notes: Some(
                "Machine-readable command, feature, safety, exit-code, and env-var inventory.",
            ),
        },
    );
    commands.insert(
        "robot-docs guide",
        CommandShape {
            shape: "object",
            jq_filter: ".",
            items_at: None,
            item_schema: None,
            error_envelope_on_stderr: false,
            notes: Some(
                "JSON/TOON modes wrap the concise agent guide; text mode prints the guide directly.",
            ),
        },
    );
    commands.insert(
        "coordination status",
        CommandShape {
            shape: "object",
            jq_filter: ".claims[]",
            items_at: Some(".claims"),
            item_schema: Some("CoordinationClaimRow"),
            error_envelope_on_stderr: false,
            notes: Some(
                "Read-only object with workspace summary and `claims[]` rows. The \
                 full envelope schema is `CoordinationStatusOutput`.",
            ),
        },
    );
}

fn insert_label_command_shapes(commands: &mut BTreeMap<&'static str, CommandShape>) {
    commands.insert(
        "label list",
        CommandShape {
            shape: "array",
            jq_filter: ".[]",
            items_at: Some("."),
            item_schema: None,
            error_envelope_on_stderr: false,
            notes: None,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn schema_generation_is_json_serializable() {
        let schemas = build_schemas(SchemaTarget::All);
        for (name, schema) in schemas {
            let value = serde_json::to_value(&schema).expect("schema serializable");
            assert!(value.is_object(), "{name} schema should be a JSON object");
        }
    }

    #[test]
    fn commands_target_emits_only_command_map() {
        let schemas = build_schemas(SchemaTarget::Commands);
        let commands = build_commands(SchemaTarget::Commands);
        assert!(
            schemas.is_empty(),
            "Commands target should not emit per-row schemas"
        );
        assert!(
            !commands.is_empty(),
            "Commands target should emit a non-empty command map"
        );
    }

    #[test]
    fn vcs_status_schema_and_command_shape_are_discoverable() {
        let schemas = build_schemas(SchemaTarget::VcsStatus);
        assert!(schemas.contains_key("VcsExportStatus"));

        let commands = build_commands(SchemaTarget::Commands);
        let shape = commands
            .get("vcs-status")
            .expect("vcs-status command shape");
        assert_eq!(shape.shape, "object");
        assert_eq!(shape.jq_filter, ".");
        assert_eq!(shape.item_schema, Some("VcsExportStatus"));
        assert!(
            !shape.error_envelope_on_stderr,
            "top-level structured CLI errors are emitted on stdout"
        );
    }

    #[test]
    fn all_target_emits_both_schemas_and_commands() {
        let schemas = build_schemas(SchemaTarget::All);
        let commands = build_commands(SchemaTarget::All);
        assert!(!schemas.is_empty(), "All target must include schemas");
        assert!(!commands.is_empty(), "All target must include commands");
    }

    #[test]
    fn all_schema_target_covers_every_named_schema_target() {
        let all_schemas = build_schemas(SchemaTarget::All);

        for target in SchemaTarget::value_variants().iter().copied() {
            if matches!(target, SchemaTarget::All | SchemaTarget::Commands) {
                continue;
            }

            let target_schemas = build_schemas(target);
            assert!(
                !target_schemas.is_empty(),
                "{target:?} should emit at least one schema"
            );

            for schema_name in target_schemas.keys() {
                assert!(
                    all_schemas.contains_key(schema_name),
                    "SchemaTarget::{target:?} emits {schema_name:?}, but \
                     SchemaTarget::All omits it"
                );
            }
        }
    }

    #[test]
    fn all_and_commands_targets_emit_identical_command_maps() {
        assert_eq!(
            build_commands(SchemaTarget::All),
            build_commands(SchemaTarget::Commands),
            "SchemaTarget::All and SchemaTarget::Commands must describe the \
             same command envelopes"
        );
    }

    #[test]
    fn per_row_target_does_not_emit_command_map() {
        let commands = build_commands(SchemaTarget::Issue);
        assert!(
            commands.is_empty(),
            "Per-row schema targets must not include command map"
        );
    }

    #[test]
    fn command_shapes_have_consistent_invariants() {
        let commands = build_commands(SchemaTarget::Commands);
        // Build schemas once, not per-iteration — schema_for! does real work.
        let known_schemas = build_schemas(SchemaTarget::All);
        for (name, shape) in &commands {
            // shape must be one of the documented values.
            assert!(
                matches!(shape.shape, "array" | "object" | "scalar"),
                "{name}: shape={:?} is not one of array|object|scalar",
                shape.shape
            );
            // jq_filter must be non-empty.
            assert!(!shape.jq_filter.is_empty(), "{name}: jq_filter is empty");
            // If shape is array, items_at must be Some(".") (jq identity — the
            // top-level value itself is the array we iterate).
            if shape.shape == "array" {
                assert_eq!(
                    shape.items_at,
                    Some("."),
                    "{name}: array shapes must set items_at = \".\""
                );
            }
            // If items_at is set, it must be a jq path starting with `.`
            // (consistent with the `.issues` / `.` notation used for `list`
            // and the array-shaped commands).
            if let Some(path) = shape.items_at {
                assert!(
                    path.starts_with('.'),
                    "{name}: items_at {path:?} must be a jq path starting with `.`"
                );
            }
            // If item_schema is set, it must reference a known schema name from build_schemas(All).
            if let Some(item_schema) = shape.item_schema {
                assert!(
                    known_schemas.contains_key(item_schema),
                    "{name}: item_schema {item_schema:?} is not a known schema target"
                );
            }
        }
    }

    #[test]
    fn tree_node_schema_matches_dep_tree_payload_shape() {
        let schemas = build_schemas(SchemaTarget::TreeNode);
        let schema = schemas
            .get("TreeNode")
            .expect("TreeNode schema should be present");
        let schema = serde_json::to_value(schema).expect("TreeNode schema should serialize");
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("TreeNode schema should expose object properties");

        for field in [
            "id",
            "title",
            "depth",
            "parent_id",
            "priority",
            "status",
            "truncated",
        ] {
            assert!(
                properties.contains_key(field),
                "TreeNode schema missing actual dep tree field {field:?}"
            );
        }

        for stale_issue_field in ["created_at", "updated_at", "issue_type", "labels"] {
            assert!(
                !properties.contains_key(stale_issue_field),
                "TreeNode schema should not flatten full Issue field {stale_issue_field:?}"
            );
        }
    }

    #[test]
    fn stats_and_status_share_the_same_envelope() {
        // `br status` is documented as an alias for `br stats`; the schema
        // map should describe identical shapes for both names so agents
        // looking up either name get a consistent answer.
        let commands = build_commands(SchemaTarget::Commands);
        let stats = commands.get("stats").expect("stats entry must exist");
        let status = commands.get("status").expect("status alias must exist");
        assert_eq!(stats.shape, status.shape);
        assert_eq!(stats.jq_filter, status.jq_filter);
        assert_eq!(stats.items_at, status.items_at);
        assert_eq!(stats.item_schema, status.item_schema);
        assert_eq!(
            stats.error_envelope_on_stderr,
            status.error_envelope_on_stderr
        );
    }

    #[test]
    fn count_command_shapes_cover_ungrouped_and_grouped_envelopes() {
        let schemas = build_schemas(SchemaTarget::All);
        assert!(
            schemas.contains_key("CountGroup"),
            "schema all must include grouped count rows"
        );

        let commands = build_commands(SchemaTarget::Commands);
        let count = commands
            .get("count")
            .expect("ungrouped count entry must exist");
        assert_eq!(count.jq_filter, ".count");
        assert_eq!(count.items_at, None);
        assert_eq!(count.item_schema, None);

        let grouped = commands
            .get("count --by")
            .expect("grouped count entry must exist");
        assert_eq!(grouped.jq_filter, ".groups[]");
        assert_eq!(grouped.items_at, Some(".groups"));
        assert_eq!(grouped.item_schema, Some("CountGroup"));
    }
}
