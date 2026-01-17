//! Upgrade command implementation.
//!
//! Enables br to update itself to the latest version using the `self_update` crate.

use crate::cli::UpgradeArgs;
use crate::error::{BeadsError, Result};
use self_update::backends::github;
use self_update::cargo_crate_version;
use self_update::update::ReleaseUpdate;
use serde::Serialize;

/// Repo owner for GitHub releases.
const REPO_OWNER: &str = "Dicklesworthstone";

/// Repo name for GitHub releases.
const REPO_NAME: &str = "beads_rust";

/// Binary name.
const BIN_NAME: &str = "br";

/// Update check result.
#[derive(Serialize)]
struct UpdateCheckResult {
    current_version: String,
    latest_version: String,
    update_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    download_url: Option<String>,
}

/// Update result.
#[derive(Serialize)]
struct UpdateResult {
    current_version: String,
    new_version: String,
    updated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// Execute the upgrade command.
///
/// # Errors
///
/// Returns an error if the update check or download fails.
pub fn execute(args: &UpgradeArgs, json: bool) -> Result<()> {
    let current_version = cargo_crate_version!();

    if args.dry_run {
        return execute_dry_run(args, current_version, json);
    }

    if args.check {
        return execute_check(current_version, json);
    }

    execute_upgrade(args, current_version, json)
}

/// Execute check-only mode.
fn execute_check(current_version: &str, json: bool) -> Result<()> {
    tracing::info!("Checking for updates...");

    let updater = build_updater(current_version)?;
    let latest = updater.get_latest_release().map_err(map_update_error)?;
    let latest_version = &latest.version;

    let update_available = version_newer(latest_version, current_version);

    // Get download URL from first asset if available
    let download_url = latest.assets.first().map(|a| a.download_url.clone());

    let result = UpdateCheckResult {
        current_version: current_version.to_string(),
        latest_version: latest_version.clone(),
        update_available,
        download_url,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Current version: {current_version}");
        println!("Latest version:  {latest_version}");

        if update_available {
            println!("\n\u{2191} Update available! Run `br upgrade` to install.");
        } else {
            println!("\n\u{2713} Already up to date");
        }
    }

    Ok(())
}

/// Execute dry-run mode.
fn execute_dry_run(args: &UpgradeArgs, current_version: &str, json: bool) -> Result<()> {
    tracing::info!("Dry-run mode: checking what would happen...");

    let target_version = args.version.as_deref();
    let updater = build_updater(current_version)?;
    let latest = updater.get_latest_release().map_err(map_update_error)?;
    let latest_version = &latest.version;

    let install_version = target_version.unwrap_or(latest_version);
    let would_update = args.force || version_newer(install_version, current_version);

    // Get download URL from first asset if available
    let download_url = latest
        .assets
        .first()
        .map_or_else(|| "N/A".to_string(), |a| a.download_url.clone());

    if json {
        let result = serde_json::json!({
            "dry_run": true,
            "current_version": current_version,
            "target_version": install_version,
            "would_download": download_url,
            "would_update": would_update,
        });
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Dry-run mode (no changes will be made)\n");
        println!("Current version: {current_version}");
        println!("Target version:  {install_version}");
        println!("Would download:  {download_url}");
        println!(
            "Would install:   {}",
            if would_update {
                "yes"
            } else {
                "no (already up to date)"
            }
        );
        println!("\nNo changes made.");
    }

    Ok(())
}

/// Execute the actual upgrade.
fn execute_upgrade(args: &UpgradeArgs, current_version: &str, json: bool) -> Result<()> {
    tracing::info!(current = %current_version, "Starting upgrade...");

    if !json {
        println!("Checking for updates...");
        println!("Current version: {current_version}");
    }

    let updater = if let Some(ref target_version) = args.version {
        build_updater_with_target(target_version, current_version, !json)?
    } else {
        build_updater(current_version)?
    };

    // Get latest release info first
    let latest = updater.get_latest_release().map_err(map_update_error)?;
    let latest_version = &latest.version;

    if !json {
        println!("Latest version:  {latest_version}");
    }

    let update_available = args.force || version_newer(latest_version, current_version);

    if !update_available {
        let result = UpdateResult {
            current_version: current_version.to_string(),
            new_version: latest_version.clone(),
            updated: false,
            message: Some("Already up to date".to_string()),
        };

        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("\n\u{2713} Already up to date");
        }
        return Ok(())
    }

    if !json {
        println!("\nDownloading {latest_version}...");
    }

    // Perform the update
    let status = updater.update().map_err(map_update_error)?;

    let result = UpdateResult {
        current_version: current_version.to_string(),
        new_version: status.version().to_string(),
        updated: status.updated(),
        message: if status.updated() {
            Some(format!("Updated to {}", status.version()))
        } else {
            Some("No update performed".to_string())
        },
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if status.updated() {
        println!(
            "\n\u{2713} Updated br from {current_version} to {}",
            status.version()
        );
    } else {
        println!("\n\u{2713} Already up to date");
    }

    Ok(())
}

/// Build the self-update updater.
fn build_updater(current_version: &str) -> Result<Box<dyn ReleaseUpdate>> {
    github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .show_download_progress(true)
        .current_version(current_version)
        .build()
        .map_err(map_update_error)
}

/// Build updater with a specific target version.
fn build_updater_with_target(
    target_version: &str,
    current_version: &str,
    show_progress: bool,
) -> Result<Box<dyn ReleaseUpdate>> {
    github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .show_download_progress(show_progress)
        .current_version(current_version)
        .target_version_tag(target_version)
        .build()
        .map_err(map_update_error)
}

/// Map `self_update` errors to `BeadsError`.
fn map_update_error<E: std::error::Error + Send + Sync + 'static>(err: E) -> BeadsError {
    BeadsError::Other(anyhow::Error::from(err))
}

/// Compare versions to check if new is greater than current.
///
/// Handles semver-like versions (e.g., "0.2.0" > "0.1.0", "0.10.0" > "0.9.0").
fn version_newer(new: &str, current: &str) -> bool {
    let parse_version = |v: &str| -> Vec<u32> {
        v.trim_start_matches('v')
            .split('.')
            .filter_map(|s| s.parse().ok())
            .collect()
    };

    let new_parts = parse_version(new);
    let current_parts = parse_version(current);

    for (n, c) in new_parts.iter().zip(current_parts.iter()) {
        match n.cmp(c) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => {} // Continue to next part
        }
    }

    // If all compared parts are equal, the one with more parts is newer
    new_parts.len() > current_parts.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_comparison_basic() {
        assert!(version_newer("0.2.0", "0.1.0"));
        assert!(version_newer("1.0.0", "0.9.0"));
        assert!(version_newer("0.1.1", "0.1.0"));
    }

    #[test]
    fn test_version_comparison_double_digits() {
        assert!(version_newer("0.10.0", "0.9.0"));
        assert!(version_newer("0.10.0", "0.2.0"));
        assert!(version_newer("1.0.0", "0.99.99"));
    }

    #[test]
    fn test_version_comparison_equal() {
        assert!(!version_newer("0.1.0", "0.1.0"));
        assert!(!version_newer("1.0.0", "1.0.0"));
    }

    #[test]
    fn test_version_comparison_older() {
        assert!(!version_newer("0.1.0", "0.2.0"));
        assert!(!version_newer("0.9.0", "1.0.0"));
    }

    #[test]
    fn test_version_with_v_prefix() {
        assert!(version_newer("v0.2.0", "v0.1.0"));
        assert!(version_newer("v0.2.0", "0.1.0"));
        assert!(version_newer("0.2.0", "v0.1.0"));
    }

    #[test]
    fn test_version_more_parts() {
        assert!(version_newer("0.1.0.1", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.0.1"));
    }
}