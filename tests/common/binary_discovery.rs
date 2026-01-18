//! Binary discovery and version pinning for conformance testing.
//!
//! Ensures conformance runs use the correct br/bd binaries and records version metadata.
//! Fails early with actionable errors if bd is missing or unsupported.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Minimum bd version required for conformance testing.
const MIN_BD_VERSION: &str = "0.5.0";

/// Binary version metadata captured from `--version --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryVersion {
    pub binary: String,
    pub path: PathBuf,
    pub version: String,
    pub commit: Option<String>,
    pub build_date: Option<String>,
    #[serde(default)]
    pub raw_output: String,
}

impl BinaryVersion {
    /// Serialize to JSON for inclusion in conformance logs.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "binary": self.binary,
            "path": self.path.display().to_string(),
            "version": self.version,
            "commit": self.commit,
            "build_date": self.build_date,
        })
    }
}

/// Result of binary discovery.
#[derive(Debug, Clone)]
pub struct DiscoveredBinaries {
    pub br: BinaryVersion,
    pub bd: Option<BinaryVersion>,
}

impl DiscoveredBinaries {
    /// Check if bd is available for conformance testing.
    pub const fn bd_available(&self) -> bool {
        self.bd.is_some()
    }

    /// Get bd or return an error message.
    pub fn require_bd(&self) -> Result<&BinaryVersion, String> {
        self.bd.as_ref().ok_or_else(|| {
            "bd (Go beads) binary not found. Conformance tests require bd to be installed.\n\
             Install from: https://github.com/steveyegge/beads\n\
             Or set BD_BINARY env var to the path."
                .to_string()
        })
    }

    /// Serialize for inclusion in conformance summary.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "br": self.br.to_json(),
            "bd": self.bd.as_ref().map(BinaryVersion::to_json),
            "conformance_ready": self.bd_available(),
        })
    }
}

/// Discover br binary (from cargo build).
fn discover_br() -> Result<BinaryVersion, String> {
    // First check if BR_BINARY env var is set
    if let Ok(br_path) = std::env::var("BR_BINARY") {
        let path = PathBuf::from(&br_path);
        if path.exists() {
            return probe_binary("br", &path);
        }
        return Err(format!("BR_BINARY={br_path} does not exist"));
    }

    // Try cargo-built binary
    let cargo_bin = assert_cmd::cargo::cargo_bin!("br");
    if cargo_bin.exists() {
        return probe_binary("br", cargo_bin);
    }

    // Try release binary
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let release_bin = manifest_dir.join("target/release/br");
    if release_bin.exists() {
        return probe_binary("br", &release_bin);
    }

    // Try PATH
    if let Some(path) = which("br") {
        return probe_binary("br", &path);
    }

    Err("br binary not found. Build with `cargo build` first.".to_string())
}

/// Discover bd binary (Go beads).
fn discover_bd() -> Option<BinaryVersion> {
    // First check if BD_BINARY env var is set
    if let Ok(bd_path) = std::env::var("BD_BINARY") {
        let path = PathBuf::from(&bd_path);
        if path.exists() {
            return probe_binary("bd", &path).ok();
        }
        eprintln!("Warning: BD_BINARY={bd_path} does not exist");
        return None;
    }

    // Get home directory
    let home = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_default();

    // Try common locations
    let mut common_paths = vec![
        PathBuf::from("/usr/local/bin/bd"),
        PathBuf::from("/usr/bin/bd"),
    ];

    if !home.as_os_str().is_empty() {
        common_paths.push(home.join(".local/bin/bd"));
        common_paths.push(home.join("go/bin/bd"));
    }

    for path in common_paths {
        if path.exists() {
            if let Ok(version) = probe_binary("bd", &path) {
                return Some(version);
            }
        }
    }

    // Try PATH
    if let Some(path) = which("bd") {
        return probe_binary("bd", &path).ok();
    }

    None
}

/// Probe a binary to extract version information.
fn probe_binary(name: &str, path: &Path) -> Result<BinaryVersion, String> {
    // Try `--version --json` first
    let json_output = run_version_command(path, &["version", "--json"]);
    if let Some(output) = json_output {
        if let Ok(parsed) = parse_json_version(&output) {
            return Ok(BinaryVersion {
                binary: name.to_string(),
                path: path.to_path_buf(),
                version: parsed.version,
                commit: parsed.commit,
                build_date: parsed.build_date,
                raw_output: output,
            });
        }
    }

    // Fallback to plain `--version`
    let plain_output = run_version_command(path, &["--version"]);
    if let Some(output) = plain_output {
        let version = parse_plain_version(&output);
        return Ok(BinaryVersion {
            binary: name.to_string(),
            path: path.to_path_buf(),
            version,
            commit: None,
            build_date: None,
            raw_output: output,
        });
    }

    // Last resort: just verify it runs
    let check_output = run_version_command(path, &["--help"]);
    if check_output.is_some() {
        return Ok(BinaryVersion {
            binary: name.to_string(),
            path: path.to_path_buf(),
            version: "unknown".to_string(),
            commit: None,
            build_date: None,
            raw_output: check_output.unwrap_or_default(),
        });
    }

    Err(format!(
        "Binary at {} does not respond to version commands",
        path.display()
    ))
}

/// Run a version command and capture output.
fn run_version_command(binary: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(binary)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

/// Parsed JSON version response.
#[derive(Debug, Deserialize)]
struct JsonVersion {
    version: String,
    commit: Option<String>,
    build_date: Option<String>,
}

/// Parse JSON version output.
fn parse_json_version(output: &str) -> Result<JsonVersion, serde_json::Error> {
    // Handle potential prefix text before JSON
    let json_start = output.find('{').unwrap_or(0);
    serde_json::from_str(&output[json_start..])
}

/// Parse plain text version output (e.g., "br 0.1.0").
fn parse_plain_version(output: &str) -> String {
    let output = output.trim();

    // Try to extract version number
    for word in output.split_whitespace() {
        if word
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            // Include digits, dots, hyphens, and alphanumeric suffixes (e.g., "0.1.0-dev")
            let version: String = word
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
                .collect();
            if !version.is_empty() {
                return version;
            }
        }
    }

    "unknown".to_string()
}

/// Find binary in PATH.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let path = dir.join(name);
            if path.exists() && path.is_file() {
                Some(path)
            } else {
                None
            }
        })
    })
}

/// Discover both br and bd binaries.
///
/// Returns error only if br is not found (bd is optional for non-conformance tests).
pub fn discover_binaries() -> Result<DiscoveredBinaries, String> {
    let br = discover_br()?;
    let bd = discover_bd();

    Ok(DiscoveredBinaries { br, bd })
}

/// Check if bd version meets minimum requirements for conformance.
pub fn check_bd_version(version: &BinaryVersion) -> Result<(), String> {
    let current = &version.version;

    // Skip check for development/unknown versions
    if current == "unknown" || current.contains("dev") {
        return Ok(());
    }

    // Simple version comparison (works for semver)
    if compare_versions(current, MIN_BD_VERSION).is_lt() {
        return Err(format!(
            "bd version {} is below minimum required version {}. Please upgrade bd.",
            current, MIN_BD_VERSION
        ));
    }

    Ok(())
}

/// Simple semver-style version comparison.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u32> {
        s.split(|c: char| !c.is_ascii_digit())
            .filter_map(|p| p.parse().ok())
            .collect()
    };

    let av = parse(a);
    let bv = parse(b);

    av.cmp(&bv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_br() {
        let result = discover_br();
        assert!(result.is_ok(), "br should be discoverable: {:?}", result);

        let version = result.unwrap();
        assert_eq!(version.binary, "br");
        assert!(version.path.exists());
    }

    #[test]
    fn test_discover_binaries() {
        let result = discover_binaries();
        assert!(result.is_ok(), "Binary discovery failed: {:?}", result);

        let binaries = result.unwrap();
        assert_eq!(binaries.br.binary, "br");

        // bd may or may not be available
        if binaries.bd_available() {
            let bd = binaries.bd.as_ref().unwrap();
            assert_eq!(bd.binary, "bd");
        }
    }

    #[test]
    fn test_parse_plain_version() {
        assert_eq!(parse_plain_version("br 0.1.0"), "0.1.0");
        assert_eq!(parse_plain_version("beads 0.5.2"), "0.5.2");
        assert_eq!(parse_plain_version("0.1.0-dev"), "0.1.0-dev");
        assert_eq!(parse_plain_version("no version"), "unknown");
    }

    #[test]
    fn test_compare_versions() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("0.1.0", "0.1.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.2.0", "0.1.0"), Ordering::Greater);
        assert_eq!(compare_versions("0.1.0", "0.2.0"), Ordering::Less);
        assert_eq!(compare_versions("1.0.0", "0.5.0"), Ordering::Greater);
    }

    #[test]
    fn test_discovered_binaries_json() {
        let binaries = discover_binaries().expect("discovery failed");
        let json = binaries.to_json();

        assert!(json.get("br").is_some());
        assert!(json.get("conformance_ready").is_some());
    }
}
