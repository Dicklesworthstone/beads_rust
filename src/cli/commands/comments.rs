//! Comments command implementation.
//!
//! Comments are an issue's HISTORY: append-only, attributed to an author,
//! timestamped by the database, and never rewritten. They are deliberately
//! distinct from the `notes`/`design`/`acceptance_criteria` fields, which
//! are STATE — the current standing summary of an issue, replaced wholesale
//! by `bd update` and covered by the content hash.
//!
//! Because appending is a pure insert, "append annotation" cannot truncate
//! whatever was already there. That is the failure this command exists to
//! make unrepresentable: writing an annotation into `notes` requires a
//! read-modify-write, and a read that comes back short silently destroys
//! the rest of the field on write-back.
//!
//! Surface:
//!
//! ```text
//! bd comments <id>                  # complete log, chronological
//! bd comments add <id> <text>       # append one entry
//! bd comments add <id> -f <file>    # append from a file ("-" = stdin)
//! ```
//!
//! [`append_comment`] is the ONLY comment writer in the CLI: `bd reopen
//! --reason` routes through it too, so there is a single place where a
//! comment can be created, attributed and event-logged.

use crate::cli::{
    CommentsAddArgs, CommentsArgs, CommentsCommands, OutputFormat, resolve_output_format_basic,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::escape_markup;
use crate::model::Comment;
use crate::output::{OutputContext, OutputMode};
use crate::storage::SqliteStorage;
use crate::util::id::IdResolver;
use std::fmt::Write as FmtWrite;
use std::io::Read;

/// Default number of comments `bd show` renders.
///
/// `bd show` already bounds events to 10; comments follow that precedent
/// rather than inventing a second convention, but with a much smaller
/// bound because a comment carries far more text than an event does
/// (observed bodies run to ~2.3KB each, where an event line is one
/// sentence). Three keeps the tail of the conversation visible without
/// turning `bd show` into a firehose; `--comments <N|all>` expands it and
/// `bd comments <id>` is always complete.
pub const DEFAULT_SHOW_COMMENT_LIMIT: usize = 3;

/// Parse a `--comments <N|all>` specification.
///
/// `None` (flag absent) means the default bound; `all` (or `*`) means no
/// bound at all; a number means exactly that many. `0` is legal and means
/// "render none" — the count is still reported, so this hides bodies
/// without hiding the fact that history exists.
///
/// # Errors
///
/// Returns a validation error if the value is neither `all` nor a number.
pub fn parse_comment_limit(spec: Option<&str>) -> Result<Option<usize>> {
    match spec.map(str::trim) {
        None => Ok(Some(DEFAULT_SHOW_COMMENT_LIMIT)),
        Some(value) if value.eq_ignore_ascii_case("all") || value == "*" => Ok(None),
        Some(value) => value.parse::<usize>().map(Some).map_err(|_| {
            BeadsError::validation(
                "comments",
                format!("expected a count or `all`, got `{value}`"),
            )
        }),
    }
}

/// Bound a comment list to the newest `limit` entries, keeping them in
/// chronological order.
///
/// Returns whether anything was dropped from the view. `limit` of `None`
/// keeps everything. Newest-N-in-chronological-order is the shape a reader
/// wants: the most recent exchange, still reading forwards in time.
pub fn bound_comments(comments: &mut Vec<Comment>, limit: Option<usize>) -> bool {
    let Some(limit) = limit else {
        return false;
    };
    if comments.len() <= limit {
        return false;
    }
    let drop_count = comments.len() - limit;
    comments.drain(..drop_count);
    true
}

/// Append a comment to an issue.
///
/// This is the single writer path for comments. It verifies the issue
/// exists (so a typo'd ID is a clean `IssueNotFound` rather than a foreign
/// key error), then delegates to storage, which inserts the row, bumps
/// `updated_at`, records the `Commented` event and marks the issue dirty
/// for JSONL export.
///
/// # Errors
///
/// Returns an error if the issue does not exist or the insert fails.
pub fn append_comment(
    storage: &mut SqliteStorage,
    issue_id: &str,
    author: &str,
    text: &str,
) -> Result<Comment> {
    if !storage.id_exists(issue_id)? {
        return Err(BeadsError::IssueNotFound {
            id: issue_id.to_string(),
        });
    }
    storage.add_comment(issue_id, author, text)
}

/// Execute the comments command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened, the issue ID cannot
/// be resolved, or a comment body cannot be read.
pub fn execute(
    args: &CommentsArgs,
    _json: bool,
    cli: &config::CliOverrides,
    outer_ctx: &OutputContext,
) -> Result<()> {
    match &args.command {
        Some(CommentsCommands::Add(add_args)) => add(add_args, cli, outer_ctx),
        None => list(args, cli, outer_ctx),
    }
}

/// Resolve the target issue ID, falling back to the last-touched issue.
fn resolve_target(
    storage: &SqliteStorage,
    beads_dir: &std::path::Path,
    requested: Option<&String>,
) -> Result<String> {
    let input = match requested {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            let last_touched = crate::util::get_last_touched_id(beads_dir);
            if last_touched.is_empty() {
                return Err(BeadsError::validation(
                    "id",
                    "no issue ID provided and no last-touched issue",
                ));
            }
            last_touched
        }
    };

    let resolver = IdResolver::with_defaults();
    let resolution = resolver.resolve(
        &input,
        |id| storage.id_exists(id).unwrap_or(false),
        |hash| storage.find_ids_by_hash(hash).unwrap_or_default(),
    )?;
    Ok(resolution.id)
}

/// `bd comments <id>` — the complete, unbounded log.
fn list(args: &CommentsArgs, cli: &config::CliOverrides, outer_ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let storage = &storage_ctx.storage;
    let config_layer = config::load_config(&beads_dir, Some(storage), cli)?;
    let use_color = config::should_use_color(&config_layer);

    let id = resolve_target(storage, &beads_dir, args.id.as_ref())?;
    let comments = storage.get_comments(&id)?;

    let output_format = resolve_output_format_basic(args.format, outer_ctx.is_json(), false);
    let quiet = cli.quiet.unwrap_or(false);
    let ctx = OutputContext::from_output_format(output_format, quiet, !use_color);
    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    match output_format {
        // A bare array of Comment objects, matching the classic surface —
        // this list is never bounded, so there is nothing to declare
        // about truncation here (see `show` for the bounded view).
        OutputFormat::Json => ctx.json_pretty(&comments),
        OutputFormat::Toon => ctx.toon(&comments),
        OutputFormat::Text | OutputFormat::Csv => {
            ctx.print(&format_comment_log(&id, &comments));
        }
    }

    Ok(())
}

/// Render the full comment log as text.
///
/// Author and body are escaped before they reach the console. Text printed
/// through the rich console is parsed as markup, and a `[word]` sequence is
/// taken for a style tag and consumed — so an unescaped author would vanish
/// from its own heading, and a body mentioning `[bold]` would be silently
/// rewritten. A record that quietly drops part of what someone wrote is
/// worse than no record, so every piece of stored data is escaped here.
fn format_comment_log(id: &str, comments: &[Comment]) -> String {
    if comments.is_empty() {
        return format!("No comments on {id}.");
    }

    let mut out = String::new();
    let _ = writeln!(out, "Comments on {id} ({})", comments.len());
    for comment in comments {
        out.push('\n');
        let _ = writeln!(
            out,
            "{} at {}",
            escape_markup(&comment.author),
            comment.created_at.format("%Y-%m-%d %H:%M")
        );
        for line in comment.body.lines() {
            let _ = writeln!(out, "    {}", escape_markup(line));
        }
        if comment.body.is_empty() {
            // An empty body is legal; say so rather than rendering a
            // blank stretch the reader has to interpret.
            let _ = writeln!(out, "    (empty)");
        }
    }
    // `ctx.print` adds the trailing newline.
    let _ = out.pop();
    out
}

/// `bd comments add <id> <text>` — append one entry.
fn add(args: &CommentsAddArgs, cli: &config::CliOverrides, outer_ctx: &OutputContext) -> Result<()> {
    let text = resolve_body(args)?;

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let use_color = config::should_use_color(&config_layer);

    // Author precedence: explicit --author, then the resolved agent
    // identity (config actor -> agent identity -> user), which is the
    // same resolution used for `created_by` and event actors.
    let author = match args.author.as_deref().map(str::trim) {
        Some(explicit) if !explicit.is_empty() => explicit.to_string(),
        _ => config::resolve_actor_with_storage(&config_layer, &storage_ctx.storage),
    };

    let id = resolve_target(&storage_ctx.storage, &beads_dir, args.id.as_ref())?;
    let comment = append_comment(&mut storage_ctx.storage, &id, &author, &text)?;

    crate::util::set_last_touched_id(&beads_dir, &id);

    let output_format = resolve_output_format_basic(args.format, outer_ctx.is_json(), false);
    let quiet = cli.quiet.unwrap_or(false);
    let ctx = OutputContext::from_output_format(output_format, quiet, !use_color);
    if !matches!(ctx.mode(), OutputMode::Quiet) {
        match output_format {
            OutputFormat::Json => ctx.json_pretty(&comment),
            OutputFormat::Toon => ctx.toon(&comment),
            OutputFormat::Text | OutputFormat::Csv => {
                ctx.success(&format!("Added comment to {id} (as {author})"));
            }
        }
    }

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}

/// Resolve the comment body from the positional text or `--file`.
///
/// Empty text is allowed (a deliberate part of the classic behavior); what
/// is not allowed is supplying neither, or both.
fn resolve_body(args: &CommentsAddArgs) -> Result<String> {
    match (args.text.as_deref(), args.file.as_deref()) {
        (Some(_), Some(_)) => Err(BeadsError::validation(
            "text",
            "provide either comment text or --file, not both",
        )),
        (Some(text), None) => Ok(text.to_string()),
        (None, Some(path)) => {
            if path == std::path::Path::new("-") {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(|e| BeadsError::validation("file", format!("cannot read stdin: {e}")))?;
                Ok(buf)
            } else {
                std::fs::read_to_string(path).map_err(|e| {
                    BeadsError::validation("file", format!("cannot read {}: {e}", path.display()))
                })
            }
        }
        (None, None) => Err(BeadsError::validation(
            "text",
            "comment text required (positional TEXT, or --file <FILE>)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::{TimeZone, Utc};

    fn make_issue(id: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: "Commented issue".to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ..Issue::default()
        }
    }

    fn storage_with_issue(id: &str) -> SqliteStorage {
        let mut storage = SqliteStorage::open_memory().expect("storage");
        storage.create_issue(&make_issue(id), "tester").expect("create");
        storage
    }

    #[test]
    fn append_comment_preserves_existing_comments_and_order() {
        let mut storage = storage_with_issue("bd-c1");

        for text in ["first", "second", "third"] {
            append_comment(&mut storage, "bd-c1", "alice", text).expect("append");
        }

        let comments = storage.get_comments("bd-c1").expect("comments");
        let bodies: Vec<&str> = comments.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec!["first", "second", "third"]);
        assert!(comments.iter().all(|c| c.author == "alice"));
    }

    #[test]
    fn append_comment_allows_empty_text_and_duplicates() {
        let mut storage = storage_with_issue("bd-c2");

        append_comment(&mut storage, "bd-c2", "alice", "").expect("empty");
        append_comment(&mut storage, "bd-c2", "alice", "same").expect("dup 1");
        append_comment(&mut storage, "bd-c2", "alice", "same").expect("dup 2");

        let comments = storage.get_comments("bd-c2").expect("comments");
        assert_eq!(comments.len(), 3);
        assert_eq!(comments[0].body, "");
    }

    #[test]
    fn append_comment_rejects_unknown_issue() {
        let mut storage = storage_with_issue("bd-c3");
        let err =
            append_comment(&mut storage, "no-such-issue", "alice", "hi").expect_err("must fail");
        assert!(matches!(err, BeadsError::IssueNotFound { .. }));
    }

    #[test]
    fn resolve_body_prefers_positional_text_and_allows_leading_dash() {
        let args = CommentsAddArgs {
            text: Some("- a bulleted note".to_string()),
            ..CommentsAddArgs::default()
        };
        assert_eq!(resolve_body(&args).expect("body"), "- a bulleted note");
    }

    #[test]
    fn resolve_body_requires_text_or_file() {
        let args = CommentsAddArgs::default();
        assert!(resolve_body(&args).is_err());
    }

    #[test]
    fn resolve_body_rejects_text_and_file_together() {
        let args = CommentsAddArgs {
            text: Some("inline".to_string()),
            file: Some("/tmp/whatever".into()),
            ..CommentsAddArgs::default()
        };
        assert!(resolve_body(&args).is_err());
    }

    #[test]
    fn resolve_body_reads_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("body.md");
        std::fs::write(&path, "- from a file\n- second line\n").expect("write");
        let args = CommentsAddArgs {
            file: Some(path),
            ..CommentsAddArgs::default()
        };
        assert_eq!(
            resolve_body(&args).expect("body"),
            "- from a file\n- second line\n"
        );
    }

    #[test]
    fn format_comment_log_renders_author_timestamp_and_body() {
        let comments = vec![
            Comment {
                id: 1,
                issue_id: "bd-1".to_string(),
                author: "alice".to_string(),
                body: "line one\nline two".to_string(),
                created_at: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 0).unwrap(),
            },
            Comment {
                id: 2,
                issue_id: "bd-1".to_string(),
                author: "bob".to_string(),
                body: String::new(),
                created_at: Utc.with_ymd_and_hms(2026, 1, 3, 4, 5, 0).unwrap(),
            },
        ];

        let out = format_comment_log("bd-1", &comments);
        assert!(out.starts_with("Comments on bd-1 (2)"));
        // The author is not bracketed: a bare `[alice]` is indistinguishable
        // from a style tag to the console and would be swallowed whole.
        assert!(out.contains("alice at 2026-01-02 03:04"));
        assert!(out.contains("    line one"));
        assert!(out.contains("    line two"));
        assert!(out.contains("bob at 2026-01-03 04:05"));
        assert!(out.contains("(empty)"));
    }

    /// A body is a verbatim record, so bracketed text in it survives to the
    /// console as an escape rather than being parsed away as markup.
    #[test]
    fn format_comment_log_escapes_markup_in_body_and_author() {
        let comments = vec![Comment {
            id: 1,
            issue_id: "bd-1".to_string(),
            author: "[bot]".to_string(),
            body: "use [bold] for headings".to_string(),
            created_at: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 0).unwrap(),
        }];

        let out = format_comment_log("bd-1", &comments);
        assert!(out.contains("\\[bot] at"), "author escaped: {out}");
        assert!(out.contains("use \\[bold] for headings"), "body escaped: {out}");
    }

    #[test]
    fn format_comment_log_says_so_when_empty() {
        assert_eq!(format_comment_log("bd-1", &[]), "No comments on bd-1.");
    }

    fn comment(n: i64) -> Comment {
        Comment {
            id: n,
            issue_id: "bd-1".to_string(),
            author: "alice".to_string(),
            body: format!("comment {n}"),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
                + chrono::Duration::minutes(n),
        }
    }

    #[test]
    fn bound_comments_keeps_newest_in_chronological_order() {
        let mut comments: Vec<Comment> = (1..=5).map(comment).collect();
        let truncated = bound_comments(&mut comments, Some(2));
        assert!(truncated);
        let bodies: Vec<&str> = comments.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec!["comment 4", "comment 5"]);
    }

    #[test]
    fn bound_comments_is_a_noop_below_the_bound() {
        let mut comments: Vec<Comment> = (1..=2).map(comment).collect();
        assert!(!bound_comments(&mut comments, Some(3)));
        assert_eq!(comments.len(), 2);
        assert!(!bound_comments(&mut comments, Some(2)));
        assert_eq!(comments.len(), 2);
    }

    #[test]
    fn bound_comments_none_means_unbounded() {
        let mut comments: Vec<Comment> = (1..=50).map(comment).collect();
        assert!(!bound_comments(&mut comments, None));
        assert_eq!(comments.len(), 50);
    }

    #[test]
    fn bound_comments_zero_hides_bodies_but_reports_truncation() {
        let mut comments: Vec<Comment> = (1..=3).map(comment).collect();
        assert!(bound_comments(&mut comments, Some(0)));
        assert!(comments.is_empty());
    }

    #[test]
    fn parse_comment_limit_handles_default_all_and_counts() {
        assert_eq!(
            parse_comment_limit(None).expect("default"),
            Some(DEFAULT_SHOW_COMMENT_LIMIT)
        );
        assert_eq!(parse_comment_limit(Some("all")).expect("all"), None);
        assert_eq!(parse_comment_limit(Some("ALL")).expect("all"), None);
        assert_eq!(parse_comment_limit(Some("7")).expect("7"), Some(7));
        assert_eq!(parse_comment_limit(Some("0")).expect("0"), Some(0));
        assert!(parse_comment_limit(Some("some")).is_err());
        assert!(parse_comment_limit(Some("-1")).is_err());
    }
}
