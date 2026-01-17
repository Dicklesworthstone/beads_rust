//! Comments command implementation.

use crate::cli::{CommentAddArgs, CommentCommands, CommentListArgs, CommentsArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::storage::SqliteStorage;
use crate::util::id::{IdResolver, ResolverConfig, find_matching_ids};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;

/// Execute the comments command.
///
/// # Errors
///
/// Returns an error if database operations fail or if inputs are invalid.
pub fn execute(args: &CommentsArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let config_layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let all_ids = storage_ctx.storage.get_all_ids()?;
    let actor = config::actor_from_layer(&config_layer);
    let storage = &mut storage_ctx.storage;

    match &args.command {
        Some(CommentCommands::Add(add_args)) => add_comment(
            add_args,
            storage,
            &resolver,
            &all_ids,
            actor.as_deref(),
            json,
        ),
        Some(CommentCommands::List(list_args)) => {
            list_comments(list_args, storage, &resolver, &all_ids, json)
        }
        None => {
            let id = args
                .id
                .as_deref()
                .ok_or_else(|| BeadsError::validation("id", "missing issue id"))?;
            list_comments_by_id(id, storage, &resolver, &all_ids, json)
        }
    }?;

    storage_ctx.flush_no_db_if_dirty()?;
    Ok(())
}

fn add_comment(
    args: &CommentAddArgs,
    storage: &mut SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    actor: Option<&str>,
    json: bool,
) -> Result<()> {
    let issue_id = resolve_issue_id(storage, resolver, all_ids, &args.id)?;
    let text = read_comment_text(args)?;
    if text.trim().is_empty() {
        return Err(BeadsError::validation(
            "text",
            "comment text cannot be empty",
        ));
    }
    let author = resolve_author(args.author.as_deref(), actor);

    let comment = storage.add_comment(&issue_id, &author, &text)?;

    if json {
        let output = serde_json::to_string_pretty(&comment)?;
        println!("{output}");
    } else {
        println!("Comment added to {issue_id}");
    }

    Ok(())
}

fn list_comments(
    args: &CommentListArgs,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    json: bool,
) -> Result<()> {
    list_comments_by_id(&args.id, storage, resolver, all_ids, json)
}

fn list_comments_by_id(
    id: &str,
    storage: &SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    json: bool,
) -> Result<()> {
    let issue_id = resolve_issue_id(storage, resolver, all_ids, id)?;
    let comments = storage.get_comments(&issue_id)?;

    if json {
        let output = serde_json::to_string_pretty(&comments)?;
        println!("{output}");
        return Ok(());
    }

    if comments.is_empty() {
        println!("No comments for {issue_id}.");
        return Ok(());
    }

    println!("Comments for {issue_id}:");
    for comment in comments {
        let timestamp = comment.created_at.format("%Y-%m-%d %H:%M UTC");
        println!("[{}] at {}", comment.author, timestamp);
        println!("{}", comment.body.trim_end_matches('\n'));
        println!();
    }

    Ok(())
}

fn resolve_issue_id(
    storage: &SqliteStorage,
    resolver: &IdResolver,
    all_ids: &[String],
    input: &str,
) -> Result<String> {
    resolver
        .resolve(
            input,
            |id| storage.id_exists(id).unwrap_or(false),
            |hash| find_matching_ids(all_ids, hash),
        )
        .map(|resolved| resolved.id)
}

fn read_comment_text(args: &CommentAddArgs) -> Result<String> {
    if let Some(path) = &args.file {
        if path.as_os_str() == "-" {
            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            return Ok(buffer);
        }
        return Ok(fs::read_to_string(path)?);
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
    if let Some(author) = author_override {
        if !author.trim().is_empty() {
            return author.to_string();
        }
    }
    if let Some(actor) = actor {
        if !actor.trim().is_empty() {
            return actor.to_string();
        }
    }
    if let Ok(value) = std::env::var("BD_ACTOR") {
        if !value.trim().is_empty() {
            return value;
        }
    }
    if let Ok(value) = std::env::var("BEADS_ACTOR") {
        if !value.trim().is_empty() {
            return value;
        }
    }
    if let Some(name) = git_user_name() {
        return name;
    }
    if let Ok(value) = std::env::var("USER") {
        if !value.trim().is_empty() {
            return value;
        }
    }

    "unknown".to_string()
}

fn git_user_name() -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--get", "user.name"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() { None } else { Some(name) }
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
        // Empty actor should fall through to env/git/USER/unknown
        // Since we can't easily control env, just test that it doesn't panic
        // and returns something non-empty
        let result = resolve_author(None, Some(""));
        assert!(!result.is_empty());
        info!("test_resolve_author_empty_actor_falls_through: assertions passed");
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
}
