//! `bd ask` — send a question to the human operator.
//!
//! An ask is just a row in the `messages` table addressed to the
//! reserved [`OPERATOR_PREFIX`](crate::config::OPERATOR_PREFIX) with a
//! `choices` field that tells `bd admin operator attend` how to render
//! the answer prompt:
//!
//! - `None`         → not an ask, just a regular message (use `bd msg`)
//! - `Some("")`     → free-form ask: operator answers via `$EDITOR`
//! - `Some("y,n")`  → binary ask: operator hits `y` or `n`
//! - `Some("a,b")`  → general ask: one hotkey per token's first letter
//!
//! Operator-side: `bd admin operator attend` pages through unread
//! asks (`to_prefix='operator' AND read_at IS NULL AND choices IS NOT NULL`).
//! Agents pick up replies via their normal inbox / `bd watch` stream.
//!
//! Convention: an agent that hits a wall should `bd ask`, then `bd idle`,
//! then end its current task. The reply will surface in the next `bd
//! watch` event when the agent resumes. There is no `--wait` flag —
//! that's by design, to avoid two agents deadlocking on each other.

use crate::cli::AskArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::Message;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::storage::messages::generate_message_id;
use chrono::Utc;
use std::collections::HashSet;
use std::io::Read;

/// Execute `bd ask`.
///
/// # Errors
///
/// Returns an error when the body is empty, `--choices` is malformed,
/// the sender prefix resolves to `operator`, or DB writes fail.
pub fn execute(args: &AskArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let body = resolve_body(&args.body)?;
    if body.trim().is_empty() {
        return Err(BeadsError::validation("body", "question body is empty"));
    }

    let choices = resolve_choices(args)?;

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let layer = config::load_config(&beads_dir, Some(&storage_ctx.storage), cli)?;
    let from = config::id_config_from_layer(&layer).prefix;

    if from.eq_ignore_ascii_case(config::OPERATOR_PREFIX) {
        return Err(BeadsError::validation(
            "from",
            "the operator cannot ask the operator; check BD_ISSUE_PREFIX",
        ));
    }

    let now = Utc::now();
    let id = pick_message_id(&storage_ctx.storage, &from, config::OPERATOR_PREFIX, &body, now)?;
    let msg = Message {
        id: id.clone(),
        from_prefix: from,
        to_prefix: config::OPERATOR_PREFIX.to_string(),
        body,
        sent_at: now,
        read_at: None,
        in_reply_to: None,
        choices: Some(choices),
    };

    storage_ctx.storage.insert_message(&msg)?;

    if ctx.is_json() {
        ctx.json_pretty(&msg);
    } else {
        ctx.success(&format!("Asked operator: {}", msg.id));
    }
    Ok(())
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

/// Validate & canonicalize the `choices` field stored on the message.
/// Free-form asks → `""`; `--yn` → `"y,n"`; `--choices a,b,c` → `"a,b,c"`
/// after trimming and uniqueness checks.
fn resolve_choices(args: &AskArgs) -> Result<String> {
    if args.yn {
        return Ok("y,n".to_string());
    }
    let Some(raw) = &args.choices else {
        return Ok(String::new());
    };

    let mut tokens: Vec<String> = Vec::new();
    let mut firsts: HashSet<char> = HashSet::new();
    for piece in raw.split(',') {
        let token = piece.trim().to_ascii_lowercase();
        if token.is_empty() {
            return Err(BeadsError::validation(
                "choices",
                "empty choice token (check for stray commas)",
            ));
        }
        if !token.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(BeadsError::validation(
                "choices",
                format!("choice token '{token}' must be ASCII alphanumeric"),
            ));
        }
        let first = token.chars().next().expect("non-empty checked above");
        if first == 'r' || first == 'q' {
            return Err(BeadsError::validation(
                "choices",
                format!(
                    "choice tokens cannot start with 'r' or 'q' — those are \
                     reserved for the operator REPL (reply / quit). Got '{token}'"
                ),
            ));
        }
        if !firsts.insert(first) {
            return Err(BeadsError::validation(
                "choices",
                format!(
                    "choice tokens must have unique first letters \
                     (duplicate '{first}' in '{raw}')"
                ),
            ));
        }
        tokens.push(token);
    }
    if tokens.len() < 2 {
        return Err(BeadsError::validation(
            "choices",
            "supply at least two choices, or omit --choices for a free-form ask",
        ));
    }
    Ok(tokens.join(","))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args_with(yn: bool, choices: Option<&str>) -> AskArgs {
        AskArgs {
            body: vec!["q".to_string()],
            yn,
            choices: choices.map(String::from),
        }
    }

    #[test]
    fn yn_canonicalizes() {
        assert_eq!(resolve_choices(&args_with(true, None)).unwrap(), "y,n");
    }

    #[test]
    fn freeform_is_empty_string() {
        assert_eq!(resolve_choices(&args_with(false, None)).unwrap(), "");
    }

    #[test]
    fn choices_trim_and_lowercase() {
        let r = resolve_choices(&args_with(false, Some("Safe , Bold , Abort"))).unwrap();
        assert_eq!(r, "safe,bold,abort");
    }

    #[test]
    fn rejects_duplicate_first_letter() {
        assert!(resolve_choices(&args_with(false, Some("safe,sad"))).is_err());
    }

    #[test]
    fn rejects_single_choice() {
        assert!(resolve_choices(&args_with(false, Some("yes"))).is_err());
    }

    #[test]
    fn rejects_empty_token() {
        assert!(resolve_choices(&args_with(false, Some("a,,b"))).is_err());
    }

    #[test]
    fn rejects_non_alphanumeric() {
        assert!(resolve_choices(&args_with(false, Some("a,b-c"))).is_err());
    }

    #[test]
    fn rejects_reserved_first_letters() {
        assert!(resolve_choices(&args_with(false, Some("safe,reload"))).is_err());
        assert!(resolve_choices(&args_with(false, Some("safe,quit"))).is_err());
    }
}
