//! Schema command implementation.
//!
//! Emits JSON Schema documents describing br's primary machine-readable outputs.
//! This is intended for AI agents and tooling that want stable schemas without
//! reading source code.

use crate::cli::{OutputFormat, SchemaArgs, SchemaTarget, resolve_output_format_basic};
use crate::error::Result;
use crate::format::{
    BlockedIssue, IssueDetails, IssueWithCounts, ReadyIssue, StaleIssue, Statistics, TreeNode,
};
use crate::model::Issue;
use crate::output::{OutputContext, OutputMode};
use crate::{config, output};
use chrono::{DateTime, Utc};
use schemars::schema::RootSchema;
use schemars::schema_for;
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

#[derive(Debug, Serialize)]
struct SchemaOutput {
    tool: &'static str,
    generated_at: DateTime<Utc>,
    schemas: BTreeMap<&'static str, RootSchema>,
}

pub fn execute(args: &SchemaArgs, cli: &config::CliOverrides, outer_ctx: &OutputContext) -> Result<()> {
    let output_format = resolve_output_format_basic(args.format, outer_ctx.is_json(), false);
    let quiet = cli.quiet.unwrap_or(false);

    // Schema output is always machine-readable; for text mode we print pretty JSON.
    let ctx = output::OutputContext::from_output_format(output_format, quiet, true);
    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    let schemas = build_schemas(args.target);
    let payload = SchemaOutput {
        tool: "br",
        generated_at: Utc::now(),
        schemas,
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

fn build_schemas(target: SchemaTarget) -> BTreeMap<&'static str, RootSchema> {
    let mut schemas = BTreeMap::new();

    match target {
        SchemaTarget::All => {
            schemas.insert("Issue", schema_for!(Issue));
            schemas.insert("IssueWithCounts", schema_for!(IssueWithCounts));
            schemas.insert("IssueDetails", schema_for!(IssueDetails));
            schemas.insert("ReadyIssue", schema_for!(ReadyIssue));
            schemas.insert("StaleIssue", schema_for!(StaleIssue));
            schemas.insert("BlockedIssue", schema_for!(BlockedIssue));
            schemas.insert("TreeNode", schema_for!(TreeNode));
            schemas.insert("Statistics", schema_for!(Statistics));
            schemas.insert("ErrorEnvelope", schema_for!(ErrorEnvelope));
        }
        SchemaTarget::Issue => {
            schemas.insert("Issue", schema_for!(Issue));
        }
        SchemaTarget::IssueWithCounts => {
            schemas.insert("IssueWithCounts", schema_for!(IssueWithCounts));
        }
        SchemaTarget::IssueDetails => {
            schemas.insert("IssueDetails", schema_for!(IssueDetails));
        }
        SchemaTarget::ReadyIssue => {
            schemas.insert("ReadyIssue", schema_for!(ReadyIssue));
        }
        SchemaTarget::StaleIssue => {
            schemas.insert("StaleIssue", schema_for!(StaleIssue));
        }
        SchemaTarget::BlockedIssue => {
            schemas.insert("BlockedIssue", schema_for!(BlockedIssue));
        }
        SchemaTarget::TreeNode => {
            schemas.insert("TreeNode", schema_for!(TreeNode));
        }
        SchemaTarget::Statistics => {
            schemas.insert("Statistics", schema_for!(Statistics));
        }
        SchemaTarget::Error => {
            schemas.insert("ErrorEnvelope", schema_for!(ErrorEnvelope));
        }
    }

    schemas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_generation_is_json_serializable() {
        let schemas = build_schemas(SchemaTarget::All);
        for (name, schema) in schemas {
            let value = serde_json::to_value(&schema).expect("schema serializable");
            assert!(value.is_object(), "{name} schema should be a JSON object");
        }
    }
}
