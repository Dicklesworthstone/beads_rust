//! Comments command implementation.

use super::{
    RoutedWorkspaceWriteLock, acquire_routed_workspace_write_lock,
    auto_import_storage_ctx_if_stale, cli_for_routed_workspace, report_auto_flush_failure,
    resolve_issue_id, retry_mutation_with_jsonl_recovery,
};
use crate::cli::{CommentAddArgs, CommentCommands, CommentsArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{sanitize_terminal_inline, sanitize_terminal_text};
use crate::model::Comment;
use crate::output::{OutputContext, OutputMode, rich_text_panel, wrap_rich_lines};
use crate::storage::SqliteStorage;
use crate::util::id::{IdResolver, ResolverConfig};
use crate::util::time::format_relative_time;
use chrono::Utc;
use rich_rust::prelude::*;
use std::fs;
use std::io::Read;
use std::path::Path;

/// Execute the comments command.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute(
    args: &CommentsArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;

    match &args.command {
        Some(CommentCommands::Add(add_args)) => execute_add(add_args, cli, ctx, &beads_dir),
        Some(CommentCommands::List(list_args)) => {
            execute_list(&list_args.id, json, cli, ctx, &beads_dir, list_args.wrap)
        }
        None => {
            let id = args
                .id
                .as_deref()
                .ok_or_else(|| BeadsError::validation("id", "missing issue id"))?;
            execute_list(id, json, cli, ctx, &beads_dir, args.wrap)
        }
    }
}

/// Execute local read-only comments commands using storage already opened by the caller.
///
/// Returns `Ok(false)` when the command must use the normal routed or mutating path.
///
/// # Errors
///
/// Returns an error if route resolution, config loading, ID resolution, or comment lookup fails.
pub fn execute_with_storage_ctx(
    args: &CommentsArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<bool> {
    match &args.command {
        Some(CommentCommands::Add(_)) => Ok(false),
        Some(CommentCommands::List(list_args)) => execute_list_with_storage_ctx(
            &list_args.id,
            json,
            cli,
            ctx,
            local_beads_dir,
            list_args.wrap,
            storage_ctx,
        ),
        None => {
            let id = args
                .id
                .as_deref()
                .ok_or_else(|| BeadsError::validation("id", "missing issue id"))?;
            execute_list_with_storage_ctx(
                id,
                json,
                cli,
                ctx,
                local_beads_dir,
                args.wrap,
                storage_ctx,
            )
        }
    }
}

fn execute_add(
    args: &CommentAddArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
) -> Result<()> {
    let (mut storage_ctx, route_cli, auto_flush_external, _routed_write_lock) =
        open_routed_storage_for_input(beads_dir, cli, &args.id)?;
    let config_layer = storage_ctx.load_config(&route_cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let actor = config::actor_from_layer(&config_layer);

    let (issue_id, author, text) =
        prepare_comment_add(args, &storage_ctx.storage, &resolver, actor.as_deref())?;
    let comment = retry_mutation_with_jsonl_recovery(
        &mut storage_ctx,
        true,
        "comment add",
        Some(issue_id.as_str()),
        |storage| storage.add_comment(&issue_id, &author, &text),
    )?;
    storage_ctx.flush_no_db_if_dirty()?;
    if auto_flush_external && let Err(error) = storage_ctx.auto_flush_if_enabled() {
        report_auto_flush_failure(
            ctx,
            &storage_ctx.paths.beads_dir,
            &storage_ctx.paths.jsonl_path,
            &error,
        );
    }
    crate::util::set_last_touched_id(beads_dir, &issue_id);

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    if ctx.is_json() {
        ctx.json_pretty(&comment);
    } else if ctx.is_toon() {
        ctx.toon(&comment);
    } else if ctx.is_rich() {
        render_comment_added_rich(&issue_id, &comment, ctx);
    } else {
        println!("{}", comment_added_message(&issue_id));
    }

    Ok(())
}

fn execute_list(
    issue_input: &str,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    wrap: bool,
) -> Result<()> {
    let (storage_ctx, route_cli, _, _routed_write_lock) =
        open_routed_storage_for_input(beads_dir, cli, issue_input)?;
    let config_layer = storage_ctx.load_config(&route_cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));

    list_comments_by_id(
        issue_input,
        &storage_ctx.storage,
        &resolver,
        json,
        ctx,
        wrap,
    )
}

fn execute_list_with_storage_ctx(
    issue_input: &str,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    local_beads_dir: &Path,
    wrap: bool,
    storage_ctx: &config::OpenStorageResult,
) -> Result<bool> {
    let route = config::routing::resolve_route(issue_input, local_beads_dir)?;
    if route.is_external {
        return Ok(false);
    }

    let config_layer = storage_ctx.load_config(cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));

    list_comments_by_id(
        issue_input,
        &storage_ctx.storage,
        &resolver,
        json,
        ctx,
        wrap,
    )?;
    Ok(true)
}

fn open_routed_storage_for_input(
    local_beads_dir: &Path,
    cli: &config::CliOverrides,
    issue_input: &str,
) -> Result<(
    config::OpenStorageResult,
    config::CliOverrides,
    bool,
    RoutedWorkspaceWriteLock,
)> {
    let route = config::routing::resolve_route(issue_input, local_beads_dir)?;
    let mut route_cli = cli_for_routed_workspace(cli, route.is_external);
    let routed_write_lock = acquire_routed_workspace_write_lock(
        &route.beads_dir,
        route.is_external,
        route_cli.lock_timeout,
    )?;
    routed_write_lock.mark_cli_write_lock_held(&mut route_cli);
    let mut storage_ctx = config::open_storage_with_cli(&route.beads_dir, &route_cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, &route_cli)?;
    Ok((storage_ctx, route_cli, route.is_external, routed_write_lock))
}

fn prepare_comment_add(
    args: &CommentAddArgs,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    actor: Option<&str>,
) -> Result<(String, String, String)> {
    let issue_id = resolve_issue_id(storage, resolver, &args.id)?;
    let text = read_comment_text(args)?;
    if text.trim().is_empty() {
        return Err(BeadsError::validation(
            "text",
            "comment text cannot be empty",
        ));
    }
    let author = resolve_author(args.author.as_deref(), actor);
    Ok((issue_id, author, text))
}

fn list_comments_by_id(
    id: &str,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    _json: bool,
    ctx: &OutputContext,
    wrap: bool,
) -> Result<()> {
    let issue_id = resolve_issue_id(storage, resolver, id)?;
    let comments = storage.get_comments(&issue_id)?;

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    if ctx.is_json() {
        ctx.json_pretty(&comments);
        return Ok(());
    }

    if ctx.is_toon() {
        ctx.toon(&comments);
        return Ok(());
    }

    if matches!(ctx.mode(), OutputMode::Rich) {
        render_comments_list_rich(&issue_id, &comments, ctx, wrap);
        return Ok(());
    }

    if comments.is_empty() {
        println!("{}", no_comments_message(&issue_id));
        return Ok(());
    }

    println!("{}", comments_header(&issue_id));
    for comment in comments {
        let timestamp = comment.created_at.format("%Y-%m-%d %H:%M UTC");
        println!(
            "[{}] at {}",
            sanitize_terminal_inline(&comment.author),
            timestamp
        );
        println!(
            "{}",
            sanitize_terminal_text(comment.body.trim_end_matches('\n'))
        );
        println!();
    }

    Ok(())
}

/// Render a list of comments in rich format.
fn render_comments_list_rich(
    issue_id: &str,
    comments: &[Comment],
    ctx: &OutputContext,
    wrap: bool,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    if comments.is_empty() {
        let mut text = Text::new("");
        text.append_styled("\u{1f4ad} ", theme.dimmed.clone());
        text.append_styled(&no_comments_message(issue_id), theme.dimmed.clone());
        console.print_renderable(&text);
        return;
    }

    let mut content = Text::new("");
    let now = Utc::now();

    for (i, comment) in comments.iter().enumerate() {
        if i > 0 {
            // Separator between comments
            content.append_styled(
                &"\u{2500}".repeat(40.min(width.saturating_sub(4))),
                theme.dimmed.clone(),
            );
            content.append("\n\n");
        }

        // Author and timestamp
        content.append_styled(
            &format!("@{}", sanitize_terminal_inline(&comment.author)),
            theme.username.clone(),
        );
        content.append_styled(" \u{2022} ", theme.dimmed.clone());
        content.append_styled(
            &format_relative_time(comment.created_at, now),
            theme.timestamp.clone(),
        );
        content.append("\n");

        // Comment body
        content.append(sanitize_terminal_text(comment.body.trim_end_matches('\n')).as_ref());
        content.append("\n\n");
    }

    let title = comments_panel_title(issue_id, comments.len());
    let content = if wrap {
        wrap_rich_text(&content, width)
    } else {
        content
    };
    let panel = rich_text_panel(&content, width)
        .title(Text::styled(&title, theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

fn wrap_rich_text(text: &Text, panel_width: usize) -> Text {
    let content_width = panel_width.saturating_sub(4).max(1);
    let lines = wrap_rich_lines(text, content_width);
    let mut wrapped = Text::new("");
    for (idx, line) in lines.iter().enumerate() {
        if idx > 0 {
            wrapped.append("\n");
        }
        wrapped.append_text(line);
    }
    wrapped
}

/// Render confirmation for a newly added comment.
fn render_comment_added_rich(issue_id: &str, comment: &Comment, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();

    let mut text = Text::new("");
    text.append_styled("\u{2713} ", theme.success.clone());
    text.append_styled("Added comment to ", theme.success.clone());
    text.append_styled(
        sanitize_terminal_inline(issue_id).as_ref(),
        theme.issue_id.clone(),
    );
    console.print_renderable(&text);

    console.print("");

    // Show the comment that was added
    let mut comment_text = Text::new("");
    comment_text.append_styled(
        &format!("@{}", sanitize_terminal_inline(&comment.author)),
        theme.username.clone(),
    );
    comment_text.append_styled(" \u{2022} just now", theme.timestamp.clone());
    comment_text.append("\n");
    comment_text.append(sanitize_terminal_text(comment.body.trim_end_matches('\n')).as_ref());
    console.print_renderable(&comment_text);
}

fn comment_added_message(issue_id: &str) -> String {
    format!("Comment added to {}", sanitize_terminal_inline(issue_id))
}

fn comments_header(issue_id: &str) -> String {
    format!("Comments for {}:", sanitize_terminal_inline(issue_id))
}

fn no_comments_message(issue_id: &str) -> String {
    format!("No comments for {}.", sanitize_terminal_inline(issue_id))
}

fn comments_panel_title(issue_id: &str, count: usize) -> String {
    format!("Comments: {} ({count})", sanitize_terminal_inline(issue_id))
}

const MAX_STDIN_COMMENT_BYTES: usize = 10 * 1024 * 1024;

fn read_limited_string<R: Read>(reader: &mut R, byte_limit: usize, field: &str) -> Result<String> {
    let max_bytes = byte_limit
        .checked_add(1)
        .and_then(|limit| u64::try_from(limit).ok())
        .unwrap_or(u64::MAX);
    let mut buffer = Vec::new();
    reader.take(max_bytes).read_to_end(&mut buffer)?;
    if buffer.len() > byte_limit {
        return Err(BeadsError::validation(
            field,
            format!("{field} input exceeds maximum size of {byte_limit} bytes"),
        ));
    }
    String::from_utf8(buffer).map_err(|err| {
        BeadsError::validation(field, format!("{field} input must be valid UTF-8: {err}"))
    })
}

fn read_comment_text(args: &CommentAddArgs) -> Result<String> {
    if let Some(path) = &args.file {
        if path.as_os_str() == "-" {
            let mut stdin = std::io::stdin();
            return read_limited_string(&mut stdin, MAX_STDIN_COMMENT_BYTES, "text");
        }
        let mut file = fs::File::open(path)?;
        return read_limited_string(&mut file, MAX_STDIN_COMMENT_BYTES, "file");
    }
    if let Some(message) = &args.message {
        return Ok(message.clone());
    }
    if !args.text.is_empty() {
        return Ok(args.text.join(" "));
    }
    Err(BeadsError::validation("text", "comment text required"))
}

fn resolve_author(author_override: Option<&str>, actor: Option<&str>) -> String {
    if let Some(author) = author_override
        && !author.trim().is_empty()
    {
        return author.to_string();
    }
    if let Some(actor) = actor
        && !actor.trim().is_empty()
    {
        return actor.to_string();
    }
    if let Some(value) = resolve_author_from_env(|name| std::env::var(name).ok()) {
        return value;
    }

    "unknown".to_string()
}

fn resolve_author_from_env(mut lookup: impl FnMut(&str) -> Option<String>) -> Option<String> {
    for key in ["BD_ACTOR", "BEADS_ACTOR", "USER", "LOGNAME", "USERNAME"] {
        if let Some(value) = lookup(key)
            && !value.trim().is_empty()
        {
            return Some(value);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::init_test_logging;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use tracing::info;

    #[test]
    fn test_resolve_author_with_override() {
        init_test_logging();
        info!("test_resolve_author_with_override: starting");
        // When author override is provided, it should be used
        let result = resolve_author(Some("custom_author"), Some("actor_name"));
        assert_eq!(result, "custom_author");
        info!("test_resolve_author_with_override: assertions passed");
    }

    #[test]
    fn test_resolve_author_empty_override_uses_actor() {
        init_test_logging();
        info!("test_resolve_author_empty_override_uses_actor: starting");
        // Empty override should fall through to actor
        let result = resolve_author(Some(""), Some("actor_name"));
        assert_eq!(result, "actor_name");
        info!("test_resolve_author_empty_override_uses_actor: assertions passed");
    }

    #[test]
    fn test_resolve_author_whitespace_override_uses_actor() {
        init_test_logging();
        info!("test_resolve_author_whitespace_override_uses_actor: starting");
        // Whitespace-only override should fall through to actor
        let result = resolve_author(Some("   "), Some("actor_name"));
        assert_eq!(result, "actor_name");
        info!("test_resolve_author_whitespace_override_uses_actor: assertions passed");
    }

    #[test]
    fn test_resolve_author_no_override_uses_actor() {
        init_test_logging();
        info!("test_resolve_author_no_override_uses_actor: starting");
        // No override should use actor
        let result = resolve_author(None, Some("actor_name"));
        assert_eq!(result, "actor_name");
        info!("test_resolve_author_no_override_uses_actor: assertions passed");
    }

    #[test]
    fn test_resolve_author_empty_actor_falls_through() {
        init_test_logging();
        info!("test_resolve_author_empty_actor_falls_through: starting");
        // Empty actor should fall through to env/USER/LOGNAME/USERNAME/unknown
        // Since we can't easily control env, just test that it doesn't panic
        // and returns something non-empty
        let result = resolve_author(None, Some(""));
        assert!(!result.is_empty());
        info!("test_resolve_author_empty_actor_falls_through: assertions passed");
    }

    #[test]
    fn test_resolve_author_env_helper_checks_windows_username() {
        init_test_logging();
        info!("test_resolve_author_env_helper_checks_windows_username: starting");
        let result = resolve_author_from_env(|name| match name {
            "USERNAME" => Some("windows-user".to_string()),
            _ => None,
        });
        assert_eq!(result.as_deref(), Some("windows-user"));
        info!("test_resolve_author_env_helper_checks_windows_username: assertions passed");
    }

    #[test]
    fn test_read_comment_text_from_message_flag() {
        init_test_logging();
        info!("test_read_comment_text_from_message_flag: starting");
        let args = CommentAddArgs {
            id: "test-id".to_string(),
            text: vec![],
            file: None,
            author: None,
            message: Some("message flag content".to_string()),
        };
        let result = read_comment_text(&args).unwrap();
        assert_eq!(result, "message flag content");
        info!("test_read_comment_text_from_message_flag: assertions passed");
    }

    #[test]
    fn test_read_comment_text_from_positional_args() {
        init_test_logging();
        info!("test_read_comment_text_from_positional_args: starting");
        let args = CommentAddArgs {
            id: "test-id".to_string(),
            text: vec!["hello".to_string(), "world".to_string()],
            file: None,
            author: None,
            message: None,
        };
        let result = read_comment_text(&args).unwrap();
        assert_eq!(result, "hello world");
        info!("test_read_comment_text_from_positional_args: assertions passed");
    }

    #[test]
    fn test_read_comment_text_from_file() {
        init_test_logging();
        info!("test_read_comment_text_from_file: starting");
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "Comment from file").unwrap();
        file.flush().unwrap();

        let args = CommentAddArgs {
            id: "test-id".to_string(),
            text: vec![],
            file: Some(file.path().to_path_buf()),
            author: None,
            message: None,
        };
        let result = read_comment_text(&args).unwrap();
        assert!(result.contains("Comment from file"));
        info!("test_read_comment_text_from_file: assertions passed");
    }

    #[test]
    fn test_read_comment_text_rejects_oversized_file() {
        init_test_logging();
        info!("test_read_comment_text_rejects_oversized_file: starting");
        let mut file = NamedTempFile::new().unwrap();
        let payload = vec![b'a'; MAX_STDIN_COMMENT_BYTES + 1];
        file.write_all(&payload).unwrap();
        file.flush().unwrap();

        let args = CommentAddArgs {
            id: "test-id".to_string(),
            text: vec![],
            file: Some(file.path().to_path_buf()),
            author: None,
            message: None,
        };
        let err = read_comment_text(&args).expect_err("oversized file");
        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "file"));
        info!("test_read_comment_text_rejects_oversized_file: assertions passed");
    }

    #[test]
    fn test_read_comment_text_file_takes_precedence() {
        init_test_logging();
        info!("test_read_comment_text_file_takes_precedence: starting");
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "File content").unwrap();
        file.flush().unwrap();

        let args = CommentAddArgs {
            id: "test-id".to_string(),
            text: vec!["text content".to_string()],
            file: Some(file.path().to_path_buf()),
            author: None,
            message: Some("message content".to_string()),
        };
        let result = read_comment_text(&args).unwrap();
        // File should take precedence
        assert!(result.contains("File content"));
        info!("test_read_comment_text_file_takes_precedence: assertions passed");
    }

    #[test]
    fn test_read_comment_text_no_input_fails() {
        init_test_logging();
        info!("test_read_comment_text_no_input_fails: starting");
        let args = CommentAddArgs {
            id: "test-id".to_string(),
            text: vec![],
            file: None,
            author: None,
            message: None,
        };
        let result = read_comment_text(&args);
        assert!(result.is_err());
        info!("test_read_comment_text_no_input_fails: assertions passed");
    }

    #[test]
    fn test_read_limited_string_accepts_content_within_limit() {
        init_test_logging();
        info!("test_read_limited_string_accepts_content_within_limit: starting");
        let payload = "a".repeat(32);
        let mut reader = payload.as_bytes();
        let result = read_limited_string(&mut reader, 32, "text").expect("read within limit");
        assert_eq!(result.len(), 32);
        info!("test_read_limited_string_accepts_content_within_limit: assertions passed");
    }

    #[test]
    fn test_read_limited_string_rejects_oversized_input() {
        init_test_logging();
        info!("test_read_limited_string_rejects_oversized_input: starting");
        let payload = "a".repeat(33);
        let mut reader = payload.as_bytes();
        let err = read_limited_string(&mut reader, 32, "text").expect_err("oversized stdin");
        assert!(matches!(err, BeadsError::Validation { .. }));
        info!("test_read_limited_string_rejects_oversized_input: assertions passed");
    }

    #[test]
    fn test_read_limited_string_checks_size_before_utf8_decode() {
        init_test_logging();
        info!("test_read_limited_string_checks_size_before_utf8_decode: starting");
        let payload = "aaaaé";
        let mut reader = payload.as_bytes();
        let err = read_limited_string(&mut reader, 4, "text")
            .expect_err("oversized input should be reported before UTF-8 decoding");
        assert!(
            matches!(&err, BeadsError::Validation { field, reason }
                if field == "text" && reason.contains("exceeds maximum size")),
            "unexpected error: {err:?}"
        );
        info!("test_read_limited_string_checks_size_before_utf8_decode: assertions passed");
    }

    #[test]
    fn test_read_limited_string_rejects_invalid_utf8() {
        init_test_logging();
        info!("test_read_limited_string_rejects_invalid_utf8: starting");
        let payload = [0xff];
        let mut reader = payload.as_slice();
        let err = read_limited_string(&mut reader, 4, "text")
            .expect_err("invalid UTF-8 input should be rejected");
        assert!(
            matches!(&err, BeadsError::Validation { field, reason }
                if field == "text" && reason.contains("valid UTF-8")),
            "unexpected error: {err:?}"
        );
        info!("test_read_limited_string_rejects_invalid_utf8: assertions passed");
    }

    #[test]
    fn comment_human_messages_sanitize_issue_id() {
        init_test_logging();
        info!("comment_human_messages_sanitize_issue_id: starting");
        let issue_id = "bd-1\x1b]52;c;bad\x07\rreset";

        let rendered = [
            comment_added_message(issue_id),
            comments_header(issue_id),
            no_comments_message(issue_id),
            comments_panel_title(issue_id, 2),
        ]
        .join("\n");

        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert!(!rendered.contains('\r'));
        assert!(rendered.contains("bd-1\\u{1b}]52;c;bad\\u{7}\\rreset"));
        info!("comment_human_messages_sanitize_issue_id: assertions passed");
    }
}
