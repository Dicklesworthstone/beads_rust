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

    let body = resolve_body(args)?;

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

    let body = resolve_body(args)?;

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

/// Resolve the message body from the positional words or `--file`,
/// mirroring `bd comments add`'s flag semantics with one deliberate
/// difference (documented on [`crate::cli::MsgArgs::file`] and below).
///
/// Sources, in the order they're distinguished:
///
/// - **both** a positional body and `--file`: a usage error. Two bodies
///   supplied at once must never be resolved by silently preferring
///   one — that is exactly the kind of ambiguity this surface exists to
///   refuse.
/// - **positional, single `-`**: the conventional stdin marker. This is
///   the fix for the historic footgun where a piped `bd msg to -`
///   stored the literal two-byte string `"-"` and reported success —
///   three real sends were lost to it before anyone noticed. Multi-word
///   positional bodies are unaffected (`bd msg to - decided` is text
///   starting with a dash, not a stdin request, matching `bd comments
///   add`'s treatment of a leading dash as content).
/// - **positional, anything else**: literal text, `join(" ")`,
///   unchanged from today.
/// - **`--file -`**: read from stdin.
/// - **`--file <path>`**: read that file. Any content sitting unread on
///   stdin is deliberately ignored — the explicit flag always wins over
///   an unread pipe rather than blending the two or guessing.
/// - **neither**: read from stdin (today's `bd msg <target> < file`
///   path, unchanged).
///
/// All three stdin-shaped forms above, plus `--file <path>`, normalize
/// identically (trailing `\n` stripped, matching what the bare-stdin
/// path has always done) so the same body produces the same stored
/// message regardless of which of the four carried it in. This is the
/// one place this command's semantics diverge from `bd comments add`'s
/// `--file`, which reads raw and keeps the trailing newline: `bd
/// comments add` has no pre-existing bare-stdin behavior to stay
/// consistent with, `bd msg` does, and byte-for-byte agreement with
/// that existing path was called out explicitly as a requirement.
/// Literal positional text is never normalized this way, matching
/// today's behavior exactly.
///
/// Every source is checked for an empty result (after trimming
/// whitespace) and refused by name — an empty send is never intended,
/// and silently accepting one is the exact hole a mandatory shell-side
/// `[ -n "$BODY" ]` guard exists to plug for exactly this command.
///
/// # Errors
///
/// Returns a validation error if both a body and `--file` are given, if
/// `--file` names a path that cannot be read, if the resolved body is
/// empty, or if a stdin read is attempted while stdin is a terminal.
fn resolve_body(args: &MsgArgs) -> Result<String> {
    let has_positional = !args.body.is_empty();

    let (raw, label): (String, String) = match (has_positional, args.file.as_deref()) {
        (true, Some(_)) => {
            return Err(BeadsError::validation(
                "body",
                "provide either a message body or --file, not both",
            ));
        }
        (true, None) => {
            if args.body.len() == 1 && args.body[0] == "-" {
                (read_stdin_body()?, "stdin".to_string())
            } else {
                let text = args.body.join(" ");
                if text.trim().is_empty() {
                    return Err(BeadsError::validation("body", "message body is empty"));
                }
                return Ok(text);
            }
        }
        (false, Some(path)) if path == std::path::Path::new("-") => {
            (read_stdin_body()?, "stdin".to_string())
        }
        (false, Some(path)) => {
            let content = std::fs::read_to_string(path).map_err(|e| {
                BeadsError::validation("file", format!("cannot read {}: {e}", path.display()))
            })?;
            (
                normalize_stream_body(&content),
                format!("--file {}", path.display()),
            )
        }
        (false, None) => (read_stdin_body()?, "stdin".to_string()),
    };

    if raw.trim().is_empty() {
        return Err(BeadsError::validation(
            "body",
            format!("message body is empty (read from {label})"),
        ));
    }
    Ok(raw)
}

/// Read the message body from stdin, refusing a terminal outright.
///
/// A bare `-` (or an omitted body, or `--file -`) reading from an
/// attached terminal has no content to receive: it is neither a
/// literal dash nor something to wait on — the historic bug this
/// command is being fixed for is exactly a case where an ambiguous
/// input was resolved by guessing instead of refusing, so the terminal
/// case is refused rather than left to hang or to silently read
/// whatever is typed.
fn read_stdin_body() -> Result<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return Err(BeadsError::validation(
            "body",
            "refusing to read the message body from a terminal — pipe input, \
             redirect a file, or pass --file <path>",
        ));
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(BeadsError::from)?;
    Ok(normalize_stream_body(&buf))
}

/// Strip trailing newlines the same way the bare-stdin path always has,
/// so every stream-shaped body source agrees byte-for-byte.
fn normalize_stream_body(raw: &str) -> String {
    raw.trim_end_matches('\n').to_string()
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
        let args = MsgArgs {
            to: "target".to_string(),
            body: vec!["hello".to_string(), "world".to_string()],
            ..MsgArgs::default()
        };
        assert_eq!(resolve_body(&args).unwrap(), "hello world");
    }

    #[test]
    fn resolve_body_rejects_body_and_file_together() {
        let args = MsgArgs {
            to: "target".to_string(),
            body: vec!["inline".to_string()],
            file: Some(std::path::PathBuf::from("/dev/null")),
            ..MsgArgs::default()
        };
        let err = resolve_body(&args).unwrap_err();
        assert!(
            err.to_string().contains("not both"),
            "expected a not-both usage error, got: {err}"
        );
    }

    #[test]
    fn resolve_body_rejects_empty_literal() {
        let args = MsgArgs {
            to: "target".to_string(),
            body: vec!["   ".to_string()],
            ..MsgArgs::default()
        };
        let err = resolve_body(&args).unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "expected an empty-body error, got: {err}"
        );
    }

    #[test]
    fn resolve_body_rejects_missing_file() {
        let args = MsgArgs {
            to: "target".to_string(),
            file: Some(std::path::PathBuf::from("/no/such/path/for/msg/test")),
            ..MsgArgs::default()
        };
        let err = resolve_body(&args).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("/no/such/path/for/msg/test"),
            "error must name the missing path, got: {text}"
        );
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
