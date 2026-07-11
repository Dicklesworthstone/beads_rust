//! Upgrade command implementation.
//!
//! Enables br to update itself to the latest version using the `self_update` crate.

use crate::cli::UpgradeArgs;
use crate::cli::commands::{
    GITHUB_REPO_NAME, GITHUB_REPO_OWNER, github_raw_main_url, github_releases_url,
};
use crate::error::{BeadsError, Result};
use crate::output::{OutputContext, OutputMode};
use crate::util::hex_encode;
use rich_rust::prelude::*;
use self_update::backends::github;
use self_update::cargo_crate_version;
use self_update::update::{Release, ReleaseAsset, ReleaseUpdate};
use self_update::{Download, Extract, Move, VersionStatus};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

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
pub fn execute(args: &UpgradeArgs, ctx: &OutputContext) -> Result<()> {
    let current_version = cargo_crate_version!();

    if args.dry_run {
        return execute_dry_run(args, current_version, ctx);
    }

    if args.check {
        return execute_check(current_version, ctx);
    }

    execute_upgrade(args, current_version, ctx)
}

/// Execute check-only mode.
fn execute_check(current_version: &str, ctx: &OutputContext) -> Result<()> {
    tracing::info!("Checking for updates...");

    let updater = build_updater(current_version)?;
    let releases = updater.get_latest_release().map_err(map_update_error)?;
    let latest = releases
        .latest()
        .ok_or_else(|| BeadsError::upgrade("No releases found"))?;
    let latest_version = latest.version();

    let update_available = version_newer(latest_version, current_version);

    let download_url = release_binary_asset_for(&latest, asset_target_name(), None)
        .map(|asset| asset.download_url().to_string());

    let result = UpdateCheckResult {
        current_version: current_version.to_string(),
        latest_version: latest_version.to_string(),
        update_available,
        download_url,
    };

    if ctx.is_json() {
        ctx.json_pretty(&result);
    } else if ctx.is_quiet() {
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_check_rich(&result, ctx);
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
fn execute_dry_run(args: &UpgradeArgs, current_version: &str, ctx: &OutputContext) -> Result<()> {
    tracing::info!("Dry-run mode: checking what would happen...");

    let target_version = args.version.as_deref();
    let updater = build_updater(current_version)?;
    let releases = updater.get_latest_release().map_err(map_update_error)?;
    let latest = releases
        .latest()
        .ok_or_else(|| BeadsError::upgrade("No releases found"))?;
    let latest_version = latest.version();

    let install_version = target_version.unwrap_or(latest_version);
    let would_update = args.force || version_newer(install_version, current_version);

    let download_url = release_binary_asset_for(&latest, asset_target_name(), None)
        .map_or_else(|| "N/A".to_string(), |a| a.download_url().to_string());

    if ctx.is_json() {
        let result = serde_json::json!({
            "dry_run": true,
            "current_version": current_version,
            "target_version": install_version,
            "would_download": download_url,
            "would_update": would_update,
        });
        ctx.json_pretty(&result);
    } else if ctx.is_quiet() {
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_dry_run_rich(
            current_version,
            install_version,
            &download_url,
            would_update,
            ctx,
        );
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
fn execute_upgrade(args: &UpgradeArgs, current_version: &str, ctx: &OutputContext) -> Result<()> {
    tracing::info!(current = %current_version, "Starting upgrade...");

    let is_json = ctx.is_json();
    let is_quiet = ctx.is_quiet();
    let is_rich = matches!(ctx.mode(), OutputMode::Rich);

    if !is_json && !is_quiet && !is_rich {
        println!("Checking for updates...");
        println!("Current version: {current_version}");
    } else if is_rich {
        ctx.info(&format!(
            "Checking for updates (current: {current_version})..."
        ));
    }

    let updater = if let Some(ref target_version) = args.version {
        build_updater_with_target(target_version, current_version, !is_json && !is_rich)?
    } else {
        build_updater(current_version)?
    };

    let Some(release) = select_release_for_update(updater.as_ref(), current_version)? else {
        render_not_updated(
            current_version,
            current_version,
            ctx,
            is_json,
            is_quiet,
            is_rich,
        );
        return Ok(());
    };
    let latest_version = release.version();

    if !is_json && !is_quiet && !is_rich {
        println!("Latest version:  {latest_version}");
    }

    let update_available = args.force || version_newer(latest_version, current_version);

    if !update_available {
        render_not_updated(
            current_version,
            latest_version,
            ctx,
            is_json,
            is_quiet,
            is_rich,
        );
        return Ok(());
    }

    if !is_json && !is_quiet && !is_rich {
        println!("\nDownloading {latest_version}...");
    } else if is_rich {
        ctx.info(&format!("Downloading {latest_version}..."));
    }

    // Perform the update only after the downloaded archive matches the release
    // checksum asset. This command handles the release asset
    // download/extract/replace sequence directly because br releases publish a
    // separate `<archive>.sha256` asset.
    let status = update_with_checksum(updater.as_ref(), &release).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("archive-tar") || msg.contains("ArchiveNotEnabled") || msg.contains("tar") {
            BeadsError::upgrade(archive_support_error_message(&msg))
        } else {
            map_update_error(e)
        }
    })?;

    let result = UpdateResult {
        current_version: current_version.to_string(),
        new_version: status.version().to_string(),
        updated: status.is_updated(),
        message: if status.is_updated() {
            Some(format!("Updated to {}", status.version()))
        } else {
            Some("No update performed".to_string())
        },
    };

    if is_json {
        ctx.json_pretty(&result);
    } else if is_quiet {
    } else if is_rich {
        render_upgrade_result_rich(&result, current_version, ctx);
    } else if status.is_updated() {
        println!(
            "\n\u{2713} Updated br from {current_version} to {}",
            status.version()
        );
    } else {
        println!("\n\u{2713} Already up to date");
    }

    Ok(())
}

fn archive_support_error_message(msg: &str) -> String {
    let install_url = github_raw_main_url("install.sh");
    let releases_url = github_releases_url();
    format!(
        "{msg}\n\n\
         This binary was built without archive support for the required format (e.g., .tar.gz).\n\
         This is a known issue in some older versions (e.g., 0.1.21 - 0.1.26). Version 0.1.27 and later include the correct 'archive-tar' linkage.\n\n\
         Please upgrade manually by running:\n\n  \
         curl -fsSL {install_url} | bash\n\n\
         Or by downloading the release from:\n  \
         {releases_url}\n\n\
         After that, `br upgrade` will work correctly for future updates."
    )
}

fn render_not_updated(
    current_version: &str,
    latest_version: &str,
    ctx: &OutputContext,
    is_json: bool,
    is_quiet: bool,
    is_rich: bool,
) {
    let result = UpdateResult {
        current_version: current_version.to_string(),
        new_version: latest_version.to_string(),
        updated: false,
        message: Some("Already up to date".to_string()),
    };

    if is_json {
        ctx.json_pretty(&result);
    } else if is_quiet {
    } else if is_rich {
        render_up_to_date_rich(current_version, latest_version, ctx);
    } else {
        println!("\n\u{2713} Already up to date");
    }
}

/// Resolve a GitHub auth token from environment variables.
///
/// Checks `GITHUB_TOKEN` first, then `GH_TOKEN`. Returns `None` if neither
/// is set or if the value is empty.
fn resolve_auth_token() -> Option<String> {
    env::var("GITHUB_TOKEN")
        .or_else(|_| env::var("GH_TOKEN"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Map the Rust target triple to the asset name fragment used in GitHub releases.
///
/// Release assets follow the pattern `br-{VERSION}-{platform}_{arch}.tar.gz`
/// (e.g. `darwin_amd64`, `linux_arm64`), which differs from the Rust target
/// triple that `self_update` uses by default (e.g. `x86_64-apple-darwin`).
fn asset_target_name() -> &'static str {
    match self_update::get_target() {
        "x86_64-apple-darwin" => "darwin_amd64",
        "aarch64-apple-darwin" => "darwin_arm64",
        "x86_64-unknown-linux-gnu" => "linux_amd64",
        "x86_64-unknown-linux-musl" => "linux_musl_amd64",
        "aarch64-unknown-linux-gnu" => "linux_arm64",
        "aarch64-unknown-linux-musl" => "linux_musl_arm64",
        "x86_64-pc-windows-msvc" | "x86_64-pc-windows-gnu" => "windows_amd64",
        other => other, // fall back to the raw triple for unknown targets
    }
}

fn is_archive_asset_name(name: &str) -> bool {
    let path = Path::new(name);
    if path.extension().is_some_and(|ext| {
        ext.eq_ignore_ascii_case("sha256") || ext.eq_ignore_ascii_case("minisig")
    }) {
        return false;
    }

    has_tar_gz_extension(path)
        || path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

fn has_tar_gz_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gz"))
        && path
            .file_stem()
            .and_then(|stem| Path::new(stem).extension())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tar"))
}

fn release_binary_asset_for<'a>(
    release: &'a Release,
    target: &str,
    identifier: Option<&str>,
) -> Option<&'a ReleaseAsset> {
    release
        .assets()
        .iter()
        .find(|asset| {
            is_archive_asset_name(asset.name())
                && asset.name().contains(target)
                && identifier.is_none_or(|id| asset.name().contains(id))
        })
        .or_else(|| {
            release.assets().iter().find(|asset| {
                is_archive_asset_name(asset.name())
                    && identifier.is_none_or(|id| asset.name().contains(id))
            })
        })
}

fn checksum_asset_for<'a>(release: &'a Release, archive_name: &str) -> Option<&'a ReleaseAsset> {
    let expected_name = format!("{archive_name}.sha256");
    release
        .assets()
        .iter()
        .find(|asset| asset.name() == expected_name)
}

fn select_release_for_update(
    updater: &dyn ReleaseUpdate,
    current_version: &str,
) -> Result<Option<Release>> {
    if let Some(target_version) = updater.release_tag() {
        return updater
            .get_release_version(target_version)
            .map(Some)
            .map_err(map_update_error);
    }

    let releases = updater
        .get_newer_releases()
        .map_err(map_update_error)?
        .into_vec();
    let compatible = releases
        .iter()
        .find(|release| {
            self_update::version::bump_is_compatible(current_version, release.version())
                .unwrap_or(false)
        })
        .cloned();

    Ok(compatible.or_else(|| releases.into_iter().next()))
}

fn update_with_checksum(updater: &dyn ReleaseUpdate, release: &Release) -> Result<VersionStatus> {
    let target = updater.target();
    let target_asset = release_binary_asset_for(release, target, updater.asset_identifier())
        .ok_or_else(|| {
            BeadsError::upgrade(format!("No release archive found for target `{target}`"))
        })?;
    let checksum_asset = checksum_asset_for(release, target_asset.name()).ok_or_else(|| {
        let expected_checksum_name = format!("{}.sha256", target_asset.name());
        BeadsError::upgrade(format!(
            "Missing SHA256 checksum asset `{}` for release archive `{}`",
            expected_checksum_name,
            target_asset.name()
        ))
    })?;

    let tmp_archive_dir = tempfile::TempDir::new()
        .map_err(|err| BeadsError::upgrade(format!("failed to create temp dir: {err}")))?;
    let tmp_archive_path = tmp_archive_dir.path().join(target_asset.name());
    let mut tmp_archive = File::create(&tmp_archive_path)
        .map_err(|err| BeadsError::upgrade(format!("failed to create temp archive: {err}")))?;

    download_asset_to_writer(
        target_asset.download_url(),
        &mut tmp_archive,
        updater.show_download_progress(),
        updater.auth_token(),
    )?;
    drop(tmp_archive);

    let expected_sha256 =
        download_expected_sha256(checksum_asset, target_asset.name(), updater.auth_token())?;
    verify_sha256(&tmp_archive_path, &expected_sha256)?;

    let bin_path_in_archive = updater.bin_path_in_archive();
    Extract::from_source(&tmp_archive_path)
        .extract_file(tmp_archive_dir.path(), bin_path_in_archive)
        .map_err(map_update_error)?;

    let new_exe = tmp_archive_dir.path().join(bin_path_in_archive);
    let bin_install_path = updater.bin_install_path();
    if bin_install_path
        == env::current_exe().map_err(|err| {
            BeadsError::upgrade(format!("failed to locate current executable: {err}"))
        })?
    {
        self_replace::self_replace(new_exe).map_err(|err| BeadsError::upgrade(err.to_string()))?;
    } else {
        Move::from_source(&new_exe)
            .to_dest(bin_install_path)
            .map_err(map_update_error)?;
    }

    Ok(VersionStatus::Updated(release.version().to_string()))
}

fn download_expected_sha256(
    checksum_asset: &ReleaseAsset,
    archive_name: &str,
    auth_token: Option<&str>,
) -> Result<String> {
    let mut bytes = Vec::new();
    download_asset_to_writer(checksum_asset.download_url(), &mut bytes, false, auth_token)?;
    let checksum_text = String::from_utf8(bytes)
        .map_err(|err| BeadsError::upgrade(format!("checksum asset is not UTF-8: {err}")))?;
    parse_sha256_checksum(&checksum_text, archive_name)
}

fn parse_sha256_checksum(checksum_text: &str, archive_name: &str) -> Result<String> {
    for line in checksum_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let filename = parts.next();
        if filename.is_some_and(|name| name != archive_name) {
            continue;
        }
        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(hash.to_ascii_lowercase());
        }
    }

    Err(BeadsError::upgrade(format!(
        "checksum asset did not contain a SHA256 entry for `{archive_name}`"
    )))
}

fn verify_sha256(path: &Path, expected_sha256: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual.eq_ignore_ascii_case(expected_sha256) {
        return Ok(());
    }

    Err(BeadsError::upgrade(format!(
        "SHA256 verification failed for {}: expected {}, got {}",
        path.display(),
        expected_sha256,
        actual
    )))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .map_err(|err| BeadsError::upgrade(format!("failed to open downloaded archive: {err}")))?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 8192];

    loop {
        let n = file.read(&mut buf).map_err(|err| {
            BeadsError::upgrade(format!("failed to read downloaded archive: {err}"))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(hex_encode(&hasher.finalize()))
}

fn download_asset_to_writer(
    url: &str,
    writer: &mut impl Write,
    show_progress: bool,
    auth_token: Option<&str>,
) -> Result<()> {
    let mut download = Download::from_url(url);
    download.show_download_progress(show_progress);
    download.request_header("ACCEPT", "application/octet-stream");
    if let Some(token) = auth_token {
        download.request_header("AUTHORIZATION", format!("token {token}"));
    }

    download.download_to(writer).map_err(map_update_error)
}

/// Build the self-update updater.
fn build_updater(current_version: &str) -> Result<Box<dyn ReleaseUpdate>> {
    let mut builder = github::Update::configure();
    builder
        .repo_owner(GITHUB_REPO_OWNER)
        .repo_name(GITHUB_REPO_NAME)
        .bin_name(BIN_NAME)
        .target(asset_target_name())
        .show_download_progress(true)
        .no_confirm(true)
        .current_version(current_version);

    if let Some(token) = resolve_auth_token() {
        tracing::debug!("Using GitHub auth token from environment");
        builder.auth_token(&token);
    }

    builder
        .build()
        .map(|updater| Box::new(updater) as Box<dyn ReleaseUpdate>)
        .map_err(map_update_error)
}

/// Build updater with a specific target version.
fn build_updater_with_target(
    target_version: &str,
    current_version: &str,
    show_progress: bool,
) -> Result<Box<dyn ReleaseUpdate>> {
    let mut builder = github::Update::configure();
    builder
        .repo_owner(GITHUB_REPO_OWNER)
        .repo_name(GITHUB_REPO_NAME)
        .bin_name(BIN_NAME)
        .target(asset_target_name())
        .show_download_progress(show_progress)
        .no_confirm(true)
        .current_version(current_version)
        .release_tag(target_version);

    if let Some(token) = resolve_auth_token() {
        tracing::debug!("Using GitHub auth token from environment");
        builder.auth_token(&token);
    }

    builder
        .build()
        .map(|updater| Box::new(updater) as Box<dyn ReleaseUpdate>)
        .map_err(map_update_error)
}

/// Map `self_update` errors to `BeadsError`.
fn map_update_error<E: std::error::Error + Send + Sync + 'static>(err: E) -> BeadsError {
    BeadsError::upgrade(err.to_string())
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

// ─────────────────────────────────────────────────────────────
// Rich Output Rendering
// ─────────────────────────────────────────────────────────────

/// Render update check results with rich formatting.
fn render_check_rich(result: &UpdateCheckResult, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    // Version comparison
    content.append_styled("Current version:  ", theme.dimmed.clone());
    content.append_styled(&result.current_version, theme.emphasis.clone());
    content.append("\n");

    content.append_styled("Latest version:   ", theme.dimmed.clone());
    if result.update_available {
        content.append_styled(&result.latest_version, theme.success.clone());
        content.append_styled(" ✓ NEW", theme.success.clone());
    } else {
        content.append_styled(&result.latest_version, theme.emphasis.clone());
    }
    content.append("\n\n");

    // Status message
    if result.update_available {
        content.append_styled("↑ ", theme.success.clone());
        content.append_styled("Update available! ", theme.success.clone());
        content.append("Run ");
        content.append_styled("`br upgrade`", theme.accent.clone());
        content.append(" to install.\n");
    } else {
        content.append_styled("✓ ", theme.success.clone());
        content.append("Already up to date\n");
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Upgrade Check", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render dry-run results with rich formatting.
fn render_dry_run_rich(
    current_version: &str,
    target_version: &str,
    download_url: &str,
    would_update: bool,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    content.append_styled("⚡ Dry-run mode ", theme.warning.clone());
    content.append_styled("(no changes will be made)\n\n", theme.dimmed.clone());

    content.append_styled("Current version:  ", theme.dimmed.clone());
    content.append_styled(current_version, theme.emphasis.clone());
    content.append("\n");

    content.append_styled("Target version:   ", theme.dimmed.clone());
    content.append_styled(target_version, theme.emphasis.clone());
    content.append("\n");

    content.append_styled("Would download:   ", theme.dimmed.clone());
    content.append_styled(download_url, theme.accent.clone());
    content.append("\n");

    content.append_styled("Would install:    ", theme.dimmed.clone());
    if would_update {
        content.append_styled("yes", theme.success.clone());
    } else {
        content.append_styled("no (already up to date)", theme.warning.clone());
    }
    content.append("\n\n");

    content.append_styled("No changes made.", theme.dimmed.clone());
    content.append("\n");

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Upgrade Dry Run", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render "already up to date" message with rich formatting.
fn render_up_to_date_rich(current_version: &str, latest_version: &str, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    content.append_styled("Current version:  ", theme.dimmed.clone());
    content.append_styled(current_version, theme.emphasis.clone());
    content.append("\n");

    content.append_styled("Latest version:   ", theme.dimmed.clone());
    content.append_styled(latest_version, theme.emphasis.clone());
    content.append("\n\n");

    content.append_styled("✓ ", theme.success.clone());
    content.append("Already up to date\n");

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Upgrade Status", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render upgrade result with rich formatting.
fn render_upgrade_result_rich(result: &UpdateResult, current_version: &str, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled("✓ ", theme.success.clone());

    if result.updated {
        content.append_styled("Upgraded ", theme.success.clone());
        content.append("br from ");
        content.append_styled(current_version, theme.dimmed.clone());
        content.append(" to ");
        content.append_styled(&result.new_version, theme.success.clone());
        content.append("\n");
    } else {
        content.append("Already up to date\n");
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Upgrade Complete", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
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

    #[test]
    fn archive_support_error_message_uses_shared_repo_urls() {
        let message = archive_support_error_message("archive-tar disabled");

        assert!(message.contains(&github_raw_main_url("install.sh")));
        assert!(message.contains(&github_releases_url()));
    }

    fn release_with_assets(names: &[&str]) -> Release {
        Release::builder()
            .name("v1.2.3")
            .version("1.2.3")
            .date("2026-04-22T00:00:00Z")
            .assets(names.iter().map(|name| {
                ReleaseAsset::new(*name, format!("https://api.github.com/assets/{name}"))
            }))
            .build()
            .expect("valid release fixture")
    }

    #[test]
    fn release_binary_asset_ignores_checksum_and_signature_assets() {
        let release = release_with_assets(&[
            "br-1.2.3-linux_amd64.tar.gz.sha256",
            "br-1.2.3-linux_amd64.tar.gz.minisig",
            "br-1.2.3-linux_amd64.tar.gz",
        ]);

        let asset = release_binary_asset_for(&release, "linux_amd64", None).unwrap();
        assert_eq!(asset.name(), "br-1.2.3-linux_amd64.tar.gz");
    }

    #[test]
    fn checksum_asset_matches_archive_name_exactly() {
        let release = release_with_assets(&[
            "br-1.2.3-linux_arm64.tar.gz.sha256",
            "br-1.2.3-linux_amd64.tar.gz.sha256",
            "br-1.2.3-linux_amd64.tar.gz",
        ]);

        let asset = checksum_asset_for(&release, "br-1.2.3-linux_amd64.tar.gz").unwrap();
        assert_eq!(asset.name(), "br-1.2.3-linux_amd64.tar.gz.sha256");
    }

    #[test]
    fn parse_sha256_checksum_accepts_standard_sha256sum_line() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let checksum = parse_sha256_checksum(
            &format!("{hash}  br-1.2.3-linux_amd64.tar.gz\n"),
            "br-1.2.3-linux_amd64.tar.gz",
        )
        .unwrap();
        assert_eq!(checksum, hash);
    }

    #[test]
    fn parse_sha256_checksum_rejects_wrong_archive_name() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let err = parse_sha256_checksum(
            &format!("{hash}  br-1.2.3-linux_arm64.tar.gz\n"),
            "br-1.2.3-linux_amd64.tar.gz",
        )
        .unwrap_err();
        assert!(err.to_string().contains("linux_amd64"));
    }
}
