use beads_rust::cli::commands;
use beads_rust::cli::{Cli, Commands};
use beads_rust::config;
use beads_rust::exit::{ExitKind, exit_with_status};
use beads_rust::logging::init_logging;
use beads_rust::output::OutputContext;
use beads_rust::sync::{auto_flush, auto_import_if_stale};
use beads_rust::{BeadsError, Result, StructuredError};
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use std::io::{self, IsTerminal, Write as _};
use std::path::Path;
use tracing::debug;

/// Exit quietly instead of aborting when stdout disappears mid-write.
///
/// `bd list | head` is the most ordinary thing a user or an agent does, and
/// before this guard it crashed the process every single time.
///
/// The chain: the Rust runtime sets `SIGPIPE` to `SIG_IGN` before `main`, so a
/// write to a pipe whose reader has exited returns `EPIPE` rather than killing
/// the process. `println!` treats a failed stdout write as unrecoverable and
/// panics ("failed printing to stdout"). Under `[profile.release]`'s
/// `panic = "abort"` that panic becomes `abort()`, so the process dies on
/// `SIGABRT` and the kernel writes a multi-megabyte core.
///
/// It was invisible in normal use, which is why it survived: a pipeline
/// reports the *last* command's exit status, so the shell said `0` while the
/// process was dying. Over 100 such cores (~288 MB) had piled up on one
/// machine before anyone noticed, and nothing on the terminal ever said a
/// word.
///
/// # Why a panic hook rather than restoring `SIG_DFL`
///
/// The conventional fix is `signal(SIGPIPE, SIG_DFL)`, but this crate sets
/// `unsafe_code = "forbid"` (and `src/lib.rs` repeats `#![forbid(unsafe_code)]`)
/// — a deliberate invariant, and `forbid` cannot be locally overridden. Doing
/// it safely would mean taking on a new dependency purely to hide one `unsafe`
/// call, which is a poor trade right before a release. A panic hook needs
/// neither: it runs *before* the abort under `panic = "abort"`, so exiting
/// from it pre-empts the core dump entirely, and it covers every one of the
/// ~355 `println!` sites in the tree at once rather than only the ones someone
/// remembered to convert.
///
/// # Why exit 0
///
/// The reader closing early is the reader's own choice (`head` does it by
/// design), so the caller already knows the output was truncated — nothing is
/// being hidden from them. Dying by `SIGPIPE` would instead surface as `141`
/// under `set -o pipefail`, turning a completely normal idiom into a spurious
/// failure for every agent that pipes `bd` into `head` or `jq`. If a future
/// maintainer prefers strict unix convention, changing the `0` below to `141`
/// is the whole edit.
///
/// # Interaction with auto-flush
///
/// Exiting at a write means a mutating command can stop before its post-command
/// JSONL auto-flush runs. That is safe and self-healing: `auto_flush` gates on
/// `get_dirty_issue_count()` and dirty flags are cleared only after a
/// successful export, so the next mutating command re-exports the pending
/// rows. The mutation itself was committed to SQLite before any output was
/// printed. Nor is it a new exposure — the previous behaviour (`abort()`)
/// skipped the flush just as abruptly, only with a core dump attached.
///
/// # The failure banner, and why the order inside this hook matters
///
/// This hook also covers the one nonzero exit path that is not a
/// `std::process::exit` call: a fatal panic. The broken-pipe check must stay
/// **first**. `br list | head -1` is the most common pipeline in this fleet and
/// it arrives here as a panic; if the banner were emitted for "a panic
/// happened" rather than for the status being exited with, that entirely normal
/// command would start printing `br: FAILED (PANIC, ...)`.
///
/// The rule that keeps this correct is in `beads_rust::exit`: the banner is a
/// function of the exit status. The broken-pipe branch exits **0** and so is
/// silent, by the same rule that makes every other zero exit silent — not by a
/// special case anyone has to remember.
fn install_broken_pipe_guard() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if is_broken_pipe_panic(info) {
            // Status 0: a reader that left early is not a failure, so this
            // emits no banner. Routed through the funnel so that stays true
            // for the same reason it is true everywhere else.
            exit_with_status(0, ExitKind::Notice, "BROKEN_PIPE");
        }
        default_hook(info);

        // After the default hook, so the banner is genuinely the last line:
        // the panic message is already on stderr and nothing else will be
        // written before the process dies.
        if beads_rust::exit::panic_is_fatal() {
            beads_rust::exit::emit_exit_banner(
                ExitKind::Failure,
                beads_rust::exit::PANIC,
                beads_rust::exit::panic_exit_status(),
            );
        }
    }));
}

/// Panic on demand so the panic banner is end-to-end testable.
///
/// This exists **solely** so `tests/e2e_fail_banner.rs` can assert that a
/// process dying by panic still emits the final banner — the most catastrophic
/// exit path, and the one where a confusing scrollback costs the most. There is
/// no other way to reach a panic from `br`'s own inputs, and behaviour in a
/// death path that has only ever been reasoned about is exactly what this
/// feature exists to stop trusting.
///
/// Gated on `debug_assertions`, so it cannot exist in the shipped release
/// binary. (The abort-profile verification described in that test builds
/// release *with* debug assertions on via `--config` to reach it.)
///
/// `BD_PANIC_FOR_TEST=partial-stdout` additionally leaves an unterminated line
/// in stdout's buffer first. That is the *only* way to observe the
/// `stdout().flush()` inside [`beads_rust::exit::emit_exit_banner`]: stdout is a
/// `LineWriter`, so buffered bytes are only ever pending when a printer stopped
/// mid-line, and no command in the tree today both stops mid-line and then
/// fails. Without that flush the pending bytes are emitted by std's exit cleanup
/// *after* the banner (under unwinding) or dropped entirely (under
/// `panic = "abort"`) — the first of which is precisely the "banner is usually
/// last" outcome this feature must not ship.
#[cfg(debug_assertions)]
fn maybe_panic_for_test() {
    let Some(mode) = std::env::var_os("BD_PANIC_FOR_TEST") else {
        return;
    };
    if mode == *"partial-stdout" {
        print!("PARTIAL-STDOUT-NO-NEWLINE");
    }
    panic!("BD_PANIC_FOR_TEST: deliberate panic for the failure-banner test");
}

/// Is this panic a failed write caused by the reader going away?
///
/// There are two distinct sources, and both must be caught — missing either
/// leaves half the commands still crashing:
///
/// 1. **std**, from `println!`/`print!`: the message is built in
///    `std::io::stdio::print_to` as `"failed printing to stdout: ..."`. This is
///    what `bd list` hits.
/// 2. **`rich_rust`**, from its console writer
///    (`console.rs`: `"failed to write to output stream: ... BrokenPipe"`).
///    This is what `bd search` hits, and it was found only because the
///    regression test exercised `search` as well as `list` — fixing just the
///    std path left `search` exiting 101.
///
/// Matched on payload strings because neither source offers a typed signal.
/// That makes this the brittle part of the guard, so the check is deliberately
/// broad: any panic whose message names a broken pipe qualifies, regardless of
/// which writer raised it. Exiting quietly is the right response to a vanished
/// reader no matter who noticed it first.
///
/// `tests/e2e_broken_pipe.rs` asserts on the *process outcome* rather than on
/// these strings, so it will fail loudly if any of this wording drifts.
fn is_broken_pipe_panic(info: &std::panic::PanicHookInfo<'_>) -> bool {
    let payload = info.payload();
    let Some(message) = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
    else {
        return false;
    };

    message.starts_with("failed printing to ")
        || message.contains("BrokenPipe")
        || message.contains("Broken pipe")
}

#[allow(clippy::too_many_lines)]
fn main() {
    install_broken_pipe_guard();

    #[cfg(debug_assertions)]
    maybe_panic_for_test();

    CompleteEnv::with_factory(Cli::command).complete();

    let cli = parse_cli();
    let output_ctx = OutputContext::from_args(&cli);

    // Initialize logging
    if let Err(e) = init_logging(cli.verbose, cli.quiet, None) {
        eprintln!("Failed to initialize logging: {e}");
        // Don't exit, just continue without logging or with basic stderr
    }

    let overrides = build_cli_overrides(&cli);

    // Track if this command potentially mutates data (for auto-flush)
    let is_mutating = is_mutating_command(&cli.command);

    if should_auto_import(&cli.command) && !cli.no_db {
        if let Err(e) = run_auto_import(&overrides, cli.allow_stale, cli.no_auto_import) {
            handle_error(&e, cli.json);
        }
    }

    let result = match cli.command {
        Commands::Init(args) => commands::init::execute(args.force, None, &output_ctx),
        Commands::Working => commands::presence::execute_working(&overrides, &output_ctx),
        Commands::Idle => commands::presence::execute_idle(&overrides, &output_ctx),
        Commands::Create(args) => commands::create::execute(&args, &overrides, &output_ctx),
        Commands::Update(args) => commands::update::execute(&args, &overrides, &output_ctx),
        Commands::Comments(args) => {
            commands::comments::execute(&args, cli.json, &overrides, &output_ctx)
        }
        Commands::Delete(args) => {
            commands::delete::execute(&args, cli.json, &overrides, &output_ctx)
        }
        Commands::List(args) => commands::list::execute(&args, cli.json, &overrides, &output_ctx),
        Commands::Search(args) => {
            commands::search::execute(&args, cli.json, &overrides, &output_ctx)
        }
        Commands::Show(args) => commands::show::execute(&args, cli.json, &overrides, &output_ctx),
        Commands::Close(args) => {
            commands::close::execute_cli(&args, cli.json || args.robot, &overrides, &output_ctx)
        }
        Commands::Reopen(args) => {
            commands::reopen::execute(&args, cli.json || args.robot, &overrides, &output_ctx)
        }
        Commands::Q(args) => commands::q::execute(args, &overrides, &output_ctx),
        Commands::Dep { command } => {
            commands::dep::execute(&command, cli.json, &overrides, &output_ctx)
        }
        Commands::Epic { command } => {
            commands::epic::execute(&command, cli.json, &overrides, &output_ctx)
        }
        Commands::Count(args) => commands::count::execute(&args, cli.json, &overrides, &output_ctx),
        Commands::Stale(args) => commands::stale::execute(&args, &overrides, &output_ctx),
        Commands::Lint(args) => commands::lint::execute(&args, cli.json, &overrides, &output_ctx),
        Commands::Dash(args) => commands::dash::execute(&args, &overrides, &output_ctx),
        Commands::Msg(args) => commands::messaging::execute_msg(&args, &overrides, &output_ctx),
        Commands::Who(args) => commands::who::execute(&args, &overrides, &output_ctx),
        Commands::Inbox(args) => commands::messaging::execute_inbox(&args, &overrides, &output_ctx),
        Commands::Outbox(args) => {
            commands::messaging::execute_outbox(&args, &overrides, &output_ctx)
        }
        Commands::Watch(args) => commands::watch::execute(&args, &overrides, &output_ctx),
        Commands::Blocked(args) => {
            commands::blocked::execute(&args, cli.json || args.robot, &overrides, &output_ctx)
        }
        Commands::Sync(args) => commands::sync::execute(&args, cli.json, &overrides, &output_ctx),
        Commands::Doctor => commands::doctor::execute(&overrides, &output_ctx),
        Commands::Info(args) => commands::info::execute(&args, &overrides, &output_ctx),
        Commands::Schema(args) => commands::schema::execute(&args, &overrides, &output_ctx),
        Commands::Where => commands::r#where::execute(&overrides, &output_ctx),
        Commands::Version(args) => commands::version::execute(&args, &output_ctx),

        #[cfg(feature = "self_update")]
        Commands::Upgrade(args) => commands::upgrade::execute(&args, &output_ctx),
        Commands::Completions(args) => commands::completions::execute(&args, &output_ctx),
        Commands::Audit { command } => {
            commands::audit::execute(&command, cli.json, &overrides, &output_ctx)
        }
        Commands::Stats(args) | Commands::Status(args) => {
            commands::stats::execute(&args, cli.json || args.robot, &overrides, &output_ctx)
        }
        Commands::Config { command } => {
            commands::config::execute(&command, cli.json, &overrides, &output_ctx)
        }
        Commands::History(args) => commands::history::execute(args, &overrides, &output_ctx),
        Commands::Defer(args) => {
            commands::defer::execute_defer(&args, cli.json || args.robot, &overrides, &output_ctx)
        }
        Commands::Undefer(args) => {
            commands::defer::execute_undefer(&args, cli.json || args.robot, &overrides, &output_ctx)
        }
        Commands::Orphans(args) => {
            commands::orphans::execute(&args, cli.json || args.robot, &overrides, &output_ctx)
        }
        Commands::Changelog(args) => {
            commands::changelog::execute(&args, cli.json || args.robot, &overrides, &output_ctx)
        }
        Commands::Query { command } => commands::query::execute(&command, &overrides, &output_ctx),
        Commands::Graph(args) => commands::graph::execute(&args, &overrides, &output_ctx),
        Commands::Admin { command } => dispatch_admin(command, &overrides, &output_ctx, cli.json),
    };

    // Handle command result
    if let Err(e) = result {
        handle_error(&e, cli.json);
    }

    // Auto-flush after successful mutating commands (unless --no-auto-flush)
    if is_mutating && !cli.no_auto_flush && !cli.no_db {
        run_auto_flush(&overrides);
    }
}

/// Route a `bd admin <op>` invocation through the same handlers as the
/// top-level (hidden) equivalent.
fn dispatch_admin(
    command: beads_rust::cli::AdminCommands,
    overrides: &config::CliOverrides,
    output_ctx: &OutputContext,
    json: bool,
) -> Result<()> {
    use beads_rust::cli::AdminCommands as A;
    match command {
        A::Init(args) => commands::init::execute(args.force, None, output_ctx),
        A::Dash(args) => commands::dash::execute(&args, overrides, output_ctx),
        A::Graph(args) => commands::graph::execute(&args, overrides, output_ctx),
        A::Stats(args) | A::Status(args) => {
            commands::stats::execute(&args, json || args.robot, overrides, output_ctx)
        }
        A::Delete(args) => commands::delete::execute(&args, json, overrides, output_ctx),
        A::Epic { command } => commands::epic::execute(&command, json, overrides, output_ctx),
        A::Count(args) => commands::count::execute(&args, json, overrides, output_ctx),
        A::Stale(args) => commands::stale::execute(&args, overrides, output_ctx),
        A::Lint(args) => commands::lint::execute(&args, json, overrides, output_ctx),
        A::Config { command } => commands::config::execute(&command, json, overrides, output_ctx),
        A::Sync(args) => commands::sync::execute(&args, json, overrides, output_ctx),
        A::Doctor => commands::doctor::execute(overrides, output_ctx),
        A::Info(args) => commands::info::execute(&args, overrides, output_ctx),
        A::Schema(args) => commands::schema::execute(&args, overrides, output_ctx),
        A::Where => commands::r#where::execute(overrides, output_ctx),
        #[cfg(feature = "self_update")]
        A::Upgrade(args) => commands::upgrade::execute(&args, output_ctx),
        A::Completions(args) => commands::completions::execute(&args, output_ctx),
        A::Audit { command } => commands::audit::execute(&command, json, overrides, output_ctx),
        A::History(args) => commands::history::execute(args, overrides, output_ctx),
        A::Orphans(args) => {
            commands::orphans::execute(&args, json || args.robot, overrides, output_ctx)
        }
        A::Changelog(args) => {
            commands::changelog::execute(&args, json || args.robot, overrides, output_ctx)
        }
        A::Query { command } => commands::query::execute(&command, overrides, output_ctx),
        A::Msg(args) => commands::messaging::execute_admin_msg(&args, overrides, output_ctx),
        A::Inbox(args) => commands::messaging::execute_admin_inbox(&args, overrides, output_ctx),
        A::Outbox(args) => commands::messaging::execute_admin_outbox(&args, overrides, output_ctx),
        A::Watch => commands::admin_watch::execute(overrides, output_ctx),
        A::Reload(args) => commands::reload::execute(&args, overrides, output_ctx),
    }
}

/// Determine if a command potentially mutates data.
const fn is_mutating_command(cmd: &Commands) -> bool {
    match cmd {
        Commands::Create(_)
        | Commands::Update(_)
        | Commands::Delete(_)
        | Commands::Close(_)
        | Commands::Reopen(_)
        | Commands::Q(_)
        | Commands::Dep { .. }
        | Commands::Defer(_)
        | Commands::Undefer(_)
        | Commands::Msg(_) => true,
        // Only `comments add` writes; a bare `comments <id>` is a read.
        Commands::Comments(args) => matches!(
            args.command,
            Some(beads_rust::cli::CommentsCommands::Add(_))
        ),
        Commands::Epic { command } => matches!(
            command,
            beads_rust::cli::EpicCommands::CloseEligible(args) if !args.dry_run
        ),
        Commands::Admin { command } => is_mutating_admin(command),
        _ => false,
    }
}

const fn is_mutating_admin(cmd: &beads_rust::cli::AdminCommands) -> bool {
    use beads_rust::cli::AdminCommands as A;
    match cmd {
        A::Epic { command } => matches!(
            command,
            beads_rust::cli::EpicCommands::CloseEligible(args) if !args.dry_run
        ),
        // Everything else that writes: deletion, message send, inbox
        // (marks read), watch (registers a watcher row), reload.
        A::Delete(_) | A::Msg(_) | A::Inbox(_) | A::Watch | A::Reload(_) => true,
        _ => false,
    }
}

const fn should_auto_import(cmd: &Commands) -> bool {
    match cmd {
        // Commands that need auto-import:
        // - Read-only commands (to ensure fresh data)
        // - Mutating commands (to avoid overwriting external changes)
        // - Subcommands (Comments, Dep, Label, Epic, Query)
        Commands::List(_)
        | Commands::Comments(_)
        | Commands::Show(_)
        | Commands::Search(_)
        | Commands::Blocked(_)
        | Commands::Count(_)
        | Commands::Stale(_)
        | Commands::Lint(_)
        | Commands::Stats(_)
        | Commands::Status(_)
        | Commands::Orphans(_)
        | Commands::Changelog(_)
        | Commands::Graph(_)
        | Commands::Create(_)
        | Commands::Update(_)
        | Commands::Delete(_)
        | Commands::Close(_)
        | Commands::Reopen(_)
        | Commands::Q(_)
        | Commands::Defer(_)
        | Commands::Undefer(_)
        | Commands::Dep { .. }
        | Commands::Epic { .. }
        | Commands::Query { .. } => true,

        // Explicitly excluded: init, sync, diagnostic, and config commands
        Commands::Init(_)
        | Commands::Sync(_)
        | Commands::Doctor
        | Commands::Info(_)
        | Commands::Schema(_)
        | Commands::Where
        | Commands::Version(_)
        | Commands::Completions(_)
        | Commands::Audit { .. }
        | Commands::Config { .. }
        | Commands::History(_)
        | Commands::Watch(_)
        | Commands::Msg(_)
        | Commands::Who(_)
        | Commands::Inbox(_)
        | Commands::Outbox(_)
        | Commands::Dash(_)
        | Commands::Working
        | Commands::Idle => false,

        Commands::Admin { command } => should_auto_import_admin(command),

        #[cfg(feature = "self_update")]
        Commands::Upgrade(_) => false,
    }
}

const fn should_auto_import_admin(cmd: &beads_rust::cli::AdminCommands) -> bool {
    use beads_rust::cli::AdminCommands as A;
    match cmd {
        // Operations that benefit from fresh data.
        A::Dash(_)
        | A::Graph(_)
        | A::Stats(_)
        | A::Status(_)
        | A::Delete(_)
        | A::Epic { .. }
        | A::Count(_)
        | A::Stale(_)
        | A::Lint(_)
        | A::Orphans(_)
        | A::Changelog(_)
        | A::Query { .. } => true,
        // Diagnostic / setup / sync-adjacent: skip.
        _ => false,
    }
}

/// Run auto-import before read-only commands when JSONL is newer.
fn run_auto_import(
    overrides: &config::CliOverrides,
    allow_stale: bool,
    no_auto_import: bool,
) -> Result<()> {
    // If not initialized, skip auto-import (e.g. running 'br init')
    let beads_dir = match config::discover_beads_dir(Some(Path::new("."))) {
        Ok(dir) => dir,
        Err(BeadsError::NotInitialized) => return Ok(()),
        Err(e) => return Err(e),
    };

    let config::OpenStorageResult {
        mut storage,
        paths,
        no_db,
    } = config::open_storage_with_cli(&beads_dir, overrides)?;

    if no_db {
        return Ok(());
    }

    // No config-sourced expected prefix anymore (issue_prefix config key
    // removed) — auto-import no longer validates against a project-wide
    // default prefix.
    let outcome = auto_import_if_stale(
        &mut storage,
        &paths.beads_dir,
        &paths.jsonl_path,
        None,
        allow_stale,
        no_auto_import,
    )?;

    if outcome.attempted {
        debug!(
            imported_count = outcome.imported_count,
            "Auto-import attempt completed"
        );
    }

    Ok(())
}

/// Run auto-flush after mutating commands.
///
/// This discovers the beads directory, opens a fresh storage connection,
/// and exports any dirty issues to JSONL.
fn run_auto_flush(overrides: &config::CliOverrides) {
    // Try to discover beads directory
    let beads_dir = match config::discover_beads_dir(Some(Path::new("."))) {
        Ok(dir) => dir,
        Err(e) => {
            debug!(
                ?e,
                "Auto-flush skipped: could not discover .beads directory"
            );
            return;
        }
    };

    // Open storage with fresh connection
    let (mut storage, _paths) =
        match config::open_storage(&beads_dir, overrides.db.as_ref(), overrides.lock_timeout) {
            Ok(result) => result,
            Err(e) => {
                debug!(?e, "Auto-flush skipped: could not open storage");
                return;
            }
        };

    // Run auto-flush
    match auto_flush(&mut storage, &beads_dir) {
        Ok(result) => {
            if result.flushed {
                debug!(
                    exported = result.exported_count,
                    hash = %result.content_hash,
                    "Auto-flush completed"
                );
            }
        }
        Err(e) => {
            // Log but don't fail - auto-flush errors shouldn't break the command
            debug!(?e, "Auto-flush failed (non-fatal)");
        }
    }
}

/// Parse the command line, routing clap's own failure through the exit funnel.
///
/// `Cli::parse()` cannot be used: on a usage error it calls
/// `std::process::exit` from inside clap, which would leave `br list --nope`
/// (exit 2) as the one everyday failure with no banner. `try_parse` hands the
/// error back so the exit goes through [`exit_with_status`] like every other.
///
/// `--help` and `--version` also arrive here as `Err`, with exit code 0; they
/// are printed and exited zero, so they get no banner — again by the status
/// rule, not by a special case.
fn parse_cli() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let code = err.exit_code();
            // clap picks the right stream itself: stdout for help/version,
            // stderr for usage errors.
            let _ = err.print();
            if code == 0 {
                let _ = io::stdout().flush();
                std::process::exit(0);
            }
            exit_with_status(code, ExitKind::Failure, beads_rust::exit::USAGE_ERROR)
        }
    }
}

/// Handle errors with structured output support.
///
/// When --json is set or stdout is not a TTY, outputs structured JSON to stderr.
/// Otherwise, outputs human-readable error with optional color.
fn handle_error(err: &BeadsError, json_mode: bool) -> ! {
    let structured = StructuredError::from_error(err);
    let exit_code = structured.code.exit_code();

    // Determine output mode: JSON if --json flag or stdout is not a terminal
    let use_json = json_mode || !io::stdout().is_terminal();

    if use_json {
        // Output structured JSON to stderr
        let json = structured.to_json();
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_else(|_| json.to_string())
        );
    } else {
        // Human-readable output with color if stderr is a terminal
        let use_color = io::stderr().is_terminal();
        eprintln!("{}", structured.to_human(use_color));
    }

    // The envelope above is what a truncating filter cuts the top off; the
    // banner emitted by the funnel is what survives it.
    exit_with_status(exit_code, ExitKind::Failure, structured.code.as_str());
}

fn build_cli_overrides(cli: &Cli) -> config::CliOverrides {
    config::CliOverrides {
        db: cli.db.clone(),
        actor: cli.actor.clone(),
        json: Some(cli.json),
        display_color: if cli.no_color { Some(false) } else { None },
        quiet: Some(cli.quiet),
        no_db: Some(cli.no_db),
        no_daemon: Some(cli.no_daemon),
        no_auto_flush: Some(cli.no_auto_flush),
        no_auto_import: Some(cli.no_auto_import),
        lock_timeout: cli.lock_timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn make_create_args() -> beads_rust::cli::CreateArgs {
        beads_rust::cli::CreateArgs {
            title: Some("test-title".to_string()),
            title_flag: None,
            type_: None,
            priority: None,
            description: None,
            assignee: None,
            owner: None,
            labels: Vec::new(),
            parent: None,
            deps: Vec::new(),
            estimate: None,
            due: None,
            defer: None,
            external_ref: None,
            status: None,
            prefix: None,
            ephemeral: false,
            dry_run: false,
            silent: false,
            file: None,
        }
    }

    #[test]
    fn parse_global_flags_and_command() {
        let cli = Cli::parse_from(["br", "--json", "-vv", "list"]);
        assert!(cli.json);
        assert_eq!(cli.verbose, 2);
        assert!(!cli.quiet);
        assert!(matches!(cli.command, Commands::List(_)));
    }

    #[test]
    fn parse_create_title_positional() {
        let cli = Cli::parse_from(["br", "create", "FixBug"]);
        match cli.command {
            Commands::Create(args) => {
                assert_eq!(args.title.as_deref(), Some("FixBug"));
            }
            other => unreachable!("expected create command, got {other:?}"),
        }
    }

    #[test]
    fn build_overrides_maps_flags() {
        let cli = Cli::parse_from([
            "br",
            "--json",
            "--no-color",
            "--no-auto-flush",
            "--lock-timeout",
            "2500",
            "list",
        ]);
        let overrides = build_cli_overrides(&cli);
        assert_eq!(overrides.json, Some(true));
        assert_eq!(overrides.display_color, Some(false));
        assert_eq!(overrides.no_auto_flush, Some(true));
        assert_eq!(overrides.lock_timeout, Some(2500));
    }

    #[test]
    fn help_includes_core_commands() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("create"));
        assert!(help.contains("list"));
        assert!(help.contains("sync"));
    }

    #[test]
    fn version_includes_name_and_version() {
        let version = Cli::command().render_version();
        assert!(version.contains("br"));
        assert!(version.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn is_mutating_command_detects_mutations() {
        let create_cmd = Commands::Create(make_create_args());
        let list_cmd = Commands::List(beads_rust::cli::ListArgs::default());
        assert!(is_mutating_command(&create_cmd));
        assert!(!is_mutating_command(&list_cmd));
    }
}
