//! Version command implementation.

use crate::error::Result;
use crate::output::OutputContext;
use serde::Serialize;
use std::fmt::Write as _;

#[derive(Serialize)]
struct VersionOutput<'a> {
    version: &'a str,
    build: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<&'a str>,
}

/// Execute the version command.
///
/// # Errors
///
/// Returns an error if JSON serialization fails.
pub fn execute(json: bool, _ctx: &OutputContext) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let build = if cfg!(debug_assertions) {
        "dev"
    } else {
        "release"
    };

    let commit = option_env!("VERGEN_GIT_SHA").filter(|s| !s.trim().is_empty());
    let branch = option_env!("VERGEN_GIT_BRANCH").filter(|s| !s.trim().is_empty());

    if json {
        let output = VersionOutput {
            version,
            build,
            commit,
            branch,
        };
        let payload = serde_json::to_string(&output)?;
        println!("{payload}");
        return Ok(());
    }

    let mut line = format!("br version {version} ({build})");
    match (branch, commit) {
        (Some(branch), Some(commit)) => {
            let short = &commit[..commit.len().min(7)];
            let _ = write!(line, " ({branch}@{short})");
        }
        (Some(branch), None) => {
            let _ = write!(line, " ({branch})");
        }
        (None, Some(commit)) => {
            let short = &commit[..commit.len().min(7)];
            let _ = write!(line, " ({short})");
        }
        (None, None) => {}
    }

    println!("{line}");
    Ok(())
}
