use super::{report_auto_flush_failure, resolve_issue_id, retry_mutation_with_jsonl_recovery};
use crate::cli::CreateArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{format_type_label, sanitize_terminal_inline};
use crate::model::{Dependency, DependencyType, Issue, IssueType, Priority, Status};
use crate::output::OutputContext;
use crate::storage::{BulkDependencyInsert, EventAttribution, SqliteStorage};
use crate::util::id::{IdGenerationInput, IdGenerator, IdResolver, ResolverConfig, child_id};
use crate::util::markdown_import::{parse_dependency, parse_markdown_file};
use crate::util::time::parse_flexible_timestamp;
use crate::validation::{IssueValidator, LabelValidator};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

/// Configuration for creating an issue.
pub struct CreateConfig {
    pub id_config: crate::util::id::IdConfig,
    pub default_priority: Priority,
    pub default_issue_type: IssueType,
    pub actor: String,
    /// Stable repo identifier stamped onto new issues so cross-repo automation
    /// has a non-caller-relative anchor instead of a literal `.`. Falls back
    /// to `None` (storage default) when the beads directory has no usable
    /// parent name, e.g. `/.beads`.
    pub source_repo: Option<String>,
    /// Absolute canonical path of the source repository, populated alongside
    /// `source_repo` so fleet automation can disambiguate two clones of the
    /// same repo at different paths on the same machine (beads_rust#289).
    /// `None` when `canonicalize` of the beads-dir parent failed.
    pub source_repo_path: Option<String>,
}

/// Derive a stable `source_repo` value from the beads directory path: the
/// basename of the parent of `.beads/`, normalised. Returns `None` when no
/// useful name can be extracted, so the caller can let the legacy storage
/// default take over.
pub(crate) fn canonical_source_repo(beads_dir: &Path) -> Option<String> {
    let parent = beads_dir.parent()?;
    let parent = if parent.as_os_str().is_empty()
        && beads_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, ".beads" | "_beads"))
    {
        Path::new(".")
    } else if parent.as_os_str().is_empty() {
        return None;
    } else {
        parent
    };
    let canonical = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    let name = canonical.file_name()?.to_string_lossy().into_owned();
    if name.is_empty() || name == "." || name == "/" {
        None
    } else {
        Some(name)
    }
}

/// Derive the absolute canonical path of the source repository (the
/// parent of `.beads/`) for the `source_repo_path` field on `Issue`.
/// Distinct from [`canonical_source_repo`], which returns just the
/// basename. Used by fleet automation to disambiguate two clones of
/// the same repo at different paths on the same machine (see
/// beads_rust#289). Falls back to `None` if `canonicalize` fails —
/// the caller treats the field as optional and leaves it unset, which
/// matches the schema contract (`source_repo_path TEXT` nullable).
pub(crate) fn canonical_source_repo_path(beads_dir: &Path) -> Option<String> {
    let parent = beads_dir.parent()?;
    let parent = if parent.as_os_str().is_empty()
        && beads_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, ".beads" | "_beads"))
    {
        Path::new(".")
    } else if parent.as_os_str().is_empty() {
        return None;
    } else {
        parent
    };
    let canonical = parent.canonicalize().ok()?;
    let path_str = canonical.to_string_lossy().into_owned();
    if path_str.is_empty() {
        None
    } else {
        Some(path_str)
    }
}

struct NewIdInput<'a> {
    title: &'a str,
    description: Option<&'a str>,
    creator: Option<&'a str>,
    now: DateTime<Utc>,
    issue_count: usize,
    id_config: &'a crate::util::id::IdConfig,
}

enum ImportReferenceResolution {
    Resolved(String),
    Ambiguous(Vec<String>),
}

#[derive(Serialize)]
struct CreateWithCapacityWarnings<'a, T> {
    created: &'a T,
    warnings: &'a [crate::close_policy::WorkflowCapacityWarning],
}

/// Execute the create command.
///
/// # Errors
///
/// Returns an error if validation fails, the database cannot be opened, or the issue cannot be created.
#[allow(clippy::too_many_lines)]
pub fn execute(args: &CreateArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    execute_with_storage(args, cli, ctx, None)
}

/// Execute the create command, optionally reusing a pre-opened storage
/// connection. When `pre_opened` is `Some`, the caller's connection is used
/// directly, avoiding a redundant second open that would compete for the
/// WAL write lock under concurrent access.
#[allow(clippy::too_many_lines)]
pub fn execute_with_storage(
    args: &CreateArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    pre_opened: Option<config::OpenStorageResult>,
) -> Result<()> {
    if let Some(ref file_path) = args.file {
        if args.title.is_some() || args.title_flag.is_some() {
            return Err(BeadsError::validation(
                "file",
                "cannot be combined with title arguments",
            ));
        }
        if args.external_ref.is_some() {
            return Err(BeadsError::validation(
                "external_ref",
                "--external-ref is not supported with --file",
            ));
        }
        return execute_import(file_path, args, cli, ctx, pre_opened);
    }

    // 1. Open storage (reuse pre-opened if available)
    let mut storage_ctx = if let Some(ctx) = pre_opened {
        ctx
    } else {
        let beads_dir = config::discover_beads_dir_with_cli(cli)?;
        config::open_storage_with_cli(&beads_dir, cli)?
    };
    let layer = storage_ctx.load_config(cli)?;

    // Strict status-workflow enforcement (issue #311). Reject an out-of-set
    // `--status` before any write when the project configures
    // `workflow.strict: true`. No-op when the workflow section is absent.
    enforce_workflow_status(&storage_ctx.paths.beads_dir, args.status.as_deref())?;

    let config = CreateConfig {
        id_config: config::id_config_from_layer(&layer),
        default_priority: config::default_priority_from_layer(&layer)?,
        default_issue_type: config::default_issue_type_from_layer(&layer)?,
        actor: config::resolve_actor(&layer),
        source_repo: canonical_source_repo(&storage_ctx.paths.beads_dir),
        source_repo_path: canonical_source_repo_path(&storage_ctx.paths.beads_dir),
    };

    // Resolve the description up front — before the retry closure — so the
    // verbatim `--description-file` content (or `-` stdin, which can only be
    // read once) is captured a single time and reused across any
    // JSONL-recovery retry.
    let description = resolve_create_description(args)?;

    let issue =
        retry_mutation_with_jsonl_recovery(&mut storage_ctx, true, "create", None, |storage| {
            create_issue_impl(storage, args, &config, description.clone())
        })?;
    let capacity_warnings = storage_ctx.storage.take_capacity_warnings();
    let created_id = issue.id.clone();
    let last_touched_dir = storage_ctx.paths.beads_dir.clone();
    let update_last_touched_after_flush = storage_ctx.no_db;
    if !args.dry_run && !update_last_touched_after_flush {
        crate::util::set_last_touched_id(&last_touched_dir, &created_id);
    }
    storage_ctx.flush_no_db_if_dirty()?;
    if !args.dry_run && update_last_touched_after_flush {
        crate::util::set_last_touched_id(&last_touched_dir, &created_id);
    }

    // Output
    if args.silent {
        println!("{}", issue.id);
    } else if ctx.is_toon() {
        if args.dry_run {
            ctx.toon(&issue);
        } else {
            let full_issue = storage_ctx
                .storage
                .get_issue_for_export(&issue.id)?
                .ok_or_else(|| BeadsError::IssueNotFound {
                    id: issue.id.clone(),
                })?;
            emit_created_with_capacity_warnings(&full_issue, &capacity_warnings, ctx);
        }
    } else if ctx.is_json() {
        if args.dry_run {
            ctx.json_pretty(&issue);
        } else {
            let full_issue = storage_ctx
                .storage
                .get_issue_for_export(&issue.id)?
                .ok_or_else(|| BeadsError::IssueNotFound {
                    id: issue.id.clone(),
                })?;
            emit_created_with_capacity_warnings(&full_issue, &capacity_warnings, ctx);
        }
    } else if args.dry_run {
        ctx.info(&format!(
            "Dry run: would create issue {}",
            create_display_text(&issue.id)
        ));
        ctx.print_line(&format!(
            "Title: {}",
            sanitize_terminal_inline(&issue.title)
        ));
        ctx.print_line(&format!("Type: {}", format_type_label(&issue.issue_type)));
        ctx.print_line(&format!("Priority: {}", issue.priority));
        if !args.labels.is_empty() {
            let labels = args
                .labels
                .iter()
                .map(|label| sanitize_terminal_inline(label).into_owned())
                .collect::<Vec<_>>();
            ctx.print_line(&format!("Labels: {}", labels.join(", ")));
        }
        if let Some(parent) = &args.parent {
            ctx.print_line(&format!("Parent: {}", create_display_text(parent)));
        }
        if !args.deps.is_empty() {
            ctx.print_line(&format!(
                "Dependencies: {}",
                create_display_list(args.deps.iter().map(String::as_str))
            ));
        }
    } else {
        ctx.success(&format!(
            "Created {}: {}",
            create_display_text(&issue.id),
            sanitize_terminal_inline(&issue.title)
        ));
    }
    if !ctx.is_json() && !ctx.is_toon() {
        for warning in &capacity_warnings {
            ctx.warning(&warning.to_string());
        }
    }
    auto_flush_after_create(&mut storage_ctx, ctx);
    Ok(())
}

/// Enforce the project's strict status-workflow policy (issue #311) against a
/// caller-supplied `--status`. A `None` status (default `open`) is always
/// permitted: an empty allowed set already forbids enforcement, and a strict
/// set that omits `open` would otherwise make `br create` unusable. Returns
/// `Ok(())` when the workflow section is absent or non-strict.
///
/// The *effective* starting status is validated — the explicit `--status` when
/// given, otherwise the create default (`open`, matching the default applied
/// below). Validating the default too keeps `br create`, `br create --status
/// open`, and `br update --status open` consistent: if a strict workflow omits
/// the starting status, `br create` must name a valid one rather than silently
/// producing a bead whose status `br doctor` will immediately flag.
///
/// # Errors
///
/// Returns a validation error when strict enforcement is configured and the
/// effective status is not in the allowed set.
fn enforce_workflow_status(beads_dir: &Path, raw_status: Option<&str>) -> Result<()> {
    let policy = crate::close_policy::load_for_beads_dir(beads_dir)?;
    if !policy.workflow.is_enforced() && !policy.workflow.transitions_enforced() {
        return Ok(());
    }
    // Parse to the canonical string so a custom-cased config entry and the
    // canonical name (`In_Progress` vs `in_progress`) compare equal. When
    // `--status` is omitted the effective status is the create default, `Open`.
    let parsed: Status = match raw_status {
        Some(raw) => raw.parse()?,
        None => Status::Open,
    };
    // Status-set enforcement (issue #311).
    policy.workflow.validate_status(parsed.as_str())?;
    // Initial-transition enforcement (issue #312, layer 1): a create has no
    // prior status, so the effective starting status is validated against the
    // reserved `initial` key (no-op when `transitions`/`initial` is absent).
    policy.workflow.validate_transition(None, parsed.as_str())
}

fn auto_flush_after_create(storage_ctx: &mut config::OpenStorageResult, ctx: &OutputContext) {
    if let Err(error) = storage_ctx.auto_flush_if_enabled() {
        report_auto_flush_failure(
            ctx,
            &storage_ctx.paths.beads_dir,
            &storage_ctx.paths.jsonl_path,
            &error,
        );
    }
}

fn create_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

fn emit_created_with_capacity_warnings<T: Serialize>(
    created: &T,
    warnings: &[crate::close_policy::WorkflowCapacityWarning],
    ctx: &OutputContext,
) {
    if warnings.is_empty() {
        if ctx.is_toon() {
            ctx.toon(created);
        } else {
            ctx.json_pretty(created);
        }
    } else {
        let payload = CreateWithCapacityWarnings { created, warnings };
        if ctx.is_toon() {
            ctx.toon(&payload);
        } else {
            ctx.json_pretty(&payload);
        }
    }
}

fn create_display_list<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    values
        .into_iter()
        .map(create_display_text)
        .collect::<Vec<_>>()
        .join(", ")
}

fn create_display_path(path: &Path) -> String {
    create_display_text(&path.display().to_string())
}

fn create_issue_summary_line(id: &str, title: &str) -> String {
    format!(
        "  {}: {}",
        create_display_text(id),
        create_display_text(title)
    )
}

/// Read a description body verbatim from a file, or from stdin when `path`
/// is `-`. The full content is returned unchanged from disk (no paragraph
/// truncation, no trimming), so callers get exactly the bytes on disk as a
/// UTF-8 string. Shared by `br create --description-file` and
/// `br update --description-file`.
pub(crate) fn read_description_file(path: &Path) -> Result<String> {
    if path.as_os_str() == "-" {
        let mut buffer = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer).map_err(|err| {
            BeadsError::validation(
                "description_file",
                format!("failed to read description from stdin: {err}"),
            )
        })?;
        return Ok(buffer);
    }
    std::fs::read_to_string(path).map_err(|err| {
        BeadsError::validation(
            "description_file",
            format!(
                "failed to read description file '{}': {err}",
                path.display()
            ),
        )
    })
}

/// Resolve the effective description for `br create`, preferring
/// `--description-file` (read verbatim) over inline `-d/--description`.
/// The two are mutually exclusive; clap enforces this at parse time, and
/// this guard re-checks it so programmatic callers cannot smuggle both.
pub(crate) fn resolve_create_description(args: &CreateArgs) -> Result<Option<String>> {
    if let Some(path) = &args.description_file {
        if args.description.is_some() {
            return Err(BeadsError::validation(
                "description_file",
                "cannot be combined with --description",
            ));
        }
        return Ok(Some(read_description_file(path)?));
    }
    Ok(args.description.clone())
}

/// Core logic for creating an issue.
///
/// Handles ID generation, validation, and storage insertion.
/// Returns the constructed Issue.
///
/// # Errors
///
/// Returns error if:
/// - Title is empty
/// - ID generation fails
/// - Validation fails
/// - Storage write fails
#[allow(clippy::too_many_lines)]
pub fn create_issue_impl(
    storage: &mut SqliteStorage,
    args: &CreateArgs,
    config: &CreateConfig,
    description: Option<String>,
) -> Result<Issue> {
    // Real callers pass the pre-resolved `--description-file`/`-d` value
    // (resolved once at the call site so `-` stdin is read exactly once,
    // before the JSONL-recovery retry closure). When a caller passes `None`,
    // fall back to the description carried on `args` so arg-based descriptions
    // still apply. `description_file` conflicts with `-d`, so this never
    // double-counts.
    let description = description.or_else(|| args.description.clone());
    // 1. Resolve title
    let title = args
        .title
        .as_ref()
        .or(args.title_flag.as_ref())
        .ok_or_else(|| BeadsError::validation("title", "cannot be empty"))?;

    if title.is_empty() {
        return Err(BeadsError::validation("title", "cannot be empty"));
    }

    // 2. Parse fields
    let priority = if let Some(p) = &args.priority {
        Priority::from_str(p)?
    } else {
        config.default_priority
    };

    let issue_type = if let Some(t) = &args.type_ {
        IssueType::from_str(t)?
    } else {
        config.default_issue_type.clone()
    };

    let due_at = parse_optional_date(args.due.as_deref())?;
    let defer_until = parse_optional_date(args.defer.as_deref())?;
    let id_resolver = IdResolver::new(ResolverConfig::with_prefix(config.id_config.prefix.clone()));
    let resolved_parent = args
        .parent
        .as_deref()
        .map(|parent| resolve_issue_id(storage, &id_resolver, parent))
        .transpose()?;

    // Parse status (default to Open if not provided)
    let status = if let Some(s) = &args.status {
        Status::from_str(s)?
    } else {
        Status::Open
    };

    let count = storage.count_issues()?;
    let mut retries = 0;
    loop {
        // 2. Generate ID
        let now = Utc::now();

        let id_input = NewIdInput {
            title,
            description: description.as_deref(),
            creator: Some(&config.actor),
            now,
            issue_count: count,
            id_config: &config.id_config,
        };
        let id = generate_new_id(
            storage,
            resolved_parent.as_deref(),
            &id_input,
            args.slug.as_deref(),
        )?;

        // Set closed_at if status is Closed
        let closed_at = if matches!(status, Status::Closed) {
            Some(now)
        } else {
            None
        };

        let deleted_at = if matches!(status, Status::Tombstone) {
            Some(now)
        } else {
            None
        };

        // 4. Construct Issue
        let mut issue = Issue {
            id: id.clone(),
            title: title.clone(),
            description: description.clone(),
            status: status.clone(),
            priority,
            issue_type: issue_type.clone(),
            created_at: now,
            updated_at: now,
            assignee: args.assignee.clone(),
            owner: args.owner.clone(),
            estimated_minutes: args.estimate,
            due_at,
            defer_until,
            external_ref: args.external_ref.clone(),
            ephemeral: args.ephemeral,
            // Defaults
            content_hash: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            created_by: Some(config.actor.clone()),
            closed_at,
            close_reason: None,
            closed_by_session: None,
            source_system: None,
            source_repo: config.source_repo.clone(),
            source_repo_path: config.source_repo_path.clone(),
            agent_context: None,
            deleted_at,
            deleted_by: if deleted_at.is_some() {
                Some(config.actor.clone())
            } else {
                None
            },
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };

        // Compute content hash
        issue.content_hash = Some(issue.compute_content_hash());

        // 5. Validate Issue
        IssueValidator::validate(&issue).map_err(BeadsError::from_validation_errors)?;

        // 5b. Validate Relations (fail fast before DB writes)
        validate_relations(args, &id)?;

        // 6. Populate Relations (labels & dependencies)
        let relation_context = RelationContext {
            actor: &config.actor,
            now,
            resolved_parent: resolved_parent.as_deref(),
            storage,
            prefix: &config.id_config.prefix,
        };
        populate_relations(&mut issue, args, &relation_context)?;
        // 7. Dry Run check - return early
        if args.dry_run {
            return Ok(issue);
        }

        // Stage Tier 1 attribution (issue #312, Layer 3 capture-only) so the
        // creation audit event records the self-reported agent identity. This
        // is recorded ONLY — it never gates or alters the create. Re-staged on
        // every loop iteration because `mutate()` consumes it on each attempt.
        storage.set_pending_event_attribution(EventAttribution::new(
            args.agent_name.as_deref(),
            args.harness.as_deref(),
            args.model.as_deref(),
        ));

        // 8. Create (atomic)
        match storage.create_issue(&issue, &config.actor) {
            Ok(()) => return Ok(issue),
            Err(BeadsError::IdCollision { .. }) => {
                if retries >= 10 {
                    return Err(BeadsError::IdCollision { id });
                }
                retries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10 * retries));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Generate a new ID, supporting both hierarchical and hash-based formats.
///
/// When `slug` is `Some(non-empty)` and the issue is non-hierarchical, the
/// resulting ID embeds the normalized slug between the prefix and the hash:
/// `<prefix>-<slug>-<hash>`. Hierarchical (parent-anchored) IDs ignore the
/// slug — child IDs use the parent ID + child number scheme and have no slug
/// segment. An empty / non-`Some` slug falls back to the historical
/// hash-only behavior.
fn generate_new_id(
    storage: &SqliteStorage,
    parent_id: Option<&str>,
    input: &NewIdInput<'_>,
    slug: Option<&str>,
) -> Result<String> {
    if let Some(parent_id) = parent_id {
        // Verify parent exists
        if !storage.id_exists(parent_id)? {
            return Err(BeadsError::IssueNotFound {
                id: parent_id.to_string(),
            });
        }

        // Find next available child number
        let next_num = storage.next_child_number(parent_id)?;
        let candidate = child_id(parent_id, next_num);

        // Double-check the ID doesn't exist (race condition safety)
        if storage.id_exists(&candidate)? {
            // Extremely unlikely, but handle by incrementing
            let mut num = next_num + 1;
            loop {
                let alt = child_id(parent_id, num);
                if !storage.id_exists(&alt)? {
                    return Ok(alt);
                }
                num += 1;
                if num > next_num.saturating_add(100) {
                    return Err(BeadsError::validation(
                        "parent",
                        "could not find available child ID",
                    ));
                }
            }
        }

        Ok(candidate)
    } else {
        // Standard ID generation for non-child issues
        let id_gen = IdGenerator::new(input.id_config.clone());
        match slug {
            Some(s) if !s.trim().is_empty() => id_gen.generate_with_slug(
                IdGenerationInput {
                    title: input.title,
                    description: input.description,
                    creator: input.creator,
                    created_at: input.now,
                    issue_count: input.issue_count,
                },
                s,
                |id| storage.id_exists(id),
            ),
            _ => id_gen.generate(
                input.title,
                input.description,
                input.creator,
                input.now,
                input.issue_count,
                |id| storage.id_exists(id),
            ),
        }
    }
}

fn resolve_dependency_id(
    resolver: &IdResolver,
    storage: &SqliteStorage,
    input: &str,
) -> Result<String> {
    if input.starts_with("external:") {
        return Ok(input.to_string());
    }

    match resolve_issue_id(storage, resolver, input) {
        Ok(id) => Ok(id),
        Err(id_error) => match storage.find_ids_by_exact_title(input)?.as_slice() {
            [] => Err(id_error),
            [id] => Ok(id.clone()),
            ids => Err(BeadsError::validation(
                "deps",
                format!(
                    "dependency title '{}' matches multiple issues: {}",
                    input.trim(),
                    ids.join(", ")
                ),
            )),
        },
    }
}

fn validate_relations(args: &CreateArgs, issue_id: &str) -> Result<()> {
    // Validate Labels
    for label in &args.labels {
        let trimmed = label.trim();
        if !trimmed.is_empty() {
            LabelValidator::validate(trimmed)
                .map_err(|e| BeadsError::validation("label", e.message))?;
        }
    }

    // Validate Parent
    if let Some(parent_id) = &args.parent
        && parent_id == issue_id
    {
        return Err(BeadsError::validation(
            "parent",
            "cannot be parent of itself",
        ));
    }

    // Validate Dependencies
    for dep_str in &args.deps {
        let (_, dep_id) = parse_create_dependency(dep_str)?;

        if dep_id == issue_id {
            return Err(BeadsError::validation("deps", "cannot depend on itself"));
        }
    }

    Ok(())
}

fn parse_create_dependency(dep_str: &str) -> Result<(DependencyType, String)> {
    // Match markdown import semantics: a colon only means `type:id` when the
    // prefix is a known dependency type; otherwise it can be part of a title.
    let (mut type_str, dep_id, valid) = parse_dependency(dep_str);
    if !valid {
        return Err(BeadsError::Validation {
            field: "deps".to_string(),
            reason: format!(
                "Unknown dependency type: '{type_str}'. \
                 Allowed types: blocks, blocked-by, parent-child, conditional-blocks, waits-for, \
                 related, discovered-from, replies-to, relates-to, duplicates, \
                 supersedes, caused-by"
            ),
        });
    }

    if type_str.eq_ignore_ascii_case("blocked-by") {
        type_str = "blocks".to_string();
    }

    let dep_type = DependencyType::from_str(&type_str)?;
    if let DependencyType::Custom(_) = dep_type {
        return Err(BeadsError::Validation {
            field: "deps".to_string(),
            reason: format!(
                "Unknown dependency type: '{type_str}'. \
                 Allowed types: blocks, blocked-by, parent-child, conditional-blocks, waits-for, \
                 related, discovered-from, replies-to, relates-to, duplicates, \
                 supersedes, caused-by"
            ),
        });
    }

    Ok((dep_type, dep_id))
}

struct RelationContext<'a> {
    actor: &'a str,
    now: DateTime<Utc>,
    resolved_parent: Option<&'a str>,
    storage: &'a crate::storage::SqliteStorage,
    prefix: &'a str,
}

fn populate_relations(
    issue: &mut Issue,
    args: &CreateArgs,
    ctx: &RelationContext<'_>,
) -> Result<()> {
    let resolver = IdResolver::new(ResolverConfig::with_prefix(ctx.prefix.to_string()));

    // Labels
    for label in &args.labels {
        let label = label.trim();
        if !label.is_empty() && !issue.labels.iter().any(|existing| existing == label) {
            issue.labels.push(label.to_string());
        }
    }

    // Parent
    if let Some(parent_id) = ctx.resolved_parent {
        issue.dependencies.push(Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: parent_id.to_string(),
            dep_type: DependencyType::ParentChild,
            created_at: ctx.now,
            created_by: Some(ctx.actor.to_string()),
            metadata: None,
            thread_id: None,
        });
    }

    // Dependencies
    for dep_str in &args.deps {
        let (dep_type, dep_id) = parse_create_dependency(dep_str)?;
        let resolved_dep_id = resolve_dependency_id(&resolver, ctx.storage, &dep_id)?;

        issue.dependencies.push(Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: resolved_dep_id,
            dep_type,
            created_at: ctx.now,
            created_by: Some(ctx.actor.to_string()),
            metadata: None,
            thread_id: None,
        });
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_import(
    path: &Path,
    args: &CreateArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    pre_opened: Option<config::OpenStorageResult>,
) -> Result<()> {
    let parsed_issues = parse_markdown_file(path)?;
    if parsed_issues.is_empty() {
        let empty_issues = Vec::<Issue>::new();
        if ctx.is_json() {
            ctx.json(&empty_issues);
        } else if ctx.is_toon() {
            ctx.toon(&empty_issues);
        }
        return Ok(());
    }

    let mut storage_ctx = if let Some(ctx) = pre_opened {
        ctx
    } else {
        let beads_dir = config::discover_beads_dir_with_cli(cli)?;
        config::open_storage_with_cli(&beads_dir, cli)?
    };
    let layer = storage_ctx.load_config(cli)?;

    // Strict status-workflow enforcement (issue #311); see `execute`.
    enforce_workflow_status(&storage_ctx.paths.beads_dir, args.status.as_deref())?;

    let id_config = config::id_config_from_layer(&layer);
    let default_priority = config::default_priority_from_layer(&layer)?;
    let default_issue_type = config::default_issue_type_from_layer(&layer)?;
    let actor = config::resolve_actor(&layer);
    let import_source_repo = canonical_source_repo(&storage_ctx.paths.beads_dir);
    let import_source_repo_path = canonical_source_repo_path(&storage_ctx.paths.beads_dir);
    let now = Utc::now();
    let _json_mode = cli.json.unwrap_or(false);
    let due_at = parse_optional_date(args.due.as_deref())?;
    let defer_until = parse_optional_date(args.defer.as_deref())?;

    // Parse status (default to Open if not provided)
    let import_status = if let Some(s) = &args.status {
        Status::from_str(s)?
    } else {
        Status::Open
    };

    // Set closed_at if status is Closed
    let import_closed_at = if matches!(import_status, Status::Closed) {
        Some(now)
    } else {
        None
    };

    let import_deleted_at = if matches!(import_status, Status::Tombstone) {
        Some(now)
    } else {
        None
    };

    let storage = &mut storage_ctx.storage;
    let mut count = storage.count_issues()?;
    let mut last_created_id: Option<String> = None;

    let mut created_ids = Vec::new();
    let mut created_issues = Vec::new();
    let mut capacity_warnings = Vec::new();

    let id_resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix.clone()));

    // Phase 1: Create all issues, deferring intra-file dependency resolution.
    // Maps for resolving symbolic references between issues in the same import.
    let mut title_to_ids: HashMap<String, Vec<String>> = HashMap::new();
    let mut standin_to_ids: HashMap<String, Vec<String>> = HashMap::new();
    // Deferred deps: (issue_id, raw_dep_strings, dep_types_from_cli)
    let mut deferred_deps: Vec<(String, Vec<String>)> = Vec::new();
    let mut deferred_parent_deps: Vec<(String, String)> = Vec::new();
    let import_title_keys: HashSet<String> = parsed_issues
        .iter()
        .map(|issue| issue.title.trim().to_lowercase())
        .filter(|title| !title.is_empty())
        .collect();
    let duplicate_import_title_keys = duplicate_import_keys(
        parsed_issues
            .iter()
            .map(|issue| issue.title.trim().to_lowercase()),
    );
    let import_standin_keys: HashSet<String> = parsed_issues
        .iter()
        .filter_map(|issue| {
            let id = issue.stand_in_id.as_ref()?.trim().to_lowercase();
            (!id.is_empty()).then_some(id)
        })
        .collect();
    let duplicate_import_standin_keys =
        duplicate_import_keys(parsed_issues.iter().filter_map(|issue| {
            let id = issue.stand_in_id.as_ref()?.trim().to_lowercase();
            (!id.is_empty()).then_some(id)
        }));

    for parsed in parsed_issues {
        let title = parsed.title.trim().to_string();
        if title.is_empty() {
            eprintln!("✗ Failed to create issue: title cannot be empty");
            continue;
        }
        let description = parsed.description.clone();
        let priority_override = parsed.priority.clone();
        let issue_type_override = parsed.issue_type.clone();
        let assignee = parsed.assignee.or_else(|| args.assignee.clone());
        let design = parsed.design.clone();
        let acceptance_criteria = parsed.acceptance_criteria.clone();
        let agent_context = parsed.agent_context.clone();

        // Resolve parent (item-specific header or CLI global fallback)
        let parent_candidate = parsed.parent.as_deref().or(args.parent.as_deref());
        let mut deferred_parent_ref = None;
        let resolved_parent: Option<String> = if let Some(p) = parent_candidate {
            let p_trimmed = p.trim();
            let p_lower = p_trimmed.to_lowercase();
            if parsed.parent.is_some()
                && (duplicate_import_standin_keys.contains(&p_lower)
                    || duplicate_import_title_keys.contains(&p_lower))
            {
                deferred_parent_ref = Some(p_trimmed.to_string());
                None
            } else if let Some(ImportReferenceResolution::Resolved(id)) =
                lookup_import_reference(&standin_to_ids, &title_to_ids, p_trimmed)
            {
                Some(id)
            } else if parsed.parent.is_some()
                && (import_standin_keys.contains(&p_lower) || import_title_keys.contains(&p_lower))
            {
                deferred_parent_ref = Some(p_trimmed.to_string());
                None
            } else {
                match resolve_issue_id(storage, &id_resolver, p) {
                    Ok(id) => Some(id),
                    Err(err) => {
                        eprintln!(
                            "✗ Failed to resolve parent for {}: {}",
                            create_display_text(&title),
                            err
                        );
                        continue;
                    }
                }
            }
        } else {
            None
        };

        let mut retries = 0;
        let mut final_id = String::new();
        let mut created = false;

        loop {
            let id_input = NewIdInput {
                title: &title,
                description: description.as_deref(),
                creator: None,
                now,
                issue_count: count,
                id_config: &id_config,
            };
            let id = match generate_new_id(storage, resolved_parent.as_deref(), &id_input, None) {
                Ok(id) => id,
                Err(err) => {
                    eprintln!("✗ Failed to create {}: {err}", create_display_text(&title));
                    break;
                }
            };

            let priority = if let Some(ref p) = priority_override {
                match Priority::from_str(p) {
                    Ok(value) => value,
                    Err(err) => {
                        eprintln!("✗ Failed to create {}: {err}", create_display_text(&title));
                        break;
                    }
                }
            } else {
                default_priority
            };

            let issue_type = if let Some(ref t) = issue_type_override {
                match IssueType::from_str(t) {
                    Ok(value) => value,
                    Err(err) => {
                        eprintln!("✗ Failed to create {}: {err}", create_display_text(&title));
                        break;
                    }
                }
            } else {
                default_issue_type.clone()
            };

            let mut issue = Issue {
                id: id.clone(),
                title: title.clone(),
                description: description.clone(),
                status: import_status.clone(),
                priority,
                issue_type,
                created_at: now,
                updated_at: now,
                assignee: assignee.clone(),
                owner: args.owner.clone(),
                estimated_minutes: args.estimate,
                due_at,
                defer_until,
                external_ref: args.external_ref.clone(),
                ephemeral: args.ephemeral,
                design: design.clone(),
                acceptance_criteria: acceptance_criteria.clone(),
                content_hash: None,
                notes: None,
                // Keep import hashes actor-independent so identical markdown imports
                // still deduplicate across sync boundaries.
                created_by: None,
                closed_at: import_closed_at,
                close_reason: None,
                closed_by_session: None,
                source_system: None,
                source_repo: import_source_repo.clone(),
                source_repo_path: import_source_repo_path.clone(),
                agent_context: agent_context.clone(),
                deleted_at: import_deleted_at,
                deleted_by: if import_deleted_at.is_some() {
                    Some(actor.clone())
                } else {
                    None
                },
                delete_reason: None,
                original_type: None,
                compaction_level: None,
                compacted_at: None,
                compacted_at_commit: None,
                original_size: None,
                sender: None,
                pinned: false,
                is_template: false,
                labels: vec![],
                dependencies: vec![],
                comments: vec![],
            };

            issue.content_hash = Some(issue.compute_content_hash());
            if let Err(err) =
                IssueValidator::validate(&issue).map_err(BeadsError::from_validation_errors)
            {
                eprintln!("✗ Failed to create {}: {err}", create_display_text(&title));
                break;
            }

            // Populate Labels (with validation)
            let mut labels = parsed.labels.clone();
            labels.extend(args.labels.clone());
            for label in labels {
                let label = label.trim().to_string();
                if label.is_empty() {
                    continue;
                }
                if let Err(err) = LabelValidator::validate(&label) {
                    eprintln!(
                        "warning: skipping invalid label '{}' for issue {}: {}",
                        create_display_text(&label),
                        create_display_text(&id),
                        err.message
                    );
                    continue;
                }
                if !issue.labels.iter().any(|existing| existing == &label) {
                    issue.labels.push(label);
                }
            }

            // Parent dependency is wired inline (parent must pre-exist or be CLI-provided).
            if let Some(parent_id) = resolved_parent.as_deref() {
                issue.dependencies.push(Dependency {
                    issue_id: id.clone(),
                    depends_on_id: parent_id.to_string(),
                    dep_type: DependencyType::ParentChild,
                    created_at: now,
                    created_by: Some(actor.clone()),
                    metadata: None,
                    thread_id: None,
                });
            }

            if args.dry_run {
                // Skip persistence: dry-run validates the bulk file and reports what
                // would be created without writing to storage or JSONL.
                created_issues.push(issue.clone());
                final_id = id;
                created = true;
                break;
            }
            // Stage Tier 1 attribution (issue #312, Layer 3 capture-only) for
            // the bulk-import creation audit event. Recorded only; never gated.
            storage.set_pending_event_attribution(EventAttribution::new(
                args.agent_name.as_deref(),
                args.harness.as_deref(),
                args.model.as_deref(),
            ));
            match storage.create_issue(&issue, &actor) {
                Ok(()) => {
                    capacity_warnings.extend(storage.take_capacity_warnings());
                    final_id = id;
                    created = true;
                    break;
                }
                Err(BeadsError::IdCollision { .. }) => {
                    if retries >= 10 {
                        eprintln!(
                            "✗ Failed to create {}: ID collision after 10 retries",
                            create_display_text(&title)
                        );
                        break;
                    }
                    retries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10 * retries));
                }
                Err(err) => {
                    eprintln!("✗ Failed to create {}: {err}", create_display_text(&title));
                    break;
                }
            }
        }

        if !created {
            continue;
        }
        let id = final_id;

        if let Some(parent_ref) = deferred_parent_ref {
            deferred_parent_deps.push((id.clone(), parent_ref));
        }

        // Collect dependencies for deferred resolution (Phase 2).
        // Must be OUTSIDE the retry loop so we only record the final (non-colliding) ID.
        let mut deps = parsed.dependencies.clone();
        deps.extend(args.deps.clone());
        if !deps.is_empty() {
            deferred_deps.push((id.clone(), deps));
        }

        // Register this issue for intra-file dependency resolution.
        title_to_ids
            .entry(title.to_lowercase())
            .or_default()
            .push(id.clone());
        if let Some(ref sid) = parsed.stand_in_id {
            let sid_trimmed = sid.trim().to_string();
            if !sid_trimmed.is_empty() {
                // Case-insensitive, consistent with title-based resolution.
                standin_to_ids
                    .entry(sid_trimmed.to_lowercase())
                    .or_default()
                    .push(id.clone());
            }
        }

        // Increment count for next ID generation in the loop
        count += 1;
        last_created_id = Some(id.clone());
        created_ids.push((id, title));
    }

    // Phase 2: Resolve and wire up deferred dependencies.
    // Now that all issues exist in storage, we can resolve intra-file references
    // by title or stand-in ID, as well as references to pre-existing issues.
    let mut resolved_import_deps = Vec::new();
    // #368: count declared edges we fail to resolve so the command can exit
    // non-zero for scripted callers instead of silently reporting success.
    let mut dropped_import_edges = 0usize;
    if !deferred_parent_deps.is_empty() && !args.dry_run {
        for (issue_id, parent_ref) in &deferred_parent_deps {
            let parent_id =
                match lookup_import_reference(&standin_to_ids, &title_to_ids, parent_ref) {
                    Some(ImportReferenceResolution::Resolved(parent_id)) => parent_id,
                    Some(ImportReferenceResolution::Ambiguous(ids)) => {
                        warn_ambiguous_import_reference("parent", parent_ref, issue_id, &ids);
                        continue;
                    }
                    None => {
                        eprintln!(
                            "warning: unresolved parent '{}' for issue {}",
                            create_display_text(parent_ref),
                            create_display_text(issue_id)
                        );
                        dropped_import_edges += 1;
                        continue;
                    }
                };
            if parent_id == *issue_id {
                eprintln!(
                    "warning: skipping self-parent for issue {}",
                    create_display_text(issue_id)
                );
                continue;
            }
            resolved_import_deps.push(BulkDependencyInsert {
                issue_id: issue_id.clone(),
                depends_on_id: parent_id,
                dep_type: "parent-child".to_string(),
            });
        }
    }

    if !deferred_deps.is_empty() && !args.dry_run {
        let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix.clone()));

        for (issue_id, deps) in &deferred_deps {
            for dep_str in deps {
                // First, check the raw string against intra-file maps before parsing.
                // This handles titles containing colons (e.g., "Step 1: Setup Database")
                // that would otherwise be misinterpreted as typed dependencies.
                let (type_str, resolved_dep_id) = if let Some(import_ref) =
                    lookup_import_reference(&standin_to_ids, &title_to_ids, dep_str)
                {
                    match import_ref {
                        ImportReferenceResolution::Resolved(id) => ("blocks".to_string(), id),
                        ImportReferenceResolution::Ambiguous(ids) => {
                            warn_ambiguous_import_reference("dependency", dep_str, issue_id, &ids);
                            continue;
                        }
                    }
                } else {
                    // No raw match — parse as type:id or bare id.
                    let (mut t, dep_id, valid) = parse_dependency(dep_str);
                    if !valid {
                        eprintln!(
                            "warning: skipping invalid dependency type '{}' for issue {}",
                            create_display_text(&t),
                            create_display_text(issue_id)
                        );
                        continue;
                    }
                    if t.eq_ignore_ascii_case("blocked-by") {
                        t = "blocks".to_string();
                    }

                    // Resolution order: stand-in ID → title → storage ID
                    // All intra-file lookups are case-insensitive.
                    let resolved = if let Some(import_ref) =
                        lookup_import_reference(&standin_to_ids, &title_to_ids, &dep_id)
                    {
                        match import_ref {
                            ImportReferenceResolution::Resolved(id) => id,
                            ImportReferenceResolution::Ambiguous(ids) => {
                                warn_ambiguous_import_reference(
                                    "dependency",
                                    &dep_id,
                                    issue_id,
                                    &ids,
                                );
                                continue;
                            }
                        }
                    } else {
                        match resolve_dependency_id(&resolver, storage, &dep_id) {
                            Ok(r) => r,
                            Err(err) => {
                                eprintln!(
                                    "warning: unresolved dependency '{}' for issue {}: {err}",
                                    create_display_text(&dep_id),
                                    create_display_text(issue_id)
                                );
                                dropped_import_edges += 1;
                                continue;
                            }
                        }
                    };
                    (t, resolved)
                };

                if resolved_dep_id == *issue_id {
                    eprintln!(
                        "warning: skipping self-dependency for issue {}",
                        create_display_text(issue_id)
                    );
                    continue;
                }
                if is_marker_only_dependency(&resolved_dep_id) {
                    eprintln!(
                        "warning: skipping invalid dependency '{}' for issue {}",
                        create_display_text(&resolved_dep_id),
                        create_display_text(issue_id)
                    );
                    continue;
                }

                resolved_import_deps.push(BulkDependencyInsert {
                    issue_id: issue_id.clone(),
                    depends_on_id: resolved_dep_id,
                    dep_type: type_str,
                });
            }
        }
    }

    if !resolved_import_deps.is_empty() && !args.dry_run {
        match storage.add_dependencies_bulk_for_import(&resolved_import_deps, &actor) {
            Ok(_) => {}
            Err(err) => {
                eprintln!(
                    "warning: bulk dependency import failed ({err}); retrying dependencies one by one"
                );
                let mut dropped_edges = 0usize;
                for dep in &resolved_import_deps {
                    if let Err(err) = storage.add_dependency(
                        &dep.issue_id,
                        &dep.depends_on_id,
                        &dep.dep_type,
                        &actor,
                    ) {
                        dropped_edges += 1;
                        eprintln!(
                            "warning: failed to add dependency {} → {}: {err}",
                            create_display_text(&dep.issue_id),
                            create_display_text(&dep.depends_on_id)
                        );
                    }
                }
                // #368: The transactional bulk insert detected a cycle (or other
                // constraint) and the per-edge fallback then silently dropped the
                // offending edge(s). The issues are still created and the graph is
                // persisted, but it differs from the declared spec — surface that
                // to scripted callers via a non-zero exit code once output and
                // auto-flush have completed, rather than reporting success.
                if dropped_edges > 0 {
                    crate::output::record_pending_exit_code(
                        crate::error::ErrorCode::CycleDetected.exit_code(),
                    );
                }
            }
        }
    }

    // #368: declared dependency edges that couldn't be resolved during Phase 2
    // (a referenced parent/dependency that doesn't exist) were dropped above
    // with only an eprintln warning. The issues are still created, but the
    // persisted graph is missing intended edges — surface that to scripted
    // callers via a non-zero exit code rather than reporting a false success.
    if dropped_import_edges > 0 {
        crate::output::record_pending_exit_code(
            crate::error::ErrorCode::DependencyNotFound.exit_code(),
        );
    }

    if ctx.is_json() || ctx.is_toon() {
        if args.dry_run {
            // In dry run, we already populated created_issues in the loop
        } else {
            let ids: Vec<String> = created_ids.iter().map(|(id, _)| id.clone()).collect();
            match storage.get_issues_for_export(&ids) {
                Ok(issues) => {
                    // Ensure output order matches creation order
                    let mut issues_by_id: std::collections::HashMap<String, crate::model::Issue> =
                        issues.into_iter().map(|i| (i.id.clone(), i)).collect();
                    for (id, _) in &created_ids {
                        if let Some(issue) = issues_by_id.remove(id) {
                            created_issues.push(issue);
                        } else {
                            eprintln!(
                                "warning: could not load created issue {} for JSON output",
                                create_display_text(id)
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("warning: could not load created issues for JSON output: {e}");
                }
            }
        }
    }

    let last_touched_dir = storage_ctx.paths.beads_dir.clone();
    let update_last_touched_after_flush = storage_ctx.no_db;
    if !update_last_touched_after_flush && let Some(last_created_id) = last_created_id.as_deref() {
        crate::util::set_last_touched_id(&last_touched_dir, last_created_id);
    }
    storage_ctx.flush_no_db_if_dirty()?;
    if update_last_touched_after_flush && let Some(last_created_id) = last_created_id.as_deref() {
        crate::util::set_last_touched_id(&last_touched_dir, last_created_id);
    }

    if created_ids.is_empty() {
        return Err(BeadsError::NothingToDo {
            reason: format!(
                "failed to create any issues from {}",
                create_display_path(path)
            ),
        });
    }

    if args.silent {
        for (id, _) in created_ids {
            println!("{id}");
        }
    } else if ctx.is_toon() || ctx.is_json() {
        emit_created_with_capacity_warnings(&created_issues, &capacity_warnings, ctx);
    } else if !created_ids.is_empty() {
        if args.dry_run {
            ctx.info(&format!(
                "Dry run: would create {} issues from {}:",
                created_ids.len(),
                create_display_path(path)
            ));
        } else {
            ctx.success(&format!(
                "Created {} issues from {}:",
                created_ids.len(),
                create_display_path(path)
            ));
        }
        for (id, title) in created_ids {
            ctx.print_line(&create_issue_summary_line(&id, &title));
        }
    }
    if !ctx.is_json() && !ctx.is_toon() {
        for warning in &capacity_warnings {
            ctx.warning(&warning.to_string());
        }
    }
    auto_flush_after_create(&mut storage_ctx, ctx);
    Ok(())
}

fn duplicate_import_keys(keys: impl IntoIterator<Item = String>) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut duplicates = HashSet::new();
    for key in keys {
        if key.is_empty() {
            continue;
        }
        if !seen.insert(key.clone()) {
            duplicates.insert(key);
        }
    }
    duplicates
}

fn lookup_import_reference(
    standin_to_ids: &HashMap<String, Vec<String>>,
    title_to_ids: &HashMap<String, Vec<String>>,
    reference: &str,
) -> Option<ImportReferenceResolution> {
    let key = reference.trim().to_lowercase();
    if key.is_empty() {
        return None;
    }

    standin_to_ids
        .get(&key)
        .or_else(|| title_to_ids.get(&key))
        .map(|ids| match ids.as_slice() {
            [id] => ImportReferenceResolution::Resolved(id.clone()),
            _ => ImportReferenceResolution::Ambiguous(ids.clone()),
        })
}

fn warn_ambiguous_import_reference(kind: &str, reference: &str, issue_id: &str, ids: &[String]) {
    eprintln!(
        "warning: ambiguous {} '{}' for issue {} matches multiple imported issues: {}",
        create_display_text(kind),
        create_display_text(reference),
        create_display_text(issue_id),
        create_display_list(ids.iter().map(String::as_str))
    );
}

fn is_marker_only_dependency(dep_id: &str) -> bool {
    matches!(dep_id.trim(), "-" | "*" | "+")
}

fn parse_optional_date(s: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    match s {
        Some(s) if !s.trim().is_empty() => parse_flexible_timestamp(s, "date").map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::init_test_logging;
    use crate::util::id::IdConfig;
    use chrono::Datelike;
    use tracing::info;

    // Helper to create basic args
    fn default_args() -> CreateArgs {
        CreateArgs {
            title: Some("Test Issue".to_string()),
            title_flag: None,
            type_: None,
            slug: None,
            priority: None,
            description: None,
            description_file: None,
            assignee: None,
            owner: None,
            labels: vec![],
            parent: None,
            deps: vec![],
            estimate: None,
            due: None,
            defer: None,
            external_ref: None,
            status: None,
            ephemeral: false,
            dry_run: false,
            silent: false,
            file: None,
            agent_name: None,
            harness: None,
            model: None,
        }
    }

    fn default_config() -> CreateConfig {
        CreateConfig {
            id_config: IdConfig {
                prefix: "bd".to_string(),
                min_hash_length: 3,
                max_hash_length: 8,
                max_collision_prob: 0.25,
            },
            default_priority: Priority::MEDIUM,
            default_issue_type: IssueType::Task,
            actor: "test_user".to_string(),
            source_repo: None,
            source_repo_path: None,
        }
    }

    #[test]
    fn canonical_source_repo_uses_repo_basename() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo_root = temp.path().join("widget_engine");
        let beads_dir = repo_root.join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("create .beads");
        assert_eq!(
            canonical_source_repo(&beads_dir).as_deref(),
            Some("widget_engine"),
        );
    }

    #[test]
    fn canonical_source_repo_uses_cwd_basename_for_relative_beads_dir() {
        let expected = std::env::current_dir()
            .expect("current dir")
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        assert_eq!(canonical_source_repo(Path::new(".beads")), expected.clone());
        assert_eq!(canonical_source_repo(Path::new("_beads")), expected);
    }

    #[test]
    fn canonical_source_repo_returns_none_for_pathological_locations() {
        // The empty path has no parent, so we cannot derive a name.
        assert!(canonical_source_repo(Path::new("")).is_none());
        // Filesystem root has a parent of "/" with no file_name component.
        assert!(canonical_source_repo(Path::new("/.beads")).is_none());
    }

    #[test]
    fn canonical_source_repo_creates_issue_with_repo_owner_value() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo_root = temp.path().join("source_repo_probe");
        let beads_dir = repo_root.join(".beads");
        std::fs::create_dir_all(&beads_dir).expect("create .beads");

        let mut storage = setup_memory_storage();
        let mut config = default_config();
        config.source_repo = canonical_source_repo(&beads_dir);
        let args = default_args();

        let issue = create_issue_impl(&mut storage, &args, &config, None).expect("create");
        assert_eq!(
            issue.source_repo.as_deref(),
            Some("source_repo_probe"),
            "new issues must store the canonical repo basename, not '.'",
        );
    }

    fn setup_memory_storage() -> SqliteStorage {
        SqliteStorage::open_memory().expect("failed to open memory db")
    }

    #[test]
    fn create_human_display_helpers_escape_terminal_controls() {
        let id = create_display_text("bd-1\x1b]52;c;bad\x07");
        let deps = create_display_list(["bd-a\x1b[2J", "external:proj:cap\x07"]);
        let path = create_display_path(Path::new("imports\x1b[31m.md"));
        let line = create_issue_summary_line("bd-2\x07", "Imported\nTitle");

        for rendered in [&id, &deps, &path, &line] {
            assert!(
                !rendered.chars().any(char::is_control),
                "display helper leaked control characters: {rendered:?}"
            );
        }
        assert_eq!(id, "bd-1\\u{1b}]52;c;bad\\u{7}");
        assert_eq!(deps, "bd-a\\u{1b}[2J, external:proj:cap\\u{7}");
        assert_eq!(path, "imports\\u{1b}[31m.md");
        assert_eq!(line, "  bd-2\\u{7}: Imported\\nTitle");
    }

    #[test]
    fn test_create_issue_basic_success() {
        init_test_logging();
        info!("test_create_issue_basic_success: starting");
        let mut storage = setup_memory_storage();
        let args = default_args();
        let config = default_config();

        let issue = create_issue_impl(&mut storage, &args, &config, None).expect("create failed");

        assert_eq!(issue.title, "Test Issue");
        assert_eq!(issue.priority, Priority::MEDIUM);
        assert_eq!(issue.issue_type, IssueType::Task);
        assert!(issue.id.starts_with("bd-"));

        // Verify persisted
        let loaded = storage.get_issue(&issue.id).expect("get issue");
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().title, "Test Issue");
        info!("test_create_issue_basic_success: assertions passed");
    }

    #[test]
    fn test_create_issue_validation_empty_title() {
        init_test_logging();
        info!("test_create_issue_validation_empty_title: starting");
        let mut storage = setup_memory_storage();
        let mut args = default_args();
        args.title = None;
        let config = default_config();

        let err = create_issue_impl(&mut storage, &args, &config, None).unwrap_err();
        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "title"));
        info!("test_create_issue_validation_empty_title: assertions passed");
    }

    #[test]
    fn test_create_issue_with_parent_propagates_storage_error() {
        init_test_logging();
        info!("test_create_issue_with_parent_propagates_storage_error: starting");
        let mut storage = setup_memory_storage();
        let mut parent = default_args();
        parent.title = Some("Parent".to_string());
        let config = default_config();
        let created_parent =
            create_issue_impl(&mut storage, &parent, &config, None).expect("create parent");

        storage
            .execute_raw("DROP TABLE issues")
            .expect("drop issues");

        let mut child = default_args();
        child.parent = Some(created_parent.id);

        let err = create_issue_impl(&mut storage, &child, &config, None).unwrap_err();
        assert!(
            matches!(err, BeadsError::Database(_)),
            "expected database error, got: {err:?}"
        );
        info!("test_create_issue_with_parent_propagates_storage_error: assertions passed");
    }

    #[test]
    fn test_create_issue_dry_run_no_writes() {
        init_test_logging();
        info!("test_create_issue_dry_run_no_writes: starting");
        let mut storage = setup_memory_storage();
        let mut args = default_args();
        args.dry_run = true;
        let config = default_config();

        let issue = create_issue_impl(&mut storage, &args, &config, None).expect("create failed");

        // Should return issue but not verify existence in DB
        assert_eq!(issue.title, "Test Issue");
        let loaded = storage.get_issue(&issue.id).expect("get issue");
        assert!(loaded.is_none(), "dry run should not persist issue");
        info!("test_create_issue_dry_run_no_writes: assertions passed");
    }

    #[test]
    fn test_create_issue_with_overrides() {
        init_test_logging();
        info!("test_create_issue_with_overrides: starting");
        let mut storage = setup_memory_storage();
        let mut args = default_args();
        args.priority = Some("0".to_string());
        args.type_ = Some("bug".to_string());
        args.description = Some("Desc".to_string());
        let config = default_config();

        let issue = create_issue_impl(&mut storage, &args, &config, None).expect("create failed");

        assert_eq!(issue.priority, Priority::CRITICAL);
        assert_eq!(issue.issue_type, IssueType::Bug);
        assert_eq!(issue.description, Some("Desc".to_string()));
        info!("test_create_issue_with_overrides: assertions passed");
    }

    #[test]
    fn test_create_issue_with_labels_and_deps() {
        init_test_logging();
        info!("test_create_issue_with_labels_and_deps: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();

        // Create dependency target first
        let target_args = CreateArgs {
            title: Some("Target".to_string()),
            ..default_args()
        };
        let target =
            create_issue_impl(&mut storage, &target_args, &config, None).expect("create target");

        // Create issue with label and dep
        let mut args = default_args();
        args.labels = vec!["backend".to_string()];
        args.deps = vec![target.id.clone()];

        let issue = create_issue_impl(&mut storage, &args, &config, None).expect("create failed");

        // Verify labels
        let labels = storage.get_labels(&issue.id).expect("get labels");
        assert!(labels.contains(&"backend".to_string()));

        // Verify deps
        let deps = storage.get_dependencies(&issue.id).expect("get deps");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], target.id);
        info!("test_create_issue_with_labels_and_deps: assertions passed");
    }

    #[test]
    fn test_create_issue_dep_title_with_colon_resolves_as_title() {
        init_test_logging();
        info!("test_create_issue_dep_title_with_colon_resolves_as_title: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();

        let target_args = CreateArgs {
            title: Some("Step 1: Setup Database".to_string()),
            ..default_args()
        };
        let target =
            create_issue_impl(&mut storage, &target_args, &config, None).expect("create target");

        let mut args = default_args();
        args.deps = vec!["Step 1: Setup Database".to_string()];

        let issue =
            create_issue_impl(&mut storage, &args, &config, None).expect("create dependent");

        let deps = storage.get_dependencies(&issue.id).expect("get deps");
        assert_eq!(deps, vec![target.id]);
        info!("test_create_issue_dep_title_with_colon_resolves_as_title: assertions passed");
    }

    #[test]
    fn test_create_issue_with_missing_dependency_fails() {
        init_test_logging();
        info!("test_create_issue_with_missing_dependency_fails: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();
        let mut args = default_args();
        args.deps = vec!["bd-missing".to_string()];

        let err = create_issue_impl(&mut storage, &args, &config, None).unwrap_err();

        assert!(matches!(err, BeadsError::IssueNotFound { id } if id == "bd-missing"));
        info!("test_create_issue_with_missing_dependency_fails: assertions passed");
    }

    #[test]
    fn test_create_parent_dependency() {
        init_test_logging();
        info!("test_create_parent_dependency: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();

        // Parent
        let parent =
            create_issue_impl(&mut storage, &default_args(), &config, None).expect("parent");

        // Child
        let mut args = default_args();
        args.parent = Some(parent.id.clone());
        let child = create_issue_impl(&mut storage, &args, &config, None).expect("child");

        let deps = storage.get_dependencies(&child.id).expect("get deps");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], parent.id);
        info!("test_create_parent_dependency: assertions passed");
    }

    #[test]
    fn test_create_child_generates_hierarchical_id() {
        init_test_logging();
        info!("test_create_child_generates_hierarchical_id: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();

        // Create parent (epic)
        let mut parent_args = default_args();
        parent_args.title = Some("Epic Parent".to_string());
        let parent = create_issue_impl(&mut storage, &parent_args, &config, None).expect("parent");

        // Create first child - should get parent.1
        let mut child1_args = default_args();
        child1_args.title = Some("First Child".to_string());
        child1_args.parent = Some(parent.id.clone());
        let child1 = create_issue_impl(&mut storage, &child1_args, &config, None).expect("child1");

        // Verify child ID has the correct format: parent_id.1
        let expected_child1_id = format!("{}.1", parent.id);
        assert_eq!(
            child1.id, expected_child1_id,
            "First child should have ID {expected_child1_id}, got {}",
            child1.id
        );

        // Create second child - should get parent.2
        let mut child2_args = default_args();
        child2_args.title = Some("Second Child".to_string());
        child2_args.parent = Some(parent.id.clone());
        let child2 = create_issue_impl(&mut storage, &child2_args, &config, None).expect("child2");

        let expected_child2_id = format!("{}.2", parent.id);
        assert_eq!(
            child2.id, expected_child2_id,
            "Second child should have ID {expected_child2_id}, got {}",
            child2.id
        );

        // Verify dependencies are set correctly
        let deps1 = storage.get_dependencies(&child1.id).expect("get deps1");
        assert_eq!(deps1.len(), 1);
        assert_eq!(deps1[0], parent.id);

        let deps2 = storage.get_dependencies(&child2.id).expect("get deps2");
        assert_eq!(deps2.len(), 1);
        assert_eq!(deps2[0], parent.id);

        info!("test_create_child_generates_hierarchical_id: assertions passed");
    }

    #[test]
    fn test_create_child_with_nonexistent_parent_fails() {
        init_test_logging();
        info!("test_create_child_with_nonexistent_parent_fails: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();

        // Try to create child with non-existent parent
        let mut args = default_args();
        args.parent = Some("bd-nonexistent".to_string());

        let result = create_issue_impl(&mut storage, &args, &config, None);
        assert!(result.is_err(), "Should fail when parent doesn't exist");

        if let Err(BeadsError::IssueNotFound { id }) = result {
            assert_eq!(id, "bd-nonexistent");
        } else {
            unreachable!("Expected IssueNotFound error");
        }

        info!("test_create_child_with_nonexistent_parent_fails: assertions passed");
    }

    #[test]
    fn test_create_issue_custom_type_accepted() {
        init_test_logging();
        info!("test_create_issue_custom_type_accepted: starting");
        // Custom types are now accepted
        let mut storage = setup_memory_storage();
        let mut args = default_args();
        args.type_ = Some("custom_type".to_string());
        let config = default_config();

        let result = create_issue_impl(&mut storage, &args, &config, None);
        assert!(result.is_ok(), "create should succeed with custom type");
        let issue = result.unwrap();
        assert_eq!(
            issue.issue_type,
            IssueType::Custom("custom_type".to_string())
        );
        info!("test_create_issue_custom_type_accepted: assertions passed");
    }

    // =========================================================================
    // parse_optional_date tests (preserved)
    // =========================================================================

    #[test]
    fn test_parse_optional_date_none() {
        init_test_logging();
        info!("test_parse_optional_date_none: starting");
        let result = parse_optional_date(None);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        info!("test_parse_optional_date_none: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_empty_string() {
        init_test_logging();
        info!("test_parse_optional_date_empty_string: starting");
        let result = parse_optional_date(Some(""));
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        info!("test_parse_optional_date_empty_string: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_iso8601() {
        init_test_logging();
        info!("test_parse_optional_date_iso8601: starting");
        let result = parse_optional_date(Some("2026-01-17T10:00:00Z"));
        assert!(result.is_ok());
        let date = result.unwrap();
        assert!(date.is_some());
        let dt = date.unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 17);
        info!("test_parse_optional_date_iso8601: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_simple_date() {
        init_test_logging();
        info!("test_parse_optional_date_simple_date: starting");
        let result = parse_optional_date(Some("2026-12-31"));
        assert!(result.is_ok());
        let date = result.unwrap();
        assert!(date.is_some());
        let dt = date.unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 12);
        assert_eq!(dt.day(), 31);
        info!("test_parse_optional_date_simple_date: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_with_timezone() {
        init_test_logging();
        info!("test_parse_optional_date_with_timezone: starting");
        let result = parse_optional_date(Some("2026-06-15T14:30:00+05:30"));
        assert!(result.is_ok());
        info!("test_parse_optional_date_with_timezone: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_invalid_format() {
        init_test_logging();
        info!("test_parse_optional_date_invalid_format: starting");
        let result = parse_optional_date(Some("not-a-date"));
        assert!(result.is_err());
        info!("test_parse_optional_date_invalid_format: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_partial_date() {
        init_test_logging();
        info!("test_parse_optional_date_partial_date: starting");
        // Flexible parser may accept various formats
        let result = parse_optional_date(Some("2026-01"));
        let _ = result;
        info!("test_parse_optional_date_partial_date: assertions passed");
    }

    // =========================================================================
    // Date boundary tests
    // =========================================================================

    #[test]
    fn test_parse_optional_date_year_boundaries() {
        init_test_logging();
        info!("test_parse_optional_date_year_boundaries: starting");
        // Far future date
        let result = parse_optional_date(Some("2099-12-31"));
        assert!(result.is_ok());

        // Past date
        let result = parse_optional_date(Some("2000-01-01"));
        assert!(result.is_ok());
        info!("test_parse_optional_date_year_boundaries: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_leap_year() {
        init_test_logging();
        info!("test_parse_optional_date_leap_year: starting");
        // Feb 29 on leap year
        let result = parse_optional_date(Some("2024-02-29"));
        assert!(result.is_ok());
        let date = result.unwrap();
        assert!(date.is_some());
        let dt = date.unwrap();
        assert_eq!(dt.month(), 2);
        assert_eq!(dt.day(), 29);
        info!("test_parse_optional_date_leap_year: assertions passed");
    }

    #[test]
    fn test_parse_optional_date_end_of_month() {
        init_test_logging();
        info!("test_parse_optional_date_end_of_month: starting");
        // 31-day month
        let result = parse_optional_date(Some("2026-03-31"));
        assert!(result.is_ok());

        // 30-day month
        let result = parse_optional_date(Some("2026-04-30"));
        assert!(result.is_ok());
        info!("test_parse_optional_date_end_of_month: assertions passed");
    }

    // =========================================================================
    // Whitespace handling tests
    // =========================================================================

    #[test]
    fn test_parse_optional_date_whitespace_only() {
        init_test_logging();
        info!("test_parse_optional_date_whitespace_only: starting");
        // Should be treated as empty/None
        let result = parse_optional_date(Some("   "));
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
        info!("test_parse_optional_date_whitespace_only: assertions passed");
    }

    #[test]
    fn test_create_issue_trims_labels() {
        init_test_logging();
        info!("test_create_issue_trims_labels: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();
        let mut args = default_args();
        args.labels = vec!["  trimmed  ".to_string()];

        let issue = create_issue_impl(&mut storage, &args, &config, None).expect("create failed");

        let labels = storage.get_labels(&issue.id).expect("get labels");
        assert_eq!(labels, vec!["trimmed"]);
        info!("test_create_issue_trims_labels: assertions passed");
    }

    #[test]
    fn test_create_issue_deduplicates_labels() {
        init_test_logging();
        info!("test_create_issue_deduplicates_labels: starting");
        let mut storage = setup_memory_storage();
        let config = default_config();
        let mut args = default_args();
        args.labels = vec![
            "backend".to_string(),
            "backend".to_string(),
            "  backend  ".to_string(),
        ];

        let issue = create_issue_impl(&mut storage, &args, &config, None).expect("create failed");

        let labels = storage.get_labels(&issue.id).expect("get labels");
        assert_eq!(labels, vec!["backend"]);
        info!("test_create_issue_deduplicates_labels: assertions passed");
    }
}
