//! Ephemeral messaging commands: msg / inbox / outbox.
//!
//! Messages are NOT issues. They round-trip locally only, expire after
//! a TTL once read, and never enter the issue work-list.
//!
//! Sender identity comes from `BD_AGENT_ID`, with a fallback: when it's
//! unset, identity is inferred from a live `bd watch` in this process's
//! ancestry (see [`config::resolve_agent_identity_with_storage`]).
//! Project config / default-`"bd"` fallbacks are deliberately *not*
//! honored here — a prefix-less environment used to silently send as
//! `"bd"`, which made operator messages appear to come from a phantom
//! agent. If no identity can be determined by either means, `bd msg`
//! errors out; the operator's send path is the separate `bd admin msg`
//! command, which forces `from = operator`.

use crate::cli::{InboxArgs, MsgArgs, OutboxArgs, OutputFormat, resolve_output_format_basic};
use crate::config::{self, OPERATOR_PREFIX};
use crate::error::{BeadsError, Result};
use crate::model::Message;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::storage::messages::{MessageFilter, generate_message_id};
use chrono::Utc;
use serde::Serialize;
use std::io::{Read, Write};

/// Preview length for the human-readable text listing, where a short
/// snippet keeps `bd inbox` scannable and the reader is told to re-fetch
/// the full body by id.
const PREVIEW_CHARS: usize = 200;
/// Preview length for structured (JSON / TOON) output. These formats are
/// consumed programmatically — typically by another agent — so a full
/// bead-length message must survive. We keep a very generous cap only as a
/// guard against pathologically huge bodies; anything under it is emitted
/// whole.
const STRUCTURED_PREVIEW_CHARS: usize = 100_000;
/// The structured cap must stay above the text cap, or machine consumers
/// would get *less* than humans. Enforced at compile time rather than in a
/// test: both operands are constants, so there is nothing to observe at
/// runtime that the compiler cannot decide now.
const _: () = assert!(STRUCTURED_PREVIEW_CHARS > PREVIEW_CHARS);
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

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    // Identity resolution needs storage now (the inference fallback
    // reads the watchers table), so this must come after `open_storage`.
    let from = config::resolve_agent_identity_with_storage(&storage_ctx.storage)?;

    let body = resolve_body(&args.body)?;
    if body.trim().is_empty() {
        return Err(BeadsError::validation("body", "message body is empty"));
    }

    if let Some(reply) = &args.reply {
        if storage_ctx.storage.get_message(reply)?.is_none() {
            return Err(BeadsError::validation(
                "reply",
                format!("no such message: {reply}"),
            ));
        }
    }

    let now = Utc::now();

    send_message(
        &mut storage_ctx.storage,
        SendParams {
            from: &from,
            to,
            body,
            reply: args.reply.as_deref(),
            force: args.force,
            require_recipient_online: true,
        },
        now,
        ctx,
    )
}

struct SendParams<'a> {
    from: &'a str,
    to: &'a str,
    body: String,
    reply: Option<&'a str>,
    force: bool,
    /// True for agent `bd msg` (which gates on the recipient having
    /// an active `bd watch` to surface typos); false for `bd admin msg`
    /// (the operator is allowed to message anyone).
    require_recipient_online: bool,
}

fn send_message(
    storage: &mut SqliteStorage,
    p: SendParams<'_>,
    now: chrono::DateTime<Utc>,
    ctx: &OutputContext,
) -> Result<()> {
    // Typo guard: reject messages to prefixes with no active `bd watch`
    // heartbeat (`bd msg infra` when the watcher is `infra1`). Skip when
    // --force is set, when replying to a real message, when messaging
    // your own prefix (testing), when the recipient is the operator
    // (always-listed-but-not-a-watcher), or when explicitly disabled by
    // the admin path.
    let recipient_is_operator = p.to.eq_ignore_ascii_case(OPERATOR_PREFIX);
    if p.require_recipient_online
        && !p.force
        && p.reply.is_none()
        && p.to != p.from
        && !recipient_is_operator
    {
        let ttl = crate::storage::watchers::WATCHER_TTL_SECONDS;
        let _ = storage.sweep_stale_watchers(now, ttl);
        if !storage.is_prefix_watched(p.to, now, ttl)? {
            let active = storage.active_watcher_prefixes(now, ttl)?;
            let hint = if active.is_empty() {
                "no agents are currently watching. If this isn't a typo, \
                 flag it to the operator with `bd msg operator`."
                    .to_string()
            } else {
                format!(
                    "active watchers: {}. If this isn't a typo, flag it to \
                     the operator with `bd msg operator`.",
                    active.join(", ")
                )
            };
            return Err(BeadsError::validation(
                "to",
                format!("no active `bd watch` for '{to}' — {hint}", to = p.to),
            ));
        }
    }

    let id = pick_message_id(storage, p.from, p.to, &p.body, now)?;
    let msg = Message {
        id: id.clone(),
        from_prefix: p.from.to_string(),
        to_prefix: p.to.to_string(),
        body: p.body,
        sent_at: now,
        read_at: None,
        in_reply_to: p.reply.map(str::to_string),
        choices: None,
    };

    storage.insert_message(&msg)?;

    if ctx.is_json() {
        ctx.json_pretty(&msg);
    } else {
        ctx.success(&format!("Sent {} to {}", msg.id, msg.to_prefix));
    }
    Ok(())
}

/// `bd admin msg <to> <body>` — operator's send path. Identifies the
/// sender as `operator` regardless of `BD_AGENT_ID`. The typo
/// guard is dropped: the operator may legitimately want to drop a
/// message for an agent that isn't watching yet (will be picked up
/// next time they boot `bd watch`).
///
/// # Errors
///
/// Returns an error if the recipient/body are invalid or DB writes fail.
pub fn execute_admin_msg(
    args: &MsgArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let to = args.to.trim();
    if to.is_empty() {
        return Err(BeadsError::validation("to", "recipient prefix is required"));
    }
    if to.eq_ignore_ascii_case(OPERATOR_PREFIX) {
        return Err(BeadsError::validation(
            "to",
            "you cannot send a message to yourself — 'operator' is the \
             reserved sender prefix for this command",
        ));
    }

    let body = resolve_body(&args.body)?;
    if body.trim().is_empty() {
        return Err(BeadsError::validation("body", "message body is empty"));
    }

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    if let Some(reply) = &args.reply {
        if storage_ctx.storage.get_message(reply)?.is_none() {
            return Err(BeadsError::validation(
                "reply",
                format!("no such message: {reply}"),
            ));
        }
    }

    let now = Utc::now();
    send_message(
        &mut storage_ctx.storage,
        SendParams {
            from: OPERATOR_PREFIX,
            to,
            body,
            reply: args.reply.as_deref(),
            force: args.force,
            require_recipient_online: false,
        },
        now,
        ctx,
    )
}

/// List received messages, or show one in full.
///
/// The viewer's identity comes from `BD_AGENT_ID`, falling back to
/// live-`bd watch`-ancestry inference when unset (see
/// [`config::resolve_agent_identity_with_storage`]).
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
    let me = config::resolve_agent_identity_with_storage(&storage_ctx.storage)?;
    execute_inbox_as(&me, args, &mut storage_ctx, ctx)
}

/// `bd admin inbox` — the operator's inbox view.
///
/// # Errors
///
/// Returns an error if the DB query fails or the requested message ID is unknown.
pub fn execute_admin_inbox(
    args: &InboxArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    execute_inbox_as(OPERATOR_PREFIX, args, &mut storage_ctx, ctx)
}

fn execute_inbox_as(
    me: &str,
    args: &InboxArgs,
    storage_ctx: &mut config::OpenStorageResult,
    ctx: &OutputContext,
) -> Result<()> {
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
        to_prefix: Some(me.to_string()),
        from_prefix: args.from.clone(),
        only_unread: !args.all,
        limit: None,
        only_asks: None,
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
    let me = config::resolve_agent_identity_with_storage(&storage_ctx.storage)?;
    execute_outbox_as(&me, args, &storage_ctx, ctx)
}

/// `bd admin outbox` — list messages sent *as* the operator.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn execute_admin_outbox(
    args: &OutboxArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    execute_outbox_as(OPERATOR_PREFIX, args, &storage_ctx, ctx)
}

fn execute_outbox_as(
    me: &str,
    args: &OutboxArgs,
    storage_ctx: &config::OpenStorageResult,
    ctx: &OutputContext,
) -> Result<()> {
    let format = resolve_output_format_basic(args.format, ctx.is_json(), false);
    let filter = MessageFilter {
        from_prefix: Some(me.to_string()),
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

/// Maximum body length (in characters) shown before a message is
/// truncated, chosen per output format. Structured formats consumed by
/// other agents get a very generous cap so bead-length bodies survive
/// intact; the human text listing stays short and scannable.
fn preview_limit_for(format: OutputFormat) -> usize {
    match format {
        OutputFormat::Json | OutputFormat::Toon => STRUCTURED_PREVIEW_CHARS,
        _ => PREVIEW_CHARS,
    }
}

fn emit_message(msg: &Message, truncate: bool, format: OutputFormat) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // Structured consumers (JSON / TOON) need the whole message; the short
    // 200-char preview is only appropriate for the scannable text listing.
    let preview_limit = preview_limit_for(format);

    let (display_body, truncated) = if truncate && msg.body.chars().count() > preview_limit {
        (
            msg.body.chars().take(preview_limit).collect::<String>(),
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
    fn structured_formats_get_a_generous_preview_limit() {
        // Human text stays short and scannable.
        assert_eq!(preview_limit_for(OutputFormat::Text), PREVIEW_CHARS);
        // Machine formats consumed by other agents keep full bodies.
        assert_eq!(preview_limit_for(OutputFormat::Json), STRUCTURED_PREVIEW_CHARS);
        assert_eq!(preview_limit_for(OutputFormat::Toon), STRUCTURED_PREVIEW_CHARS);
        // (STRUCTURED > TEXT is asserted at compile time next to the
        // constants themselves.)
    }

    #[test]
    fn bead_length_message_survives_in_json_and_toon() {
        // A realistic bead-length body: well over the text preview but under
        // the structured cap, so an agent reading its inbox in JSON/TOON must
        // receive it whole and un-truncated.
        let body = "x".repeat(4_000);
        for format in [OutputFormat::Json, OutputFormat::Toon] {
            let limit = preview_limit_for(format);
            let truncated = body.chars().count() > limit;
            assert!(
                !truncated,
                "bead-length body must not truncate in {format:?}"
            );
        }
        // The same body IS truncated in the human text listing.
        assert!(body.chars().count() > preview_limit_for(OutputFormat::Text));
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
