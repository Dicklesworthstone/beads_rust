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
//! 1. Extract prefix from issue ID (substring before final `-`, plus hyphen)
//! 2. Search local `.beads/routes.jsonl`
//! 3. Search town root `.beads/routes.jsonl` if different
//! 4. If route found with `path == "."`, use town-level `.beads`
//! 5. Otherwise resolve path relative to town root
//! 6. If `.beads/redirect` exists in target, follow it

use crate::error::{BeadsError, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use tracing::{debug, trace, warn};

const MAX_REDIRECT_BYTES: usize = 4096;
const MAX_REDIRECT_BYTES_U64: u64 = 4096;

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

/// A batch of issue inputs that resolve to the same beads directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedIssueBatch {
    /// The resolved beads directory for this batch.
    pub beads_dir: PathBuf,
    /// Whether this batch targets an external project.
    pub is_external: bool,
    /// The routed project path, if any.
    pub project_path: Option<String>,
    /// The original issue inputs assigned to this route, in input order.
    pub issue_inputs: Vec<String>,
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
/// The prefix is the substring before the final hyphen, plus the hyphen. This
/// matches ID parsing for hyphenated prefixes.
/// For example, "bd-abc123" returns "bd-" and
/// "document-intelligence-0sa" returns "document-intelligence-".
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
/// Returns an empty vector if the file doesn't exist. Blank lines and
/// whole-line `#` comments are ignored for parity with classic beads route
/// files.
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
        if is_ignorable_route_jsonl_line(&line) {
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

/// Return true for route-file lines that carry no route entry.
#[must_use]
pub(crate) fn is_ignorable_route_jsonl_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed.starts_with('#')
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

    let metadata = fs::metadata(&redirect_path)?;
    let content = read_redirect_file_limited(&redirect_path, &metadata)?;
    let target = content.trim();

    if target.is_empty() {
        warn!(path = %redirect_path.display(), "Empty redirect file");
        return Ok(None);
    }

    let target_path = PathBuf::from(target);
    let resolved = if target_path.is_absolute() {
        target_path
    } else {
        // Resolve relative to the .beads directory itself so "." stays within
        // the workspace storage root instead of escaping to the project root.
        beads_dir.join(target_path)
    };

    debug!(
        from = %beads_dir.display(),
        to = %resolved.display(),
        "Following redirect"
    );

    Ok(Some(resolved))
}

fn read_redirect_file_limited(redirect_path: &Path, metadata: &fs::Metadata) -> Result<String> {
    if metadata.len() > MAX_REDIRECT_BYTES_U64 {
        return Err(BeadsError::Config(format!(
            "Redirect file exceeds maximum size of {MAX_REDIRECT_BYTES} bytes: {}",
            redirect_path.display()
        )));
    }

    let file = File::open(redirect_path)?;
    let mut reader = file.take(MAX_REDIRECT_BYTES_U64.saturating_add(1));
    let mut content = Vec::new();
    reader.read_to_end(&mut content)?;
    if content.len() > MAX_REDIRECT_BYTES {
        return Err(BeadsError::Config(format!(
            "Redirect file exceeds maximum size of {MAX_REDIRECT_BYTES} bytes: {}",
            redirect_path.display()
        )));
    }

    String::from_utf8(content).map_err(|e| {
        BeadsError::Config(format!(
            "Redirect file must be valid UTF-8: {}: {e}",
            redirect_path.display()
        ))
    })
}

/// Follow redirects until we reach a terminal beads directory.
///
/// Protects against redirect loops by limiting the chain length.
///
/// # Errors
///
/// Returns an error if a redirect cannot be read or if a redirect loop is detected.
pub fn follow_redirects(start: &Path, max_depth: usize) -> Result<PathBuf> {
    let mut current = canonicalize_redirect_path(start);
    let mut visited = vec![current.clone()];
    let mut depth = 0usize;

    while let Some(next_raw) = read_redirect(&current)? {
        if depth >= max_depth {
            return Err(BeadsError::Config(format!(
                "Redirect chain exceeds max depth ({max_depth}): {}",
                start.display()
            )));
        }

        let next = canonicalize_redirect_path(&next_raw);
        if next == current {
            break;
        }

        // Check for loops
        if visited.iter().any(|p| p == &next) {
            return Err(BeadsError::Config(format!(
                "Redirect loop detected: {} -> {}",
                current.display(),
                next_raw.display()
            )));
        }

        visited.push(next.clone());
        current = next;
        depth += 1;
    }

    // Verify the final directory exists and still points to a real .beads dir.
    if !current.is_dir() {
        return Err(BeadsError::Config(format!(
            "Redirect target not found: {}",
            current.display()
        )));
    }

    if current
        .file_name()
        .is_none_or(|name| !super::is_beads_dir_name(name))
    {
        return Err(BeadsError::Config(format!(
            "Redirect target must be a .beads or _beads directory: {}",
            current.display()
        )));
    }

    Ok(current)
}

fn canonicalize_redirect_path(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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

/// Group issue inputs by their resolved route, preserving first-seen batch order.
///
/// # Errors
///
/// Returns an error if any route file cannot be read or a routed target is invalid.
pub fn group_issue_inputs_by_route(
    issue_inputs: &[String],
    local_beads_dir: &Path,
) -> Result<Vec<RoutedIssueBatch>> {
    let mut batches: Vec<RoutedIssueBatch> = Vec::new();

    for issue_input in issue_inputs {
        let route = resolve_route(issue_input, local_beads_dir)?;
        if let Some(existing) = batches
            .iter_mut()
            .find(|batch| batch.beads_dir == route.beads_dir)
        {
            existing.issue_inputs.push(issue_input.clone());
            continue;
        }

        batches.push(RoutedIssueBatch {
            beads_dir: route.beads_dir,
            is_external: route.is_external,
            project_path: route.project_path,
            issue_inputs: vec![issue_input.clone()],
        });
    }

    Ok(batches)
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

        // Check if it's a .beads/_beads directory or a project root
        if resolved.file_name().is_some_and(super::is_beads_dir_name) {
            resolved
        } else {
            resolved.join(".beads")
        }
    };

    // Follow redirects
    let final_path = follow_redirects(&target_path, 10)?;
    let normalized_final_path =
        dunce::canonicalize(&final_path).unwrap_or_else(|_| final_path.clone());
    let normalized_local_beads_dir =
        dunce::canonicalize(local_beads_dir).unwrap_or_else(|_| local_beads_dir.to_path_buf());

    // Determine if external
    let is_external = normalized_final_path != normalized_local_beads_dir;

    if is_external {
        Ok(RoutingResult::external(
            normalized_final_path,
            route.path.clone(),
        ))
    } else {
        Ok(RoutingResult::local(normalized_final_path))
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
        assert_eq!(
            extract_prefix("no-hyphen-here"),
            Some("no-hyphen-".to_string())
        );
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
    fn load_routes_skips_comments() {
        let dir = TempDir::new().unwrap();
        let routes_path = dir.path().join("routes.jsonl");

        let content = r#"# Local route table
        {"prefix":"bd-","path":"."}
        # Keep this in sync with the town map.
        {"prefix":"fe-","path":"../frontend"}
        "#;
        fs::write(&routes_path, content).unwrap();

        let routes = load_routes(&routes_path).unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].prefix, "bd-");
        assert_eq!(routes[1].prefix, "fe-");
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
    fn read_redirect_dot_stays_in_beads_dir() {
        let dir = TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        fs::write(beads_dir.join("redirect"), ".").unwrap();

        let result = read_redirect(&beads_dir).unwrap().unwrap();
        assert_eq!(result, beads_dir);
    }

    #[test]
    fn read_redirect_rejects_oversized_file() {
        let dir = TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let redirect_path = beads_dir.join("redirect");
        fs::write(&redirect_path, ".").unwrap();
        File::options()
            .write(true)
            .open(&redirect_path)
            .unwrap()
            .set_len(MAX_REDIRECT_BYTES_U64 + 1)
            .unwrap();

        let err = read_redirect(&beads_dir).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(msg) if msg.contains("maximum size")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn read_redirect_file_limited_checks_size_before_utf8_decode() {
        let dir = TempDir::new().unwrap();
        let redirect_path = dir.path().join("redirect");
        fs::write(&redirect_path, ".").unwrap();
        let metadata = fs::metadata(&redirect_path).unwrap();
        let mut payload = vec![b'a'; MAX_REDIRECT_BYTES];
        payload.push(0xc3);
        fs::write(&redirect_path, payload).unwrap();

        let err = read_redirect_file_limited(&redirect_path, &metadata).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(msg) if msg.contains("maximum size")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn read_redirect_rejects_invalid_utf8() {
        let dir = TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join("redirect"), [0xff]).unwrap();

        let err = read_redirect(&beads_dir).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(msg) if msg.contains("valid UTF-8")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn follow_redirects_rejects_non_beads_directory_target() {
        let dir = TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        fs::write(beads_dir.join("redirect"), "..").unwrap();

        let err = follow_redirects(&beads_dir, 10).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let BeadsError::Config(msg) = err else {
            return;
        };
        assert!(
            msg.contains("must be a .beads directory")
                || msg.contains("must be a .beads or _beads directory"),
            "unexpected config error: {msg}"
        );
    }

    #[test]
    fn follow_redirects_rejects_redirect_chains_beyond_max_depth() {
        let dir = TempDir::new().unwrap();
        let first = dir.path().join("first").join(".beads");
        let second = dir.path().join("second").join(".beads");
        let third = dir.path().join("third").join(".beads");

        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::create_dir_all(&third).unwrap();

        fs::write(first.join("redirect"), "../../second/.beads").unwrap();
        fs::write(second.join("redirect"), "../../third/.beads").unwrap();

        let err = follow_redirects(&first, 1).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let BeadsError::Config(msg) = err else {
            return;
        };
        assert!(msg.contains("max depth"));
    }

    #[test]
    fn follow_redirects_allows_dot_redirect_to_current_beads_dir() {
        let dir = TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        fs::write(beads_dir.join("redirect"), ".").unwrap();

        let resolved = follow_redirects(&beads_dir, 10).unwrap();
        assert_eq!(resolved, dunce::canonicalize(&beads_dir).unwrap());
    }

    #[test]
    fn follow_redirects_detects_loops_after_canonicalizing_targets() {
        let dir = TempDir::new().unwrap();
        let first = dir.path().join("first").join(".beads");
        let second = dir.path().join("second").join(".beads");

        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();

        fs::write(first.join("redirect"), "../../second/.beads").unwrap();
        fs::write(second.join("redirect"), "../../first/./.beads").unwrap();

        let err = follow_redirects(&first, 10).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let BeadsError::Config(msg) = err else {
            return;
        };
        assert!(msg.contains("loop detected"));
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
        let result_canonical = dunce::canonicalize(&result.beads_dir).unwrap();
        let target_canonical = dunce::canonicalize(&target_beads).unwrap();
        assert_eq!(result_canonical, target_canonical);
        assert!(result.is_external);
        assert_eq!(result.project_path, Some("../frontend".to_string()));
    }

    #[test]
    fn group_issue_inputs_by_route_preserves_first_seen_batch_order() {
        let dir = TempDir::new().unwrap();
        let local_beads = dir.path().join("current/.beads");
        let external_beads = dir.path().join("external/.beads");
        fs::create_dir_all(&local_beads).unwrap();
        fs::create_dir_all(&external_beads).unwrap();
        fs::write(
            local_beads.join("routes.jsonl"),
            r#"{"prefix":"ext-","path":"../external"}"#,
        )
        .unwrap();

        let batches = group_issue_inputs_by_route(
            &[
                "current-1".to_string(),
                "ext-1".to_string(),
                "current-2".to_string(),
                "ext-2".to_string(),
            ],
            &local_beads,
        )
        .unwrap();

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].beads_dir, local_beads);
        assert_eq!(
            batches[0].issue_inputs,
            vec!["current-1".to_string(), "current-2".to_string()]
        );
        assert!(batches[1].beads_dir.ends_with("external/.beads"));
        assert!(batches[1].is_external);
        assert_eq!(
            batches[1].issue_inputs,
            vec!["ext-1".to_string(), "ext-2".to_string()]
        );
    }

    #[test]
    fn resolve_route_self_path_is_not_external_after_normalization() {
        let dir = TempDir::new().unwrap();
        let local_beads = dir.path().join("current/.beads");
        fs::create_dir_all(&local_beads).unwrap();
        fs::write(
            local_beads.join("routes.jsonl"),
            r#"{"prefix":"self-","path":"../current"}"#,
        )
        .unwrap();

        let result = resolve_route("self-abc", &local_beads).unwrap();
        let local_canonical = dunce::canonicalize(&local_beads).unwrap();

        assert_eq!(result.beads_dir, local_canonical);
        assert!(!result.is_external);
        assert_eq!(result.project_path, None);
    }

    #[test]
    fn group_issue_inputs_by_route_merges_equivalent_external_paths() {
        let dir = TempDir::new().unwrap();
        let local_beads = dir.path().join("current/.beads");
        let external_beads = dir.path().join("external/.beads");
        fs::create_dir_all(&local_beads).unwrap();
        fs::create_dir_all(&external_beads).unwrap();
        fs::write(
            local_beads.join("routes.jsonl"),
            concat!(
                r#"{"prefix":"ext-","path":"../external"}"#,
                "\n",
                r#"{"prefix":"alt-","path":"../current/../external"}"#
            ),
        )
        .unwrap();

        let batches =
            group_issue_inputs_by_route(&["ext-1".to_string(), "alt-2".to_string()], &local_beads)
                .unwrap();

        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0].beads_dir,
            dunce::canonicalize(&external_beads).unwrap()
        );
        assert!(batches[0].is_external);
        assert_eq!(
            batches[0].issue_inputs,
            vec!["ext-1".to_string(), "alt-2".to_string()]
        );
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
