//! Package manifest validation tests.
//!
//! These tests validate the syntax and structure of package manager manifests
//! used for distributing br through Homebrew, Scoop, and AUR.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Test that the Homebrew formula has valid Ruby syntax.
#[test]
fn test_homebrew_formula_syntax() {
    let formula_path = Path::new("packaging/homebrew/br.rb");

    if !formula_path.exists() {
        eprintln!("Skipping: Homebrew formula not found at {formula_path:?}");
        return;
    }

    let content = fs::read_to_string(formula_path).expect("Failed to read Homebrew formula");

    // Basic structure checks
    assert!(
        content.contains("class Br < Formula"),
        "Formula must define Br class extending Formula"
    );
    assert!(
        content.contains("desc \""),
        "Formula must have a description"
    );
    assert!(
        content.contains("homepage \""),
        "Formula must have a homepage"
    );
    assert!(
        content.contains("license :cannot_represent"),
        "Formula must mark the rider-bearing license as non-SPDX"
    );
    assert!(
        content.contains("version \""),
        "Formula must have a version"
    );

    // Platform-specific URLs
    assert!(
        content.contains("on_macos do"),
        "Formula must have macOS platform section"
    );
    assert!(
        content.contains("on_linux do"),
        "Formula must have Linux platform section"
    );
    assert!(
        content.contains("on_arm do"),
        "Formula must have ARM architecture section"
    );
    assert!(
        content.contains("on_intel do"),
        "Formula must have Intel architecture section"
    );

    // Install and test blocks
    assert!(
        content.contains("def install"),
        "Formula must have install method"
    );
    assert!(content.contains("test do"), "Formula must have test block");

    // Check Ruby syntax if ruby is available
    if Command::new("ruby").arg("--version").output().is_ok() {
        let output = Command::new("ruby")
            .arg("-c")
            .arg(formula_path)
            .output()
            .expect("Failed to run ruby syntax check");

        assert!(
            output.status.success(),
            "Ruby syntax check failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Test that the Scoop manifest has valid JSON schema.
#[test]
fn test_scoop_manifest_schema() {
    let manifest_path = Path::new("packaging/scoop/br.json");

    if !manifest_path.exists() {
        eprintln!("Skipping: Scoop manifest not found at {manifest_path:?}");
        return;
    }

    let content = fs::read_to_string(manifest_path).expect("Failed to read Scoop manifest");

    // Parse as JSON
    let json: serde_json::Value =
        serde_json::from_str(&content).expect("Scoop manifest must be valid JSON");

    // Required fields
    assert!(
        json.get("version").is_some(),
        "Manifest must have 'version' field"
    );
    assert!(
        json.get("description").is_some(),
        "Manifest must have 'description' field"
    );
    assert!(
        json.get("homepage").is_some(),
        "Manifest must have 'homepage' field"
    );
    assert!(
        json.get("license").is_some(),
        "Manifest must have 'license' field"
    );
    assert_eq!(
        json.get("license").and_then(serde_json::Value::as_str),
        Some("https://github.com/Dicklesworthstone/beads_rust/blob/main/LICENSE"),
        "Scoop must link to the repository's rider-bearing license"
    );
    assert!(json.get("bin").is_some(), "Manifest must have 'bin' field");

    // Architecture section
    let arch = json
        .get("architecture")
        .expect("Manifest must have 'architecture' field");
    assert!(
        arch.get("64bit").is_some(),
        "Manifest must have '64bit' architecture"
    );

    // 64bit must have url and hash
    let arch_64 = arch.get("64bit").unwrap();
    assert!(
        arch_64.get("url").is_some(),
        "64bit architecture must have 'url'"
    );
    assert!(
        arch_64.get("hash").is_some(),
        "64bit architecture must have 'hash'"
    );

    // Autoupdate section (optional but recommended)
    if let Some(autoupdate) = json.get("autoupdate") {
        assert!(
            autoupdate.get("architecture").is_some(),
            "autoupdate must have 'architecture' section"
        );
    }

    // URL format validation
    let url = arch_64.get("url").unwrap().as_str().unwrap();
    assert!(url.starts_with("https://"), "URL must use HTTPS: {url}");
    assert!(
        url.contains("github.com"),
        "URL should point to GitHub releases"
    );
    // Allow case-sensitive comparison since URLs are case-sensitive
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    {
        assert!(url.ends_with(".zip"), "Windows URL should be a .zip file");
    }
}

/// Test that the AUR PKGBUILD has valid shell syntax.
#[test]
fn test_pkgbuild_syntax() {
    let pkgbuild_path = Path::new("packaging/aur/PKGBUILD");

    if !pkgbuild_path.exists() {
        eprintln!("Skipping: PKGBUILD not found at {pkgbuild_path:?}");
        return;
    }

    let content = fs::read_to_string(pkgbuild_path).expect("Failed to read PKGBUILD");

    // Required variables
    assert!(content.contains("pkgname="), "PKGBUILD must define pkgname");
    assert!(content.contains("pkgver="), "PKGBUILD must define pkgver");
    assert!(content.contains("pkgrel="), "PKGBUILD must define pkgrel");
    assert!(content.contains("pkgdesc="), "PKGBUILD must define pkgdesc");
    assert!(content.contains("arch="), "PKGBUILD must define arch");
    assert!(content.contains("url="), "PKGBUILD must define url");
    assert!(
        content.contains("license=('LicenseRef-MIT-OpenAI-Anthropic-Rider')"),
        "PKGBUILD must use a custom SPDX LicenseRef for the rider-bearing license"
    );

    // Source arrays for both architectures
    assert!(
        content.contains("source_x86_64="),
        "PKGBUILD must have x86_64 sources"
    );
    assert!(
        content.contains("source_aarch64="),
        "PKGBUILD must have aarch64 sources"
    );

    // SHA256 sums
    assert!(
        content.contains("sha256sums_x86_64="),
        "PKGBUILD must have x86_64 checksums"
    );
    assert!(
        content.contains("sha256sums_aarch64="),
        "PKGBUILD must have aarch64 checksums"
    );

    // Package function
    assert!(
        content.contains("package()"),
        "PKGBUILD must have package() function"
    );
    assert!(
        content.contains("/usr/share/licenses/${pkgname}/LICENSE"),
        "PKGBUILD must install the rider-bearing license text"
    );

    // Check bash syntax if bash is available
    if Command::new("bash").arg("--version").output().is_ok() {
        let output = Command::new("bash")
            .arg("-n")
            .arg(pkgbuild_path)
            .output()
            .expect("Failed to run bash syntax check");

        assert!(
            output.status.success(),
            "Bash syntax check failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Test that Cargo.toml has proper metadata for crates.io publishing.
#[test]
fn test_cargo_metadata() {
    let cargo_toml = fs::read_to_string("Cargo.toml").expect("Failed to read Cargo.toml");
    let manifest: toml::Value =
        toml::from_str(&cargo_toml).expect("Cargo.toml must be valid TOML");
    let package = manifest
        .get("package")
        .and_then(toml::Value::as_table)
        .expect("Cargo.toml must have a package table");

    // Required fields for crates.io
    assert!(cargo_toml.contains("name = "), "Cargo.toml must have name");
    assert!(
        cargo_toml.contains("version = "),
        "Cargo.toml must have version"
    );
    assert!(
        cargo_toml.contains("description = "),
        "Cargo.toml must have description for crates.io"
    );
    assert_eq!(
        package.get("license-file").and_then(toml::Value::as_str),
        Some("LICENSE"),
        "Cargo.toml must publish the repository's nonstandard license text"
    );
    assert!(
        package.get("license").is_none(),
        "Cargo.toml must not mislabel the rider-bearing license as a standard SPDX license"
    );
    assert!(
        cargo_toml.contains("repository = "),
        "Cargo.toml should have repository URL"
    );

    // Recommended fields
    assert!(
        cargo_toml.contains("keywords = "),
        "Cargo.toml should have keywords for discoverability"
    );
    assert!(
        cargo_toml.contains("categories = "),
        "Cargo.toml should have categories for crates.io"
    );

    // Binary definition
    assert!(
        cargo_toml.contains("[[bin]]"),
        "Cargo.toml must define binary target"
    );
    assert!(
        cargo_toml.contains("name = \"br\""),
        "Binary must be named 'br'"
    );
}

#[test]
fn test_source_and_nix_manifests_preserve_custom_license_metadata() {
    let pkgbuild_git = fs::read_to_string("packaging/aur/PKGBUILD-git")
        .expect("Failed to read source AUR PKGBUILD");
    assert!(
        pkgbuild_git.contains("license=('LicenseRef-MIT-OpenAI-Anthropic-Rider')"),
        "source PKGBUILD must use the custom rider LicenseRef"
    );
    assert!(
        pkgbuild_git.contains("/usr/share/licenses/${pkgname}/LICENSE"),
        "source PKGBUILD must install the rider-bearing license text"
    );

    let flake = fs::read_to_string("flake.nix").expect("Failed to read flake.nix");
    assert!(
        flake.contains("license = licenses.unfree;"),
        "Nix metadata must not classify the rider-bearing license as MIT/free"
    );
    assert!(
        flake.contains("$out/share/licenses/beads_rust/LICENSE"),
        "Nix package must install the rider-bearing license text"
    );
}

/// Source-install instructions must be unambiguous and use the validated lockfile.
///
/// Without `--locked`, `cargo install` resolves a fresh dependency graph that
/// can require compiler features newer than the deliberately pinned nightly.
/// Git installs must also select `beads_rust` because the repository contains
/// a second Cargo package for fuzzing.
#[test]
fn test_source_install_documentation_uses_locked_resolution() {
    for path in [
        "README.md",
        "docs/INSTALLING.md",
        "docs/AGENT_INTEGRATION.md",
        "docs/CLI_REFERENCE.md",
        "Cargo.toml",
        ".github/workflows/release.yml",
    ] {
        let content =
            fs::read_to_string(path).expect("Failed to read source-install documentation");
        let logical_lines = content.replace("\\\n", " ");

        for (line_index, line) in logical_lines.lines().enumerate() {
            let is_git_source_install = line.contains(
                "cargo install --git https://github.com/Dicklesworthstone/beads_rust.git",
            );
            let is_source_install =
                line.contains("cargo install --path .") || is_git_source_install;
            if is_source_install {
                assert!(
                    line.contains("--locked"),
                    "{path}:{} source-install command must include --locked: {line}",
                    line_index + 1
                );
            }
            if is_git_source_install {
                assert!(
                    line.contains(
                        "cargo install --git https://github.com/Dicklesworthstone/beads_rust.git beads_rust",
                    ),
                    "{path}:{} git source-install command must select the beads_rust package: {line}",
                    line_index + 1
                );
            }
        }
    }
}

/// Test that all package manifests carry a version no newer than Cargo.toml.
///
/// The `update-package-manifests.yml` workflow rewrites every packaging
/// manifest (Homebrew, Scoop, AUR) after a release is published, driven by
/// the release tag and the checksums attached to the release.  During
/// development Cargo.toml is bumped ahead of the manifests: the manifests
/// catch up only once a tagged release has been built and CI has rewritten
/// them.  This test therefore asserts "manifest version parses and is not
/// ahead of Cargo.toml" rather than requiring exact equality, which would
/// otherwise fail on every pre-release commit.
#[test]
fn test_version_consistency() -> Result<(), String> {
    fn parse_version(raw: &str, source: &str) -> Result<semver::Version, String> {
        semver::Version::parse(raw.trim())
            .map_err(|err| format!("{source} version '{raw}' is not valid semver: {err}"))
    }

    let cargo_toml = fs::read_to_string("Cargo.toml").expect("Failed to read Cargo.toml");
    let cargo_version_str = cargo_toml
        .lines()
        .find(|line| line.starts_with("version = "))
        .and_then(|line| line.split('"').nth(1))
        .expect("Could not find version in Cargo.toml");
    let cargo_version = parse_version(cargo_version_str, "Cargo.toml")?;

    let formula_path = Path::new("packaging/homebrew/br.rb");
    if formula_path.exists() {
        let formula = fs::read_to_string(formula_path).expect("Failed to read Homebrew formula");
        let raw = formula
            .lines()
            .find_map(|line| {
                let line = line.trim();
                line.strip_prefix("version \"")
                    .and_then(|rest| rest.strip_suffix('"'))
            })
            .expect("Homebrew formula missing `version \"…\"` line");
        let manifest_version = parse_version(raw, "Homebrew formula")?;
        assert!(
            manifest_version <= cargo_version,
            "Homebrew formula version {manifest_version} is ahead of Cargo.toml {cargo_version}"
        );
    }

    let scoop_path = Path::new("packaging/scoop/br.json");
    if scoop_path.exists() {
        let scoop = fs::read_to_string(scoop_path).expect("Failed to read Scoop manifest");
        let scoop_json: serde_json::Value =
            serde_json::from_str(&scoop).expect("Invalid Scoop JSON");
        let scoop_version_str = scoop_json
            .get("version")
            .and_then(serde_json::Value::as_str)
            .expect("Scoop manifest missing `version`");
        let scoop_version = parse_version(scoop_version_str, "Scoop manifest")?;
        assert!(
            scoop_version <= cargo_version,
            "Scoop manifest version {scoop_version} is ahead of Cargo.toml {cargo_version}"
        );
    }

    let pkgbuild_path = Path::new("packaging/aur/PKGBUILD");
    if pkgbuild_path.exists() {
        let pkgbuild = fs::read_to_string(pkgbuild_path).expect("Failed to read PKGBUILD");
        let raw = pkgbuild
            .lines()
            .find_map(|line| line.trim().strip_prefix("pkgver="))
            .expect("PKGBUILD missing `pkgver=` line");
        let manifest_version = parse_version(raw, "PKGBUILD")?;
        assert!(
            manifest_version <= cargo_version,
            "PKGBUILD pkgver {manifest_version} is ahead of Cargo.toml {cargo_version}"
        );
    }

    Ok(())
}

/// Package-manager manifests must follow the artifact names that DSR publishes.
///
/// `br-v<version>-...` was the older GitHub Actions naming convention. DSR
/// publishes installer-compatible archives as `br-<version>-<platform>...`, so
/// a stale `br-v...` URL makes package-manager automation look for assets that
/// do not exist on the release.
#[test]
fn test_package_manifests_use_dsr_asset_names() {
    for path in [
        "packaging/homebrew/br.rb",
        "packaging/scoop/br.json",
        "packaging/aur/PKGBUILD",
    ] {
        let content = fs::read_to_string(path).expect("Failed to read package manifest");
        assert!(
            !content.contains("PLACEHOLDER_"),
            "{path} must not ship placeholder checksums"
        );
        assert!(
            !content.contains("br-v"),
            "{path} must use DSR br-<version> asset names, not stale br-v names"
        );
    }

    let formula =
        fs::read_to_string("packaging/homebrew/br.rb").expect("Failed to read Homebrew formula");
    assert!(formula.contains("br-#{version}-darwin_arm64.tar.gz"));
    assert!(formula.contains("br-#{version}-darwin_amd64.tar.gz"));
    assert!(formula.contains("br-#{version}-linux_arm64.tar.gz"));
    assert!(formula.contains("br-#{version}-linux_amd64.tar.gz"));

    let scoop = fs::read_to_string("packaging/scoop/br.json").expect("Failed to read Scoop file");
    assert!(scoop.contains("br-$version-windows_amd64.zip"));

    let pkgbuild = fs::read_to_string("packaging/aur/PKGBUILD").expect("Failed to read PKGBUILD");
    assert!(pkgbuild.contains("br-${pkgver}-linux_amd64.tar.gz"));
    assert!(pkgbuild.contains("br-${pkgver}-linux_arm64.tar.gz"));
}

#[test]
fn test_update_package_manifests_workflow_uses_current_checksums() {
    let workflow = fs::read_to_string(".github/workflows/update-package-manifests.yml")
        .expect("Failed to read update-package-manifests workflow");

    assert!(
        workflow.contains(r#"FILE="br-${VERSION}-${platform}.${ext}.sha256""#),
        "workflow must download the DSR-published checksum sidecars"
    );
    assert!(
        workflow.contains(r#"VERSION="${VERSION#v}""#),
        "workflow_dispatch inputs must normalize an optional leading v before building asset names"
    );
    assert!(
        !workflow.contains("br-v${VERSION}"),
        "workflow must not look for stale br-v checksum sidecars"
    );
    assert!(
        workflow.contains("curl -fsSL"),
        "checksum download must fail fast instead of saving a 404 body"
    );
    assert!(
        workflow.contains("Invalid SHA256"),
        "workflow must validate checksum file contents before updating manifests"
    );
}
