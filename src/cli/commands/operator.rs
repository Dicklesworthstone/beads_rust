//! `bd admin operator` — human-side triage of agent asks.
//!
//! `bd ask` deposits a row in `messages` with `to_prefix='operator'`
//! and a non-NULL `choices` field. This module pages through those,
//! lets the human answer (single keystroke for `--yn` / `--choices`,
//! `$EDITOR` for free-form), and posts the reply as a regular
//! message that the asking agent will see in its `bd watch` stream.
//!
//! Subcommands:
//! - `attend` — interactive REPL, the daily driver
//! - `list`   — one-shot print (scripting)
//! - `reply`  — one-shot send (scripting)
//!
//! REPL key conventions:
//! - Lowercase first-letter of each choice token → pick that choice
//! - `r` → reply (open `$EDITOR`)
//! - `q` → quit
//! - Enter (empty line) → skip, leave in queue
//!
//! `r` and `q` are reserved by [`bd ask`](crate::cli::commands::ask) —
//! it rejects choice tokens starting with those letters.

use crate::cli::{OperatorListArgs, OperatorReplyArgs};
use crate::config::{self, OPERATOR_PREFIX};
use crate::error::{BeadsError, Result};
use crate::model::Message;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::storage::messages::{MessageFilter, generate_message_id};
use chrono::{DateTime, Utc};
use std::io::{BufRead, Write};
use std::process::Command;

/// Run the interactive operator REPL.
///
/// # Errors
///
/// Returns an error if storage open/queries fail. The REPL itself
/// tolerates malformed input (loops back to the prompt).
pub fn execute_attend(cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    loop {
        let asks = pending_asks(&storage_ctx.storage)?;
        if asks.is_empty() {
            writeln!(out, "no asks pending.")?;
            return Ok(());
        }
        print_list(&mut out, &asks)?;
        write!(out, "> ")?;
        out.flush()?;

        let mut line = String::new();
        let n = input.read_line(&mut line)?;
        if n == 0 {
            // EOF (Ctrl-D)
            writeln!(out)?;
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // empty line → reprint list
            continue;
        }
        if matches!(trimmed, "q" | "quit") {
            return Ok(());
        }

        let Ok(n_one_based) = trimmed.parse::<usize>() else {
            writeln!(out, "  (type a number, or 'q' to quit)")?;
            continue;
        };
        if n_one_based == 0 || n_one_based > asks.len() {
            writeln!(out, "  (invalid selection)")?;
            continue;
        }
        let ask = asks[n_one_based - 1].clone();
        match handle_ask(&mut storage_ctx.storage, &mut input, &mut out, &ask, ctx)? {
            HandleOutcome::Quit => return Ok(()),
            HandleOutcome::Answered | HandleOutcome::Skip => continue,
        }
    }
}

/// One-shot list of pending asks.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn execute_list(
    args: &OperatorListArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let filter = MessageFilter {
        to_prefix: Some(OPERATOR_PREFIX.to_string()),
        only_unread: !args.all,
        only_asks: Some(true),
        ..Default::default()
    };
    let mut asks = storage_ctx.storage.list_messages(&filter)?;
    asks.sort_by_key(|m| m.sent_at);

    if ctx.is_json() {
        ctx.json_pretty(&asks);
        return Ok(());
    }
    if asks.is_empty() {
        ctx.print("no asks pending.");
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    print_list(&mut out, &asks)?;
    Ok(())
}

/// Send a one-shot reply to an ask without entering the REPL.
///
/// # Errors
///
/// Returns an error if the ask ID doesn't exist, isn't addressed to
/// `operator`, or DB writes fail.
pub fn execute_reply(
    args: &OperatorReplyArgs,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let ask = storage_ctx
        .storage
        .get_message(&args.id)?
        .ok_or_else(|| BeadsError::validation("id", format!("no such message: {}", args.id)))?;
    if ask.to_prefix != OPERATOR_PREFIX {
        return Err(BeadsError::validation(
            "id",
            format!("{} is not an operator ask", args.id),
        ));
    }

    let body = if args.body.is_empty() {
        let mut buf = String::new();
        std::io::stdin()
            .read_line(&mut buf)
            .map_err(BeadsError::from)?;
        buf.trim().to_string()
    } else {
        args.body.join(" ")
    };
    if body.trim().is_empty() {
        return Err(BeadsError::validation("body", "reply body is empty"));
    }

    let now = Utc::now();
    let reply = build_reply(&storage_ctx.storage, &ask, body, now)?;
    storage_ctx.storage.insert_message(&reply)?;
    storage_ctx.storage.mark_message_read(&ask.id, now)?;

    if ctx.is_json() {
        ctx.json_pretty(&reply);
    } else {
        ctx.success(&format!("replied to {} (msg {})", ask.id, reply.id));
    }
    Ok(())
}

enum HandleOutcome {
    Answered,
    Skip,
    Quit,
}

fn handle_ask<R: BufRead, W: Write>(
    storage: &mut SqliteStorage,
    input: &mut R,
    out: &mut W,
    ask: &Message,
    _ctx: &OutputContext,
) -> Result<HandleOutcome> {
    let choices = parse_choices(ask.choices.as_deref().unwrap_or(""));

    loop {
        let age = format_age_compact(seconds_since(ask.sent_at));
        writeln!(out)?;
        writeln!(out, "  {} ({}, from {}):", ask.id, age, ask.from_prefix)?;
        for line in ask.body.lines() {
            writeln!(out, "    {line}")?;
        }
        let prompt = render_prompt(&choices);
        write!(out, "  {prompt} > ")?;
        out.flush()?;

        let mut line = String::new();
        let n = input.read_line(&mut line)?;
        if n == 0 {
            writeln!(out)?;
            return Ok(HandleOutcome::Quit);
        }
        let trimmed = line.trim().to_ascii_lowercase();

        if trimmed.is_empty() {
            // skip — leave unread in queue
            return Ok(HandleOutcome::Skip);
        }
        if matches!(trimmed.as_str(), "q" | "quit") {
            return Ok(HandleOutcome::Quit);
        }
        if matches!(trimmed.as_str(), "r" | "reply") {
            match open_editor_for_reply(ask)? {
                Some(body) => {
                    let now = Utc::now();
                    let reply = build_reply(storage, ask, body, now)?;
                    storage.insert_message(&reply)?;
                    storage.mark_message_read(&ask.id, now)?;
                    writeln!(out, "  → replied (msg {})", reply.id)?;
                    return Ok(HandleOutcome::Answered);
                }
                None => {
                    writeln!(out, "  (empty body — leaving ask in queue)")?;
                    return Ok(HandleOutcome::Skip);
                }
            }
        }

        if let Some(chosen) = match_choice(&trimmed, &choices) {
            let now = Utc::now();
            let reply = build_reply(storage, ask, chosen.clone(), now)?;
            storage.insert_message(&reply)?;
            storage.mark_message_read(&ask.id, now)?;
            writeln!(out, "  → answered '{chosen}'")?;
            return Ok(HandleOutcome::Answered);
        }

        writeln!(out, "  (unrecognized — try a hotkey, 'r', 'q', or Enter)")?;
    }
}

fn parse_choices(raw: &str) -> Vec<String> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    raw.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn match_choice(input: &str, choices: &[String]) -> Option<String> {
    // Full-token match wins, single-letter match falls back.
    if let Some(exact) = choices.iter().find(|c| c.as_str() == input) {
        return Some(exact.clone());
    }
    if input.chars().count() == 1 {
        let ch = input.chars().next().unwrap();
        if let Some(by_letter) = choices.iter().find(|c| c.starts_with(ch)) {
            return Some(by_letter.clone());
        }
    }
    None
}

fn render_prompt(choices: &[String]) -> String {
    let mut parts: Vec<String> = choices
        .iter()
        .map(|c| {
            let first = c.chars().next().unwrap_or('?');
            let rest: String = c.chars().skip(1).collect();
            format!("[{first}]{rest}")
        })
        .collect();
    parts.push("[r]eply".to_string());
    parts.push("[q]uit".to_string());
    let core = parts.join(" / ");
    format!("{core}  (Enter to skip)")
}

fn print_list<W: Write>(out: &mut W, asks: &[Message]) -> Result<()> {
    writeln!(out, "pending asks ({}):", asks.len())?;
    for (i, ask) in asks.iter().enumerate() {
        let age = format_age_compact(seconds_since(ask.sent_at));
        let mode = match ask.choices.as_deref() {
            Some("") | None => "[reply]".to_string(),
            Some(csv) => {
                let letters: Vec<String> = csv
                    .split(',')
                    .filter_map(|t| t.trim().chars().next())
                    .map(|c| c.to_string())
                    .collect();
                format!("[{}]", letters.join("/"))
            }
        };
        let preview = first_line(&ask.body, 60);
        writeln!(
            out,
            "  {i_one}. [{age:>5}] {from:<10} {preview:<62} {mode}",
            i_one = i + 1,
            age = age,
            from = ask.from_prefix,
            preview = preview,
            mode = mode,
        )?;
    }
    Ok(())
}

fn first_line(body: &str, max: usize) -> String {
    let line = body.lines().next().unwrap_or("");
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut truncated: String = line.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

fn pending_asks(storage: &SqliteStorage) -> Result<Vec<Message>> {
    let filter = MessageFilter {
        to_prefix: Some(OPERATOR_PREFIX.to_string()),
        only_unread: true,
        only_asks: Some(true),
        ..Default::default()
    };
    let mut asks = storage.list_messages(&filter)?;
    asks.sort_by_key(|m| m.sent_at);
    Ok(asks)
}

fn build_reply(
    storage: &SqliteStorage,
    ask: &Message,
    body: String,
    now: DateTime<Utc>,
) -> Result<Message> {
    let id = pick_reply_id(storage, &ask.to_prefix, &ask.from_prefix, &body, now)?;
    Ok(Message {
        id,
        from_prefix: OPERATOR_PREFIX.to_string(),
        to_prefix: ask.from_prefix.clone(),
        body,
        sent_at: now,
        read_at: None,
        in_reply_to: Some(ask.id.clone()),
        choices: None,
    })
}

fn pick_reply_id(
    storage: &SqliteStorage,
    from: &str,
    to: &str,
    body: &str,
    now: DateTime<Utc>,
) -> Result<String> {
    for nonce in 0..1000 {
        let candidate = generate_message_id(from, to, body, now, nonce);
        if !storage.message_id_exists(&candidate)? {
            return Ok(candidate);
        }
    }
    Err(BeadsError::validation(
        "id",
        "could not allocate a unique reply ID after 1000 attempts",
    ))
}

/// Spawn `$EDITOR` (falling back to `vim`/`nano`) on a tmpfile,
/// returning the trimmed body or `None` if the user saved empty.
fn open_editor_for_reply(ask: &Message) -> Result<Option<String>> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());

    let tmpdir = std::env::temp_dir();
    let path = tmpdir.join(format!("bd-reply-{}.md", ask.id));
    let mut header = String::new();
    header.push_str("# Reply to ");
    header.push_str(&ask.id);
    header.push_str(" from ");
    header.push_str(&ask.from_prefix);
    header.push_str(
        "\n# Lines starting with '#' are stripped. Save empty to skip.\n#\n# Original ask:\n",
    );
    for line in ask.body.lines() {
        header.push_str("#   ");
        header.push_str(line);
        header.push('\n');
    }
    header.push('\n');
    std::fs::write(&path, header)?;

    let status = Command::new(&editor).arg(&path).status();
    match status {
        Ok(st) if st.success() => {}
        Ok(st) => {
            return Err(BeadsError::validation(
                "editor",
                format!("editor exited with status {st}"),
            ));
        }
        Err(e) => {
            return Err(BeadsError::validation(
                "editor",
                format!("failed to spawn '{editor}': {e}"),
            ));
        }
    }

    let raw = std::fs::read_to_string(&path)?;
    // Best-effort cleanup; ignore failure.
    let _ = std::fs::remove_file(&path);

    let body: String = raw
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = body.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

fn seconds_since(t: DateTime<Utc>) -> i64 {
    Utc::now().signed_duration_since(t).num_seconds()
}

fn format_age_compact(secs: i64) -> String {
    if secs < 0 {
        return "now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let m = secs / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h");
    }
    let d = h / 24;
    if d < 7 {
        return format!("{d}d");
    }
    let w = d / 7;
    format!("{w}w")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_choices_handles_empty() {
        assert!(parse_choices("").is_empty());
        assert!(parse_choices("   ").is_empty());
    }

    #[test]
    fn parse_choices_splits_csv() {
        assert_eq!(parse_choices("y,n"), vec!["y", "n"]);
        assert_eq!(
            parse_choices("safe, bold ,abort"),
            vec!["safe", "bold", "abort"]
        );
    }

    #[test]
    fn match_choice_full_token() {
        let cs = parse_choices("safe,bold,abort");
        assert_eq!(match_choice("safe", &cs).as_deref(), Some("safe"));
        assert_eq!(match_choice("bold", &cs).as_deref(), Some("bold"));
        assert_eq!(match_choice("nope", &cs), None);
    }

    #[test]
    fn match_choice_single_letter() {
        let cs = parse_choices("y,n");
        assert_eq!(match_choice("y", &cs).as_deref(), Some("y"));
        assert_eq!(match_choice("n", &cs).as_deref(), Some("n"));

        let cs = parse_choices("safe,bold,abort");
        assert_eq!(match_choice("s", &cs).as_deref(), Some("safe"));
        assert_eq!(match_choice("a", &cs).as_deref(), Some("abort"));
    }

    #[test]
    fn render_prompt_appends_reply_quit() {
        let cs = parse_choices("y,n");
        let p = render_prompt(&cs);
        assert!(p.contains("[y]"));
        assert!(p.contains("[n]"));
        assert!(p.contains("[r]eply"));
        assert!(p.contains("[q]uit"));
        assert!(p.contains("Enter to skip"));
    }

    #[test]
    fn render_prompt_freeform_has_no_choice_brackets() {
        let cs: Vec<String> = parse_choices("");
        let p = render_prompt(&cs);
        // No choice hotkeys, just reply / quit / skip
        assert!(p.starts_with("[r]eply"));
    }

    #[test]
    fn first_line_truncates_with_ellipsis() {
        let s = first_line(&"x".repeat(100), 20);
        assert_eq!(s.chars().count(), 20);
        assert!(s.ends_with('…'));
    }
}
