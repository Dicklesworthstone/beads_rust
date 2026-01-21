use beads_rust::cli::commands;
use beads_rust::cli::{Cli, Commands};
use beads_rust::config;
use beads_rust::logging::init_logging;
use beads_rust::output::OutputContext;
use beads_rust::sync::{auto_flush, auto_import_if_stale};
use beads_rust::{BeadsError, Result, StructuredError};
use clap::Parser;
use std::io::{self, IsTerminal};
use std::path::Path;
use tracing::debug;

#[allow(clippy::too_many_lines)]
fn main() {
    let cli = Cli::parse();
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
        Commands::Init {
            prefix,
            force,
            backend: _,
        } => commands::init::execute(prefix, force, None, &output_ctx),
        Commands::Create(args) => commands::create::execute(&args, &overrides, &output_ctx),
        Commands::Update(args) => commands::update::execute(&args, &overrides, &output_ctx),
        Commands::Delete(args) => {
            commands::delete::execute(&args, cli.json, &overrides, &output_ctx)
        }
        Commands::List(args) => commands::list::execute(&args, cli.json, &overrides, &output_ctx),
        Commands::Comments(args) => {
            commands::comments::execute(&args, cli.json, &overrides, &output_ctx)
        }
        Commands::Search(args) => {
            commands::search::execute(&args, cli.json, &overrides, &output_ctx)
        }
        Commands::Show { ids } => commands::show::execute(ids, cli.json, &overrides, &output_ctx),
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
        Commands::Label { command } => {
            commands::label::execute(&command, cli.json, &overrides, &output_ctx)
        }
        Commands::Count(args) => commands::count::execute(&args, cli.json, &overrides, &output_ctx),
        Commands::Stale(args) => commands::stale::execute(&args, &overrides, &output_ctx),
        Commands::Lint(args) => commands::lint::execute(&args, cli.json, &overrides, &output_ctx),
        Commands::Ready(args) => commands::ready::execute(&args, cli.json, &overrides, &output_ctx),
        Commands::Blocked(args) => {
            commands::blocked::execute(&args, cli.json || args.robot, &overrides, &output_ctx)
        }
        Commands::Sync(args) => commands::sync::execute(&args, cli.json, &overrides, &output_ctx),
        Commands::Doctor => commands::doctor::execute(&overrides, &output_ctx),
        Commands::Info(args) => commands::info::execute(&args, &overrides, &output_ctx),
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
        Commands::Agents(args) => {
            let agents_args = commands::agents::AgentsArgs {
                add: args.add,
                remove: args.remove,
                update: args.update,
                check: args.check,
                dry_run: args.dry_run,
                force: args.force,
            };
            commands::agents::execute(&agents_args, &output_ctx)
        }
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
        | Commands::Label { .. }
        | Commands::Comments(_)
        | Commands::Defer(_)
        | Commands::Undefer(_) => true,
        Commands::Epic { command } => matches!(
            command,
            beads_rust::cli::EpicCommands::CloseEligible(args) if !args.dry_run
        ),
        _ => false,
    }
}

const fn should_auto_import(cmd: &Commands) -> bool {
    match cmd {
        // Read-only commands
        Commands::List(_)
        | Commands::Show { .. }
        | Commands::Search(_)
        | Commands::Ready(_)
        | Commands::Blocked(_)
        | Commands::Count(_)
        | Commands::Stale(_)
        | Commands::Lint(_)
        | Commands::Stats(_)
        | Commands::Status(_)
        | Commands::Orphans(_)
        | Commands::Changelog(_)
        | Commands::Graph(_) => true,

        // Mutating commands (should import to avoid overwriting external changes)
        Commands::Create(_)
        | Commands::Update(_)
        | Commands::Delete(_)
        | Commands::Close(_)
        | Commands::Reopen(_)
        | Commands::Q(_)
        | Commands::Defer(_)
        | Commands::Undefer(_) => true,

        // Subcommands
        Commands::Comments(_) => true, // Covers Add and List
        Commands::Dep { .. } => true,  // Covers Add, Remove, List, Tree, Cycles
        Commands::Label { .. } => true,// Covers Add, Remove, List, ListAll, Rename
        Commands::Epic { .. } => true, // Covers Status, CloseEligible
        Commands::Query { .. } => true,// Covers Save, Run, List, Delete

        // Explicitly excluded
        Commands::Init { .. }
        | Commands::Sync(_)
        | Commands::Doctor
        | Commands::Info(_)
        | Commands::Where
        | Commands::Version(_)
        | Commands::Upgrade(_)
        | Commands::Completions(_)
        | Commands::Audit { .. } // Audit is append-only JSONL, usually local
        | Commands::Config { .. }
        | Commands::History(_)
        | Commands::Agents(_) => false,
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

    let expected_prefix = storage.get_config("issue_prefix")?;
    let outcome = auto_import_if_stale(
        &mut storage,
        &paths.beads_dir,
        &paths.jsonl_path,
        expected_prefix.as_deref(),
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

    std::process::exit(exit_code);
}

fn build_cli_overrides(cli: &Cli) -> config::CliOverrides {
    config::CliOverrides {
        db: cli.db.clone(),
        actor: cli.actor.clone(),
        identity: None,
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
            other => panic!("expected create command, got {other:?}"),
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
        assert!(help.contains("ready"));
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
