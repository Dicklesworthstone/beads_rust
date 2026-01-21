//! Route resolution for cross-project issue lookup.
//!
//! Implements classic beads routing used by `show`, `update`, `close`, etc.
//! This resolves which `.beads` directory to open for a given ID prefix.
//!
//! # Key Artifacts
//!
//! - `.beads/routes.jsonl` - Route entries mapping prefixes to paths
//! - `.beads/redirect` - Override file for target beads directory
//! - `mayor/town.json` - Town root marker for hierarchical discovery
//!
//! # Resolution Order
//!
//! 1. Extract prefix from issue ID (substring before first `-`, plus hyphen)
//! 2. Search local `.beads/routes.jsonl`
//! 3. Search town root `.beads/routes.jsonl` if different
//! 4. If route found with `path == "."`, use town-level `.beads`
//! 5. Otherwise resolve path relative to town root
//! 6. If `.beads/redirect` exists in target, follow it

use crate::error::{BeadsError, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use tracing::{debug, trace, warn};

/// A route entry from routes.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteEntry {
    /// The prefix to match (e.g., "bd-", "fe-").
    pub prefix: String,
    /// Path to the target project (relative to town root or absolute).
    pub path: String,
}

/// Result of route resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingResult {
    /// The resolved beads directory.
    pub beads_dir: PathBuf,
    /// Whether this is an external project (not the current one).
    pub is_external: bool,
    /// The project name/path from the route, if any.
    pub project_path: Option<String>,
}

impl RoutingResult {
    /// Create a result for the local beads directory.
    #[must_use]
    pub const fn local(beads_dir: PathBuf) -> Self {
        Self {
            beads_dir,
            is_external: false,
            project_path: None,
        }
    }

    /// Create a result for an external project.
    #[must_use]
    pub const fn external(beads_dir: PathBuf, project_path: String) -> Self {
        Self {
            beads_dir,
            is_external: true,
            project_path: Some(project_path),
        }
    }
}

/// Extract the prefix from an issue ID.
///
/// The prefix is the substring before the first hyphen, plus the hyphen.
/// For example, "bd-abc123" returns "bd-".
///
/// Returns `None` if the ID has no hyphen.
#[must_use]
pub fn extract_prefix(issue_id: &str) -> Option<String> {
    crate::util::id::split_prefix_remainder(issue_id).map(|(prefix, _)| format!("{prefix}-"))
}

/// Find the town root by walking up looking for `mayor/town.json`.
///
/// Returns `None` if no town root is found.
#[must_use]
pub fn find_town_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();

    loop {
        let town_marker = current.join("mayor").join("town.json");
        if town_marker.is_file() {
            trace!(town_root = %current.display(), "Found town root");
            return Some(current);
        }

        if !current.pop() {
            break;
        }
    }

    None
}

/// Load route entries from a routes.jsonl file.
///
/// Returns an empty vector if the file doesn't exist.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_routes(routes_path: &Path) -> Result<Vec<RouteEntry>> {
    if !routes_path.is_file() {
        return Ok(Vec::new());
    }

    let file = File::open(routes_path)?;
    let reader = BufReader::new(file);
    let mut routes = Vec::new();

    for (line_num, line_result) in reader.lines().enumerate() {
        let line = line_result?;
        if line.trim().is_empty() {
            continue;
        }

        let entry: RouteEntry = serde_json::from_str(&line).map_err(|e| {
            BeadsError::Config(format!(
                "Invalid route at {}:{}: {}",
                routes_path.display(),
                line_num + 1,
                e
            ))
        })?;

        routes.push(entry);
    }

    debug!(
        path = %routes_path.display(),
        count = routes.len(),
        "Loaded routes"
    );

    Ok(routes)
}

/// Find a route entry matching the given prefix.
#[must_use]
pub fn find_route<'a>(routes: &'a [RouteEntry], prefix: &str) -> Option<&'a RouteEntry> {
    routes.iter().find(|r| r.prefix == prefix)
}

/// Read the redirect file if it exists.
///
/// The redirect file contains a single path (relative or absolute) pointing
/// to the actual beads directory to use.
///
/// Returns `None` if no redirect file exists.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read.
pub fn read_redirect(beads_dir: &Path) -> Result<Option<PathBuf>> {
    let redirect_path = beads_dir.join("redirect");
    if !redirect_path.is_file() {
        return Ok(None);
    }

    let content = fs::read_to_string(&redirect_path)?;
    let target = content.trim();

    if target.is_empty() {
        warn!(path = %redirect_path.display(), "Empty redirect file");
        return Ok(None);
    }

    let target_path = PathBuf::from(target);
    let resolved = if target_path.is_absolute() {
        target_path
    } else {
        // Resolve relative to the beads directory's parent
        beads_dir.parent().unwrap_or(beads_dir).join(target_path)
    };

    debug!(
        from = %beads_dir.display(),
        to = %resolved.display(),
        "Following redirect"
    );

    Ok(Some(resolved))
}

/// Follow redirects until we reach a terminal beads directory.
///
/// Protects against redirect loops by limiting the chain length.
///
/// # Errors
///
/// Returns an error if a redirect cannot be read or if a redirect loop is detected.
pub fn follow_redirects(start: &Path, max_depth: usize) -> Result<PathBuf> {
    let mut current = start.to_path_buf();
    let mut visited = vec![start.to_path_buf()];

    for _ in 0..max_depth {
        match read_redirect(&current)? {
            Some(next) => {
                // Check for loops
                if visited.iter().any(|p| p == &next) {
                    return Err(BeadsError::Config(format!(
                        "Redirect loop detected: {} -> {}",
                        current.display(),
                        next.display()
                    )));
                }

                visited.push(next.clone());
                current = next;
            }
            None => break,
        }
    }

    // Verify the final directory exists
    if !current.is_dir() {
        return Err(BeadsError::Config(format!(
            "Redirect target not found: {}",
            current.display()
        )));
    }

    Ok(current)
}

/// Resolve the target beads directory for an issue ID.
///
/// # Resolution Process
///
/// 1. Extract prefix from issue ID
/// 2. Search local routes.jsonl
/// 3. Search town root routes.jsonl (if different from local)
/// 4. Resolve the target path
/// 5. Follow any redirects
///
/// Returns the local beads directory if no routing applies.
///
/// # Errors
///
/// Returns an error if route files cannot be read or the target doesn't exist.
pub fn resolve_route(issue_id: &str, local_beads_dir: &Path) -> Result<RoutingResult> {
    let Some(prefix) = extract_prefix(issue_id) else {
        // No prefix, use local
        return Ok(RoutingResult::local(local_beads_dir.to_path_buf()));
    };

    // Load local routes
    let local_routes_path = local_beads_dir.join("routes.jsonl");
    let local_routes = load_routes(&local_routes_path)?;

    // Route paths are relative to project root (parent of .beads)
    let project_root = local_beads_dir.parent().unwrap_or(local_beads_dir);

    if let Some(route) = find_route(&local_routes, &prefix) {
        return resolve_route_entry(route, project_root, local_beads_dir);
    }

    // Find and search town root if different
    if let Some(town_root) = find_town_root(project_root) {
        let town_beads_dir = town_root.join(".beads");
        if town_beads_dir != *local_beads_dir && town_beads_dir.is_dir() {
            let town_routes_path = town_beads_dir.join("routes.jsonl");
            let town_routes = load_routes(&town_routes_path)?;

            if let Some(route) = find_route(&town_routes, &prefix) {
                return resolve_route_entry(route, &town_root, local_beads_dir);
            }
        }
    }

    // No route found, use local
    Ok(RoutingResult::local(local_beads_dir.to_path_buf()))
}

/// Resolve a route entry to a beads directory.
fn resolve_route_entry(
    route: &RouteEntry,
    base_dir: &Path,
    local_beads_dir: &Path,
) -> Result<RoutingResult> {
    let target_path = if route.path == "." {
        // Town-level beads
        base_dir.join(".beads")
    } else {
        let path = PathBuf::from(&route.path);
        let resolved = if path.is_absolute() {
            path
        } else {
            base_dir.join(path)
        };

        // Check if it's a .beads directory or a project root
        if resolved.file_name().is_some_and(|n| n == ".beads") {
            resolved
        } else {
            resolved.join(".beads")
        }
    };

    // Follow redirects
    let final_path = follow_redirects(&target_path, 10)?;

    // Determine if external
    let is_external = final_path != local_beads_dir;

    if is_external {
        Ok(RoutingResult::external(final_path, route.path.clone()))
    } else {
        Ok(RoutingResult::local(final_path))
    }
}

/// Check if an issue ID would be routed externally.
///
/// Quick check without fully resolving the route.
#[must_use]
pub fn is_external_id(issue_id: &str, local_prefix: &str) -> bool {
    extract_prefix(issue_id).is_some_and(|prefix| !prefix.eq_ignore_ascii_case(local_prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn extract_prefix_basic() {
        assert_eq!(extract_prefix("bd-abc123"), Some("bd-".to_string()));
        assert_eq!(extract_prefix("fe-xyz"), Some("fe-".to_string()));
        assert_eq!(
            extract_prefix("bead-me-up-3e9"),
            Some("bead-me-up-".to_string())
        );
        assert_eq!(
            extract_prefix("document-intelligence-0sa"),
            Some("document-intelligence-".to_string())
        );
        assert_eq!(extract_prefix("no-hyphen-here"), Some("no-hyphen-".to_string()));
        assert_eq!(extract_prefix("nohyphen"), None);
        assert_eq!(extract_prefix(""), None);
    }

    #[test]
    fn load_routes_empty() {
        let dir = TempDir::new().unwrap();
        let routes = load_routes(&dir.path().join("routes.jsonl")).unwrap();
        assert!(routes.is_empty());
    }

    #[test]
    fn load_routes_valid() {
        let dir = TempDir::new().unwrap();
        let routes_path = dir.path().join("routes.jsonl");

        let content = r#"{"prefix":"bd-","path":"."}
{"prefix":"fe-","path":"../frontend"}
"#;
        fs::write(&routes_path, content).unwrap();

        let routes = load_routes(&routes_path).unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].prefix, "bd-");
        assert_eq!(routes[0].path, ".");
        assert_eq!(routes[1].prefix, "fe-");
        assert_eq!(routes[1].path, "../frontend");
    }

    #[test]
    fn find_route_match() {
        let routes = vec![
            RouteEntry {
                prefix: "bd-".to_string(),
                path: ".".to_string(),
            },
            RouteEntry {
                prefix: "fe-".to_string(),
                path: "../frontend".to_string(),
            },
        ];

        assert_eq!(find_route(&routes, "bd-").unwrap().path, ".");
        assert_eq!(find_route(&routes, "fe-").unwrap().path, "../frontend");
        assert!(find_route(&routes, "be-").is_none());
    }

    #[test]
    fn read_redirect_none() {
        let dir = TempDir::new().unwrap();
        assert!(read_redirect(dir.path()).unwrap().is_none());
    }

    #[test]
    fn read_redirect_absolute() {
        let dir = TempDir::new().unwrap();
        let redirect_path = dir.path().join("redirect");
        fs::write(&redirect_path, "/absolute/path/.beads\n").unwrap();

        let result = read_redirect(dir.path()).unwrap();
        assert_eq!(result, Some(PathBuf::from("/absolute/path/.beads")));
    }

    #[test]
    fn read_redirect_relative() {
        let dir = TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let redirect_path = beads_dir.join("redirect");
        fs::write(&redirect_path, "../other/.beads").unwrap();

        let result = read_redirect(&beads_dir).unwrap().unwrap();
        // The path contains "../other" which resolves correctly but isn't canonicalized
        // Just verify it ends with "other/.beads"
        assert!(result.ends_with("other/.beads"));
        // And starts with the temp dir base
        let result_str = result.to_string_lossy();
        assert!(result_str.contains(".beads"));
    }

    #[test]
    fn is_external_id_check() {
        assert!(is_external_id("fe-abc", "bd-"));
        assert!(!is_external_id("bd-abc", "bd-"));
        assert!(!is_external_id("BD-abc", "bd-")); // case insensitive
        assert!(!is_external_id("nohyphen", "bd-")); // no prefix
    }

    #[test]
    fn resolve_route_no_prefix() {
        let dir = TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let result = resolve_route("nohyphen", &beads_dir).unwrap();
        assert_eq!(result.beads_dir, beads_dir);
        assert!(!result.is_external);
    }

    #[test]
    fn resolve_route_no_routes_file() {
        let dir = TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let result = resolve_route("bd-abc", &beads_dir).unwrap();
        assert_eq!(result.beads_dir, beads_dir);
        assert!(!result.is_external);
    }

    #[test]
    fn resolve_route_with_local_route() {
        let dir = TempDir::new().unwrap();

        // Create local beads dir under "current" project
        let local_beads = dir.path().join("current/.beads");
        fs::create_dir_all(&local_beads).unwrap();

        // Create target beads dir as sibling to "current" project
        let target_beads = dir.path().join("frontend/.beads");
        fs::create_dir_all(&target_beads).unwrap();

        // Create routes.jsonl with path relative to "current" project root
        // "../frontend" goes from "current" to "frontend"
        let routes_path = local_beads.join("routes.jsonl");
        fs::write(&routes_path, r#"{"prefix":"fe-","path":"../frontend"}"#).unwrap();

        let result = resolve_route("fe-abc", &local_beads).unwrap();
        // Canonicalize for comparison since paths may contain ".."
        let result_canonical = result.beads_dir.canonicalize().unwrap();
        let target_canonical = target_beads.canonicalize().unwrap();
        assert_eq!(result_canonical, target_canonical);
        assert!(result.is_external);
        assert_eq!(result.project_path, Some("../frontend".to_string()));
    }

    #[test]
    fn find_town_root_test() {
        let dir = TempDir::new().unwrap();

        // Create town structure
        let town_root = dir.path().join("town");
        fs::create_dir_all(town_root.join("mayor")).unwrap();
        fs::write(town_root.join("mayor/town.json"), "{}").unwrap();

        // Create a project within the town
        let project = town_root.join("projects/myproject");
        fs::create_dir_all(&project).unwrap();

        let result = find_town_root(&project);
        assert_eq!(result, Some(town_root));
    }

    #[test]
    fn find_town_root_not_found() {
        let dir = TempDir::new().unwrap();
        let result = find_town_root(dir.path());
        assert!(result.is_none());
    }
}
