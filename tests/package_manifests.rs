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
        content.contains("license \""),
        "Formula must have a license"
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
    assert!(content.contains("license="), "PKGBUILD must define license");

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
    assert!(
        cargo_toml.contains("license = "),
        "Cargo.toml must have license for crates.io"
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

/// Test that all package manifests have consistent version numbers.
#[test]
fn test_version_consistency() {
    // Extract version from Cargo.toml
    let cargo_toml = fs::read_to_string("Cargo.toml").expect("Failed to read Cargo.toml");
    let cargo_version = cargo_toml
        .lines()
        .find(|line| line.starts_with("version = "))
        .and_then(|line| line.split('"').nth(1))
        .expect("Could not find version in Cargo.toml");

    // Check Homebrew formula
    let formula_path = Path::new("packaging/homebrew/br.rb");
    if formula_path.exists() {
        let formula = fs::read_to_string(formula_path).expect("Failed to read Homebrew formula");
        assert!(
            formula.contains(&format!("version \"{cargo_version}\"")),
            "Homebrew formula version should match Cargo.toml ({cargo_version})"
        );
    }

    // Check Scoop manifest
    let scoop_path = Path::new("packaging/scoop/br.json");
    if scoop_path.exists() {
        let scoop = fs::read_to_string(scoop_path).expect("Failed to read Scoop manifest");
        let scoop_json: serde_json::Value =
            serde_json::from_str(&scoop).expect("Invalid Scoop JSON");
        let scoop_version = scoop_json["version"].as_str().unwrap_or("");
        assert_eq!(
            scoop_version, cargo_version,
            "Scoop manifest version should match Cargo.toml"
        );
    }

    // Check AUR PKGBUILD
    let pkgbuild_path = Path::new("packaging/aur/PKGBUILD");
    if pkgbuild_path.exists() {
        let pkgbuild = fs::read_to_string(pkgbuild_path).expect("Failed to read PKGBUILD");
        assert!(
            pkgbuild.contains(&format!("pkgver={cargo_version}")),
            "PKGBUILD version should match Cargo.toml ({cargo_version})"
        );
    }
}
