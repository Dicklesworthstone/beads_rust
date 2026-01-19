//! JSON baseline fixture loader for backward compatibility testing.
//!
//! This module provides helpers to load JSON baseline fixtures that capture
//! the expected JSON output from br commands. These baselines ensure that
//! JSON output remains byte-identical after rich output integration.
//!
//! # Usage
//!
//! ```ignore
//! use crate::common::json_baseline::{load_baseline, load_baseline_raw};
//!
//! // Load as parsed JSON Value
//! let list_baseline = load_baseline("list");
//!
//! // Load as raw string for byte-level comparison
//! let raw = load_baseline_raw("list");
//! ```
//!
//! # Fixtures
//!
//! Fixtures are stored in `tests/fixtures/json_baseline/` and generated
//! by running `scripts/generate_json_baseline.sh`.

use serde_json::Value;
use std::path::PathBuf;

/// Base path for JSON baseline fixtures relative to project root.
const FIXTURE_DIR: &str = "tests/fixtures/json_baseline";

/// Get the path to a baseline fixture file.
pub fn baseline_path(name: &str) -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .join(FIXTURE_DIR)
        .join(format!("{name}.json"))
}

/// Load a baseline fixture as a parsed JSON Value.
///
/// # Panics
///
/// Panics if the fixture file doesn't exist or contains invalid JSON.
pub fn load_baseline(name: &str) -> Value {
    let path = baseline_path(name);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Baseline fixture not found: {}: {e}", path.display()));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("Invalid JSON in baseline {}: {e}", path.display()))
}

/// Load a baseline fixture as raw string content.
///
/// Useful for byte-level comparison of JSON output.
///
/// # Panics
///
/// Panics if the fixture file doesn't exist.
pub fn load_baseline_raw(name: &str) -> String {
    let path = baseline_path(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Baseline fixture not found: {}: {e}", path.display()))
}

/// Check if a baseline fixture exists.
pub fn baseline_exists(name: &str) -> bool {
    baseline_path(name).exists()
}

/// List all available baseline fixture names.
pub fn list_baselines() -> Vec<String> {
    let path = baseline_path("").parent().unwrap().to_path_buf();
    if !path.exists() {
        return vec![];
    }

    std::fs::read_dir(path)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()?.to_str()? == "json" {
                path.file_stem()?.to_str().map(String::from)
            } else {
                None
            }
        })
        .collect()
}

/// Compare JSON output against baseline, returning differences if any.
///
/// Returns `None` if outputs match, `Some(diff_description)` if they differ.
pub fn compare_json_output(name: &str, actual: &Value) -> Option<String> {
    let expected = load_baseline(name);
    if &expected == actual {
        None
    } else {
        Some(format!(
            "JSON output differs from baseline '{name}':\n\
             Expected: {}\n\
             Actual: {}",
            serde_json::to_string_pretty(&expected).unwrap(),
            serde_json::to_string_pretty(actual).unwrap()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_baseline_path() {
        let path = baseline_path("list");
        assert!(path.to_string_lossy().contains("json_baseline"));
        assert!(path.to_string_lossy().ends_with("list.json"));
    }

    #[test]
    fn test_list_baselines() {
        let baselines = list_baselines();
        // After running generate_json_baseline.sh, we should have fixtures
        if !baselines.is_empty() {
            assert!(baselines.contains(&"list".to_string()));
        }
    }

    #[test]
    fn test_baseline_exists() {
        // This test will pass after fixtures are generated
        if baseline_exists("list") {
            let value = load_baseline("list");
            assert!(value.is_array());
        }
    }
}
