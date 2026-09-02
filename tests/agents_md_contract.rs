//! AGENTS.md is the first thing an agent reads, so its map of the codebase
//! must match the tree. The 2026-09-01 reality check found it listing a
//! `storage/queries/` directory that does not exist, omitting eleven
//! top-level modules, and naming a lint level the crate does not use. This
//! test pins the structural sections to the repository:
//!
//! - every path in the Project Structure tree exists;
//! - every direct child of `src/` (files and directories, hidden files
//!   excluded) appears in that tree;
//! - every crate named in the Key Dependencies table is a real dependency.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn agents_md() -> String {
    fs::read_to_string(repo_root().join("AGENTS.md")).expect("read AGENTS.md")
}

/// The fenced Project Structure tree, without the fence lines.
fn project_structure_block(text: &str) -> &str {
    let marker = "```\nbeads_rust/\n";
    let start = text
        .find(marker)
        .expect("AGENTS.md Project Structure block starting with `beads_rust/`");
    let body = &text[start + 4..];
    let end = body.find("\n```").expect("closing fence");
    &body[..end]
}

/// Repo-relative paths listed in the tree (directories without the trailing
/// slash), reconstructed from the box-drawing indentation.
fn listed_paths(block: &str) -> Vec<String> {
    let mut stack: Vec<String> = Vec::new();
    let mut paths = Vec::new();
    for line in block.lines().skip(1) {
        let line = line.split('#').next().unwrap_or("").trim_end();
        let Some((byte_index, _)) = line.char_indices().find(|(_, c)| *c == '├' || *c == '└')
        else {
            continue;
        };
        let depth = line[..byte_index].chars().count() / 4;
        let name = line[byte_index..]
            .trim_start_matches(['├', '└', '─', ' '])
            .trim();
        if name.is_empty() {
            continue;
        }
        stack.truncate(depth);
        let path = format!("{}{name}", stack.concat());
        paths.push(path.trim_end_matches('/').to_string());
        if name.ends_with('/') {
            stack.push(name.to_string());
        }
    }
    paths
}

/// Crate names in the first column of the Key Dependencies table.
fn key_dependency_names(text: &str) -> Vec<String> {
    let start = text
        .find("### Key Dependencies")
        .expect("Key Dependencies section");
    let section = &text[start..];
    let end = section.find("\n### ").unwrap_or(section.len());
    let mut names = Vec::new();
    for line in section[..end].lines() {
        if !line.starts_with("| `") {
            continue;
        }
        let first_cell = line.trim_start_matches('|').split('|').next().unwrap_or("");
        let mut rest = first_cell;
        while let Some(open) = rest.find('`') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('`') else {
                break;
            };
            names.push(after[..close].to_string());
            rest = &after[close + 1..];
        }
    }
    names
}

/// Dependency names from Cargo.toml's `[dependencies]`, `[build-dependencies]`,
/// and target-specific dependency tables.
fn cargo_dependency_names() -> BTreeSet<String> {
    let cargo = fs::read_to_string(repo_root().join("Cargo.toml")).expect("read Cargo.toml");
    let mut names = BTreeSet::new();
    let mut in_deps = false;
    for line in cargo.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed.ends_with("dependencies]");
            continue;
        }
        if !in_deps || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = trimmed.split_once('=') {
            names.insert(name.trim().trim_matches('"').to_string());
        }
    }
    names
}

#[test]
fn project_structure_paths_exist_and_cover_src() {
    let text = agents_md();
    let listed = listed_paths(project_structure_block(&text));
    assert!(
        listed.len() > 40,
        "expected the tree to list dozens of paths, found {}",
        listed.len()
    );

    let missing: Vec<&String> = listed
        .iter()
        .filter(|path| !repo_root().join(path).exists())
        .collect();
    assert!(
        missing.is_empty(),
        "AGENTS.md Project Structure lists paths that do not exist: {missing:?}"
    );

    let listed_src_children: BTreeSet<String> = listed
        .iter()
        .filter_map(|path| path.strip_prefix("src/"))
        .filter(|rest| !rest.contains('/'))
        .map(str::to_string)
        .collect();
    let actual_src_children: BTreeSet<String> = fs::read_dir(repo_root().join("src"))
        .expect("read src/")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    let unlisted: Vec<&String> = actual_src_children
        .difference(&listed_src_children)
        .collect();
    assert!(
        unlisted.is_empty(),
        "src/ entries missing from the AGENTS.md Project Structure tree: {unlisted:?} \
         (add a row with a one-line purpose, or mark it DORMANT per docs/ARCHITECTURE.md)"
    );
}

#[test]
fn key_dependencies_table_names_real_dependencies() {
    let text = agents_md();
    let named = key_dependency_names(&text);
    assert!(
        named.len() >= 10,
        "expected the Key Dependencies table to name at least ten crates, found {named:?}"
    );
    let actual = cargo_dependency_names();
    let unknown: Vec<&String> = named
        .iter()
        .filter(|name| !actual.contains(*name))
        .collect();
    assert!(
        unknown.is_empty(),
        "AGENTS.md Key Dependencies names crates that are not in Cargo.toml: {unknown:?}"
    );
}

#[test]
fn unsafe_code_lint_level_matches_the_doc() {
    let text = agents_md();
    let lib = fs::read_to_string(repo_root().join("src/lib.rs")).expect("read src/lib.rs");
    let level = if lib.contains("#![forbid(unsafe_code)]") {
        "forbid"
    } else if lib.contains("#![deny(unsafe_code)]") {
        "deny"
    } else {
        panic!("src/lib.rs has no crate-level unsafe_code lint");
    };
    assert!(
        text.contains(&format!("#![{level}(unsafe_code)]")),
        "AGENTS.md must state the crate's actual unsafe_code lint level ({level})"
    );
    let other = if level == "deny" { "forbid" } else { "deny" };
    assert!(
        !text.contains(&format!("#![{other}(unsafe_code)]")),
        "AGENTS.md still mentions #![{other}(unsafe_code)]"
    );
}
