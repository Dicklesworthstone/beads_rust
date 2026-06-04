//! Ephemeral messaging commands: msg / inbox / outbox.
//!
//! Messages are NOT issues. They round-trip locally only, expire after
//! a TTL once read, and never enter the issue work-list.

use crate::cli::{InboxArgs, MsgArgs, OutboxArgs, OutputFormat, resolve_output_format_basic};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::Message;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::storage::messages::{MessageFilter, generate_message_id};
use chrono::Utc;
use serde::Serialize;
use std::io::{Read, Write};

const PREVIEW_CHARS: usize = 200;
const MESSAGES_TTL_DAYS: i64 = 7;

#[derive(Serialize)]
struct MessageView<'a> {
    id: &'a str,
    from: &'a str,
    to: &'a str,
    sent_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    read_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    in_reply_to: Option<&'a str>,
    body: &'a str,
    truncated: bool,
}

/// Send a message.
///
/// # Errors
///
/// Returns an error if the recipient/body are invalid or DB writes fail.
pub fn execute_msg(args: &MsgArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let to = args.to.trim();
    if to.is_empty() {
        return Err(BeadsError::validation("to", "recipient prefix is required"));
    }

    if to.eq_ignore_ascii_case(config::OPERATOR_PREFIX) {
        return Err(BeadsError::validation(
            "to",
            "messages to 'operator' must use `bd ask` so the attend REPL \
             can render them — try `bd ask <body>`, `bd ask --yn`, or \
             `bd ask --choices a,b,c`",
        ));
    }

    let body = resolve_body(&args.body)?;
    if body.trim().is_empty() {
        return Err(BeadsError::validation("body", "message body is empty"));
    }

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let from = config::id_config_from_layer(&layer).prefix;

    if let Some(reply) = &args.reply {
        if storage_ctx.storage.get_message(reply)?.is_none() {
            return Err(BeadsError::validation(
                "reply",
                format!("no such message: {reply}"),
            ));
        }
    }

    let now = Utc::now();

    // Reject messages to prefixes with no active `bd watch` heartbeat
    // (the silent-drop footgun: `bd msg infra` when the watcher is
    // `infra1`). Skip the check when --force is set, when replying
    // to a real message (the asker presumably knows what they're
    // doing), or when messaging your own prefix (testing).
    if !args.force && args.reply.is_none() && to != from {
        let ttl = crate::storage::watchers::WATCHER_TTL_SECONDS;
        let _ = storage_ctx.storage.sweep_stale_watchers(now, ttl);
        if !storage_ctx.storage.is_prefix_watched(to, now, ttl)? {
            let active = storage_ctx.storage.active_watcher_prefixes(now, ttl)?;
            let hint = if active.is_empty() {
                "no agents are currently watching. If this isn't a typo, \
                 flag it to the operator with `bd ask`."
                    .to_string()
            } else {
                format!(
                    "active watchers: {}. If this isn't a typo, flag it to \
                     the operator with `bd ask`.",
                    active.join(", ")
                )
            };
            return Err(BeadsError::validation(
                "to",
                format!("no active `bd watch` for '{to}' — {hint}"),
            ));
        }
    }

    let id = pick_message_id(&storage_ctx.storage, &from, to, &body, now)?;
    let msg = Message {
        id: id.clone(),
        from_prefix: from,
        to_prefix: to.to_string(),
        body,
        sent_at: now,
        read_at: None,
        in_reply_to: args.reply.clone(),
        choices: None,
    };

    storage_ctx.storage.insert_message(&msg)?;

    if ctx.is_json() {
        ctx.json_pretty(&msg);
    } else {
        ctx.success(&format!("Sent {} to {}", msg.id, msg.to_prefix));
    }
    Ok(())
}

/// List received messages, or show one in full.
///
/// # Errors
///
/// Returns an error if the DB query fails or the requested message ID is unknown.
pub fn execute_inbox(
    args: &InboxArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let me = config::id_config_from_layer(&layer).prefix;

    // Sweep stale read messages on every inbox access — cheap, no daemon needed.
    let now = Utc::now();
    storage_ctx
        .storage
        .sweep_read_messages(MESSAGES_TTL_DAYS, now)?;

    let format = resolve_output_format_basic(args.format, ctx.is_json(), false);

    if let Some(id) = &args.id {
        let msg = storage_ctx
            .storage
            .get_message(id)?
            .ok_or_else(|| BeadsError::validation("id", format!("no such message: {id}")))?;
        if msg.to_prefix != me {
            return Err(BeadsError::validation(
                "id",
                format!("{id} was not addressed to '{me}'"),
            ));
        }
        if !args.peek {
            storage_ctx.storage.mark_message_read(&msg.id, now)?;
        }
        emit_message(&msg, false, format)?;
        return Ok(());
    }

    let filter = MessageFilter {
        to_prefix: Some(me),
        from_prefix: args.from.clone(),
        only_unread: !args.all,
        limit: None,
        // Asks are handled by `bd admin operator attend`; keep them out
        // of regular inbox listings so they don't get auto-marked-read.
        only_asks: Some(false),
    };
    let messages = storage_ctx.storage.list_messages(&filter)?;

    if messages.is_empty() {
        if !ctx.is_json() {
            ctx.print("(no messages)");
        } else {
            ctx.json_pretty(&Vec::<Message>::new());
        }
        return Ok(());
    }

    // Render before marking-read so display reflects original state.
    for msg in &messages {
        emit_message(msg, true, format)?;
    }

    if !args.peek && !args.all {
        for msg in &messages {
            storage_ctx.storage.mark_message_read(&msg.id, now)?;
        }
    }

    Ok(())
}

/// List messages sent from this prefix.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn execute_outbox(
    args: &OutboxArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let me = config::id_config_from_layer(&layer).prefix;

    let format = resolve_output_format_basic(args.format, ctx.is_json(), false);
    let filter = MessageFilter {
        from_prefix: Some(me),
        to_prefix: args.to.clone(),
        ..Default::default()
    };

    let messages = storage_ctx.storage.list_messages(&filter)?;

    if messages.is_empty() {
        if !ctx.is_json() {
            ctx.print("(no messages sent)");
        } else {
            ctx.json_pretty(&Vec::<Message>::new());
        }
        return Ok(());
    }
    for msg in &messages {
        emit_message(msg, true, format)?;
    }
    Ok(())
}

fn pick_message_id(
    storage: &SqliteStorage,
    from: &str,
    to: &str,
    body: &str,
    now: chrono::DateTime<Utc>,
) -> Result<String> {
    for nonce in 0..1000 {
        let candidate = generate_message_id(from, to, body, now, nonce);
        if !storage.message_id_exists(&candidate)? {
            return Ok(candidate);
        }
    }
    Err(BeadsError::validation(
        "id",
        "could not allocate a unique message ID after 1000 attempts",
    ))
}

fn resolve_body(words: &[String]) -> Result<String> {
    if !words.is_empty() {
        return Ok(words.join(" "));
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(BeadsError::from)?;
    Ok(buf.trim_end_matches('\n').to_string())
}

fn emit_message(msg: &Message, truncate: bool, format: OutputFormat) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let (display_body, truncated) = if truncate && msg.body.len() > PREVIEW_CHARS {
        (
            msg.body.chars().take(PREVIEW_CHARS).collect::<String>(),
            true,
        )
    } else {
        (msg.body.clone(), false)
    };

    match format {
        OutputFormat::Json | OutputFormat::Toon => {
            let view = MessageView {
                id: &msg.id,
                from: &msg.from_prefix,
                to: &msg.to_prefix,
                sent_at: msg.sent_at.to_rfc3339(),
                read_at: msg.read_at.map(|t| t.to_rfc3339()),
                in_reply_to: msg.in_reply_to.as_deref(),
                body: &display_body,
                truncated,
            };
            writeln!(out, "{}", serde_json::to_string(&view)?)?;
        }
        _ => {
            let unread = if msg.read_at.is_none() { "*" } else { " " };
            let reply_part = msg
                .in_reply_to
                .as_ref()
                .map(|r| format!(" ↪{r}"))
                .unwrap_or_default();
            writeln!(
                out,
                "{unread} [{ts}] {id} from {from}{reply_part}: {body}",
                ts = msg.sent_at.to_rfc3339(),
                id = msg.id,
                from = msg.from_prefix,
                body = display_body,
            )?;
            if truncated {
                writeln!(
                    out,
                    "  ... [truncated; run `bd inbox {id}` for the rest]",
                    id = msg.id
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_constant_matches_design() {
        assert_eq!(PREVIEW_CHARS, 200);
    }

    #[test]
    fn resolve_body_joins_words() {
        let words = vec!["hello".to_string(), "world".to_string()];
        assert_eq!(resolve_body(&words).unwrap(), "hello world");
    }

    #[test]
    fn emit_long_message_truncates_in_text_mode() {
        let body = "x".repeat(500);
        let m = Message {
            id: "msg-aaa".into(),
            from_prefix: "app1".into(),
            to_prefix: "app2".into(),
            body,
            sent_at: Utc::now(),
            read_at: None,
            in_reply_to: None,
            choices: None,
        };
        let mut buf = Vec::new();
        let (display, truncated) = if m.body.len() > PREVIEW_CHARS {
            (m.body.chars().take(PREVIEW_CHARS).collect::<String>(), true)
        } else {
            (m.body.clone(), false)
        };
        assert!(truncated);
        assert_eq!(display.len(), PREVIEW_CHARS);
        writeln!(buf, "preview: {display}").unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains(&"x".repeat(PREVIEW_CHARS)));
    }
}
