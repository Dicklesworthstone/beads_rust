use beads_rust::cli::commands;
use beads_rust::cli::{Cli, Commands};
use beads_rust::config;
use beads_rust::logging::init_logging;
use beads_rust::{BeadsError, StructuredError};
use clap::Parser;
use std::io::{self, IsTerminal};

fn main() {
    let cli = Cli::parse();

    // Initialize logging
    if let Err(e) = init_logging(cli.verbose, cli.quiet, None) {
        eprintln!("Failed to initialize logging: {e}");
        // Don't exit, just continue without logging or with basic stderr
    }

    let overrides = build_cli_overrides(&cli);

    let result = match cli.command {
        Commands::Init { prefix, force, .. } => commands::init::execute(prefix, force, None),
        Commands::Create(args) => commands::create::execute(args, &overrides),
        Commands::Update(args) => commands::update::execute(&args, &overrides),
        Commands::Delete(args) => commands::delete::execute(&args, &overrides),
        Commands::List(args) => commands::list::execute(&args, cli.json, &overrides),
        Commands::Comments(args) => commands::comments::execute(&args, cli.json, &overrides),
        Commands::Search(args) => commands::search::execute(&args, cli.json, &overrides),
        Commands::Show { ids } => commands::show::execute(ids, cli.json, &overrides),
        Commands::Close(args) => {
            commands::close::execute_cli(&args, cli.json || args.robot, &overrides)
        }
        Commands::Reopen(args) => {
            commands::reopen::execute(&args, cli.json || args.robot, &overrides)
        }
        Commands::Q(args) => commands::q::execute(args, &overrides),
        Commands::Dep { command } => commands::dep::execute(&command, cli.json, &overrides),
        Commands::Label { command } => commands::label::execute(&command, cli.json, &overrides),
        Commands::Count(args) => commands::count::execute(&args, cli.json, &overrides),
        Commands::Stale(args) => commands::stale::execute(&args, cli.json, &overrides),
        Commands::Ready(args) => commands::ready::execute(&args, cli.json, &overrides),
        Commands::Blocked(args) => {
            commands::blocked::execute(&args, cli.json || args.robot, &overrides)
        }
        Commands::Sync(args) => commands::sync::execute(&args, cli.json, &overrides),
        Commands::Doctor => commands::doctor::execute(cli.json, &overrides),
        Commands::Version => commands::version::execute(cli.json),
        Commands::Completions(args) => commands::completions::execute(&args),
        Commands::Stats(args) | Commands::Status(args) => {
            commands::stats::execute(&args, cli.json || args.robot, &overrides)
        }
        Commands::Config(args) => commands::config::execute(&args, cli.json, &overrides),
        Commands::History(args) => commands::history::execute(args, &overrides),
        Commands::Defer(args) => commands::defer::execute_defer(&args, cli.json, &overrides),
        Commands::Undefer(args) => commands::defer::execute_undefer(&args, cli.json, &overrides),
    };

    if let Err(e) = result {
        handle_error(&e, cli.json);
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
        no_db: Some(cli.no_db),
        no_daemon: Some(cli.no_daemon),
        no_auto_flush: Some(cli.no_auto_flush),
        no_auto_import: Some(cli.no_auto_import),
        lock_timeout: cli.lock_timeout,
    }
}
