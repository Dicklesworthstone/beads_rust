//! Version command implementation.

use crate::error::Result;
use crate::output::{OutputContext, OutputMode};
use rich_rust::prelude::*;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    rust_version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    features: Vec<&'a str>,
}

/// Execute the version command.
///
/// # Errors
///
/// Returns an error if JSON serialization fails.
pub fn execute(json: bool, ctx: &OutputContext) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let build = if cfg!(debug_assertions) {
        "dev"
    } else {
        "release"
    };

    let commit = option_env!("VERGEN_GIT_SHA").filter(|s| !s.trim().is_empty());
    let branch = option_env!("VERGEN_GIT_BRANCH").filter(|s| !s.trim().is_empty());
    let rust_version = option_env!("VERGEN_RUSTC_SEMVER").filter(|s| !s.trim().is_empty());
    let target = option_env!("VERGEN_CARGO_TARGET_TRIPLE").filter(|s| !s.trim().is_empty());

    // Collect enabled features
    let mut features = Vec::new();
    if cfg!(feature = "self_update") {
        features.push("self_update");
    }

    if json {
        let output = VersionOutput {
            version,
            build,
            commit,
            branch,
            rust_version,
            target,
            features,
        };
        let payload = serde_json::to_string(&output)?;
        println!("{payload}");
        return Ok(());
    }

    // Rich output mode
    if matches!(ctx.mode(), OutputMode::Rich) {
        render_version_rich(
            version,
            build,
            commit,
            branch,
            rust_version,
            target,
            &features,
            ctx,
        );
        return Ok(());
    }

    // Plain text output
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

/// Render version information with rich formatting.
#[allow(clippy::too_many_arguments)]
fn render_version_rich(
    version: &str,
    build: &str,
    commit: Option<&str>,
    branch: Option<&str>,
    rust_version: Option<&str>,
    target: Option<&str>,
    features: &[&str],
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    // Version header with styling
    content.append_styled(&format!("br {version}"), theme.emphasis.clone());
    content.append_styled(&format!(" ({build})"), theme.dimmed.clone());
    content.append("\n\n");

    // Build info section
    let has_build_info =
        commit.is_some() || branch.is_some() || rust_version.is_some() || target.is_some();

    if has_build_info {
        content.append_styled("Build Info:\n", theme.section.clone());

        let mut info_items: Vec<(&str, String)> = Vec::new();

        if let Some(commit) = commit {
            let short = &commit[..commit.len().min(7)];
            info_items.push(("Commit", short.to_string()));
        }
        if let Some(branch) = branch {
            info_items.push(("Branch", branch.to_string()));
        }
        if let Some(rust_ver) = rust_version {
            info_items.push(("Rust", rust_ver.to_string()));
        }
        if let Some(tgt) = target {
            info_items.push(("Target", tgt.to_string()));
        }

        let last_idx = info_items.len().saturating_sub(1);
        for (idx, (label, value)) in info_items.iter().enumerate() {
            let prefix = if idx == last_idx {
                "└── "
            } else {
                "├── "
            };
            content.append_styled(prefix, theme.dimmed.clone());
            content.append_styled(&format!("{:<8}", label), theme.accent.clone());
            content.append(&format!("{value}\n"));
        }
        content.append("\n");
    }

    // Features section
    if !features.is_empty() {
        content.append_styled("Features: ", theme.section.clone());
        content.append_styled(&features.join(", "), theme.success.clone());
        content.append("\n");
    }

    // Wrap in panel
    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("br version", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}
