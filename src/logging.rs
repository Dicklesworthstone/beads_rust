//! Logging configuration and initialization.
//!
//! Uses tracing with environment-based filtering and optional JSON file output.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::{Mutex, Once};

use anyhow::Result;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Initialize logging for the CLI.
///
/// Logging honors `RUST_LOG` if set; otherwise a default filter is used based
/// on verbosity and quiet flags.
///
/// # Errors
///
/// Returns an error if logging initialization fails.
pub fn init_logging(verbosity: u8, quiet: bool, log_file: Option<&Path>) -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(default_filter(verbosity, quiet)))?;

    let fmt_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_level(true)
        .with_file(cfg!(debug_assertions))
        .with_line_number(cfg!(debug_assertions))
        .with_ansi(std::io::stderr().is_terminal());

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer);

    if let Some(path) = log_file {
        let file = std::fs::File::create(path)?;
        let file_layer = fmt::layer()
            .with_writer(Mutex::new(file))
            .with_ansi(false)
            .json();
        tracing::subscriber::set_global_default(subscriber.with(file_layer))?;
    } else {
        tracing::subscriber::set_global_default(subscriber)?;
    }

    Ok(())
}

fn default_filter(verbosity: u8, quiet: bool) -> String {
    if quiet {
        return "error".to_string();
    }

    match verbosity {
        0 => {
            if cfg!(debug_assertions) {
                "beads_rust=debug".to_string()
            } else {
                "beads_rust=info".to_string()
            }
        }
        1 => "beads_rust=debug".to_string(),
        2 => "beads_rust=debug,rusqlite=debug".to_string(),
        _ => "beads_rust=trace".to_string(),
    }
}

/// Initialize logging for tests with the test writer.
pub fn init_test_logging() {
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter("beads_rust=debug,test=debug")
            .with_test_writer()
            .try_init()
            .ok();
    });
}
