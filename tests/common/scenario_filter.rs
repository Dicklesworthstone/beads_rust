//! Scenario filtering for tag-based test selection (beads_rust-o1az).
//!
//! Supports include/exclude patterns, environment variable configuration,
//! and detailed logging of selection criteria in summary.json.

use super::scenarios::{ExecutionMode, Scenario};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Filter for selecting scenarios by tags.
///
/// Supports include/exclude patterns and environment variable configuration.
/// Logs selection criteria in summary.json for auditability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScenarioFilter {
    /// Only run scenarios that have at least one of these tags (empty = all)
    pub include_tags: HashSet<String>,
    /// Skip scenarios that have any of these tags
    pub exclude_tags: HashSet<String>,
    /// Skip scenarios tagged "slow" unless explicitly included
    pub skip_slow: bool,
    /// Include only scenarios supporting this execution mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_mode: Option<ExecutionMode>,
}

impl ScenarioFilter {
    /// Create a filter from environment variables.
    ///
    /// Environment variables:
    /// - `SCENARIO_TAGS`: Comma-separated tags to include (e.g., "quick,crud")
    /// - `SCENARIO_EXCLUDE_TAGS`: Comma-separated tags to exclude (e.g., "slow,stress")
    /// - `SCENARIO_SKIP_SLOW`: Set to "1" to skip slow scenarios
    /// - `SCENARIO_MODE`: Required mode ("e2e", "conformance", or "benchmark")
    pub fn from_env() -> Self {
        let include_tags = std::env::var("SCENARIO_TAGS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let exclude_tags = std::env::var("SCENARIO_EXCLUDE_TAGS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let skip_slow = std::env::var("SCENARIO_SKIP_SLOW")
            .map_or(false, |v| v == "1" || v.to_lowercase() == "true");

        let required_mode = std::env::var("SCENARIO_MODE").ok().and_then(|s| {
            match s.to_lowercase().as_str() {
                "e2e" => Some(ExecutionMode::E2E),
                "conformance" => Some(ExecutionMode::Conformance),
                "benchmark" => Some(ExecutionMode::Benchmark),
                _ => None,
            }
        });

        Self {
            include_tags,
            exclude_tags,
            skip_slow,
            required_mode,
        }
    }

    /// Create a filter that accepts all scenarios.
    pub fn all() -> Self {
        Self::default()
    }

    /// Create a filter for quick tests only.
    pub fn quick() -> Self {
        Self {
            include_tags: ["quick"].into_iter().map(String::from).collect(),
            exclude_tags: ["slow", "stress"].into_iter().map(String::from).collect(),
            skip_slow: true,
            required_mode: None,
        }
    }

    /// Create a filter for CI (no slow, no stress).
    pub fn ci() -> Self {
        Self {
            include_tags: HashSet::new(),
            exclude_tags: ["slow", "stress", "benchmark"]
                .into_iter()
                .map(String::from)
                .collect(),
            skip_slow: true,
            required_mode: None,
        }
    }

    /// Builder: add include tags.
    pub fn with_include_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.include_tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: add exclude tags.
    pub fn with_exclude_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exclude_tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: set skip_slow.
    pub fn with_skip_slow(mut self, skip: bool) -> Self {
        self.skip_slow = skip;
        self
    }

    /// Builder: set required mode.
    pub fn with_required_mode(mut self, mode: ExecutionMode) -> Self {
        self.required_mode = Some(mode);
        self
    }

    /// Check if a scenario passes this filter.
    pub fn matches(&self, scenario: &Scenario) -> bool {
        // Check required mode
        if let Some(mode) = self.required_mode {
            if !scenario.supports_mode(mode) {
                return false;
            }
        }

        // Check skip_slow
        if self.skip_slow && scenario.has_tag("slow") {
            return false;
        }

        // Check exclude tags (any match = excluded)
        for tag in &self.exclude_tags {
            if scenario.has_tag(tag) {
                return false;
            }
        }

        // Check include tags (if specified, at least one must match)
        if !self.include_tags.is_empty() {
            let has_included_tag = self.include_tags.iter().any(|t| scenario.has_tag(t));
            if !has_included_tag {
                return false;
            }
        }

        true
    }

    /// Filter a collection of scenarios, returning only matching ones.
    pub fn filter<'a>(&self, scenarios: &'a [Scenario]) -> Vec<&'a Scenario> {
        scenarios.iter().filter(|s| self.matches(s)).collect()
    }

    /// Filter and return owned scenarios.
    pub fn filter_owned(&self, scenarios: Vec<Scenario>) -> Vec<Scenario> {
        scenarios.into_iter().filter(|s| self.matches(s)).collect()
    }

    /// Serialize filter settings for logging in summary.json.
    pub fn to_json(&self) -> serde_json::Value {
        let mut include: Vec<_> = self.include_tags.iter().cloned().collect();
        include.sort();
        let mut exclude: Vec<_> = self.exclude_tags.iter().cloned().collect();
        exclude.sort();

        serde_json::json!({
            "include_tags": include,
            "exclude_tags": exclude,
            "skip_slow": self.skip_slow,
            "required_mode": self.required_mode.map(|m| format!("{:?}", m)),
        })
    }

    /// Create a human-readable description of the filter.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();

        if !self.include_tags.is_empty() {
            let mut tags: Vec<_> = self.include_tags.iter().cloned().collect();
            tags.sort();
            parts.push(format!("include=[{}]", tags.join(",")));
        }

        if !self.exclude_tags.is_empty() {
            let mut tags: Vec<_> = self.exclude_tags.iter().cloned().collect();
            tags.sort();
            parts.push(format!("exclude=[{}]", tags.join(",")));
        }

        if self.skip_slow {
            parts.push("skip_slow".to_string());
        }

        if let Some(mode) = self.required_mode {
            parts.push(format!("mode={:?}", mode));
        }

        if parts.is_empty() {
            "all scenarios".to_string()
        } else {
            parts.join(", ")
        }
    }

    /// Filter scenarios with detailed result logging.
    pub fn filter_with_log(&self, scenarios: &[Scenario]) -> FilterResult {
        let mut matched_names = Vec::new();
        let mut excluded_names = Vec::new();

        for scenario in scenarios {
            if self.matches(scenario) {
                matched_names.push(scenario.name.clone());
            } else {
                let reason = self.exclusion_reason(scenario);
                excluded_names.push((scenario.name.clone(), reason));
            }
        }

        FilterResult {
            total_count: scenarios.len(),
            matched_count: matched_names.len(),
            excluded_count: excluded_names.len(),
            matched_names,
            excluded_names,
            filter_settings: self.to_json(),
        }
    }

    /// Get the reason a scenario was excluded.
    fn exclusion_reason(&self, scenario: &Scenario) -> String {
        if let Some(mode) = self.required_mode {
            if !scenario.supports_mode(mode) {
                return format!("does not support mode {:?}", mode);
            }
        }

        if self.skip_slow && scenario.has_tag("slow") {
            return "tagged 'slow' and skip_slow=true".to_string();
        }

        for tag in &self.exclude_tags {
            if scenario.has_tag(tag) {
                return format!("has excluded tag '{}'", tag);
            }
        }

        if !self.include_tags.is_empty() {
            return "does not have any included tags".to_string();
        }

        "unknown".to_string()
    }
}

/// Result of filtering scenarios with detailed logging.
#[derive(Debug, Clone, Serialize)]
pub struct FilterResult {
    /// Total scenarios before filtering
    pub total_count: usize,
    /// Scenarios that passed the filter
    pub matched_count: usize,
    /// Scenarios that were excluded
    pub excluded_count: usize,
    /// Names of matched scenarios
    pub matched_names: Vec<String>,
    /// Names of excluded scenarios (with reason)
    pub excluded_names: Vec<(String, String)>,
    /// Filter settings used
    pub filter_settings: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::scenarios::{ScenarioCommand, Scenario};

    fn make_scenario(name: &str, tags: &[&str]) -> Scenario {
        Scenario::new(name, ScenarioCommand::new(["list"]))
            .with_tags(tags.iter().map(|s| s.to_string()))
    }

    #[test]
    fn test_filter_all() {
        let filter = ScenarioFilter::all();
        let scenario = make_scenario("test", &["quick"]);
        assert!(filter.matches(&scenario));
    }

    #[test]
    fn test_filter_include_tags() {
        let filter = ScenarioFilter::all().with_include_tags(["quick"]);

        let quick = make_scenario("quick_test", &["quick"]);
        let slow = make_scenario("slow_test", &["slow"]);

        assert!(filter.matches(&quick));
        assert!(!filter.matches(&slow));
    }

    #[test]
    fn test_filter_exclude_tags() {
        let filter = ScenarioFilter::all().with_exclude_tags(["slow", "stress"]);

        let quick = make_scenario("quick_test", &["quick"]);
        let slow = make_scenario("slow_test", &["slow"]);
        let stress = make_scenario("stress_test", &["stress"]);

        assert!(filter.matches(&quick));
        assert!(!filter.matches(&slow));
        assert!(!filter.matches(&stress));
    }

    #[test]
    fn test_filter_skip_slow() {
        let filter = ScenarioFilter::all().with_skip_slow(true);

        let quick = make_scenario("quick_test", &["quick"]);
        let slow = make_scenario("slow_test", &["slow"]);

        assert!(filter.matches(&quick));
        assert!(!filter.matches(&slow));
    }

    #[test]
    fn test_filter_ci_preset() {
        let filter = ScenarioFilter::ci();

        let quick = make_scenario("quick_test", &["quick"]);
        let slow = make_scenario("slow_test", &["slow"]);
        let stress = make_scenario("stress_test", &["stress"]);
        let benchmark = make_scenario("bench_test", &["benchmark"]);

        assert!(filter.matches(&quick));
        assert!(!filter.matches(&slow));
        assert!(!filter.matches(&stress));
        assert!(!filter.matches(&benchmark));
    }

    #[test]
    fn test_filter_quick_preset() {
        let filter = ScenarioFilter::quick();

        let quick = make_scenario("quick_test", &["quick"]);
        let slow = make_scenario("slow_test", &["slow"]);
        let other = make_scenario("other_test", &["crud"]);

        assert!(filter.matches(&quick));
        assert!(!filter.matches(&slow));
        assert!(!filter.matches(&other)); // No "quick" tag
    }

    #[test]
    fn test_filter_to_json() {
        let filter = ScenarioFilter::all()
            .with_include_tags(["quick", "crud"])
            .with_exclude_tags(["slow"])
            .with_skip_slow(true);

        let json = filter.to_json();

        assert!(json["skip_slow"].as_bool().unwrap());
        let include = json["include_tags"].as_array().unwrap();
        assert!(include.contains(&serde_json::json!("quick")));
        assert!(include.contains(&serde_json::json!("crud")));
    }

    #[test]
    fn test_filter_describe() {
        let filter = ScenarioFilter::all()
            .with_include_tags(["quick"])
            .with_skip_slow(true);

        let desc = filter.describe();
        assert!(desc.contains("include="));
        assert!(desc.contains("skip_slow"));
    }

    #[test]
    fn test_filter_with_log() {
        let filter = ScenarioFilter::all().with_exclude_tags(["slow"]);

        let scenarios = vec![
            make_scenario("quick_test", &["quick"]),
            make_scenario("slow_test", &["slow"]),
        ];

        let result = filter.filter_with_log(&scenarios);

        assert_eq!(result.total_count, 2);
        assert_eq!(result.matched_count, 1);
        assert_eq!(result.excluded_count, 1);
        assert!(result.matched_names.contains(&"quick_test".to_string()));
        assert!(result.excluded_names.iter().any(|(n, _)| n == "slow_test"));
    }

    #[test]
    fn test_filter_required_mode() {
        let filter = ScenarioFilter::all().with_required_mode(ExecutionMode::Benchmark);

        let e2e_only = Scenario::new("e2e_test", ScenarioCommand::new(["list"]))
            .with_tags(["quick"])
            .with_modes(vec![ExecutionMode::E2E]);

        let all_modes = Scenario::new("all_test", ScenarioCommand::new(["list"]))
            .with_tags(["quick"])
            .with_modes(vec![ExecutionMode::E2E, ExecutionMode::Benchmark]);

        assert!(!filter.matches(&e2e_only));
        assert!(filter.matches(&all_modes));
    }
}
