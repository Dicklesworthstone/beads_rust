//! Fallback agent-identity inference from a live `bd watch` process's
//! position in this process's ancestry.
//!
//! `BD_AGENT_ID` remains the sole *authoritative* identity source (see
//! [`super::resolve_agent_identity`]); this module only kicks in when
//! that variable is unset/empty. Real fleets have long-lived agent
//! sessions that predate the env var landing in their environment, but
//! every one of them already runs `bd watch --prefix <their-id>` as a
//! monitor, and each live watch owns a row in the `watchers` table
//! (see [`crate::storage::watchers`]).
//!
//! # Algorithm (deepest-match)
//!
//! 1. Read all fresh (non-stale, non-operator) watcher rows.
//! 2. Compute the caller's ancestor chain: self pid -> ppid -> ... -> 1.
//! 3. Compute each live watcher's ancestor chain the same way (skipping
//!    watchers whose pid no longer exists).
//! 4. Walk the caller's chain nearest-first. At the first ancestor that
//!    appears in (or equals the pid of) at least one watcher's chain,
//!    stop:
//!    - exactly one watcher matches there -> that watcher's prefix wins.
//!    - multiple watchers match at the same ancestor -> tie-break by
//!      the watcher's own distance to that ancestor; if still tied,
//!      the result is ambiguous.
//! 5. pid 1 and pid 0 (trivial process-tree roots) never match; reaching
//!    them without a hit is a clean "no match".
//!
//! Why deepest-match works: a leader spawns children, so a child's own
//! watcher's ancestor chain also contains the leader's host process —
//! but a caller running *inside* the child hits the child's host
//! process (near) before the leader's host process (far), and a caller
//! running in the leader hits its own host first. Two unrelated agents
//! typically share only the process-tree root (pid 1), which is
//! excluded by construction.
//!
//! # Structure
//!
//! [`infer_prefix`] is the pure-data core matcher (no I/O) so unit
//! tests can exercise the algorithm directly on hand-built ancestor
//! chains. [`resolve_via_watchers`] adds the freshness / operator /
//! liveness filtering, and takes the chain-reading strategy as a
//! [`ChainReader`] so it too is testable without touching real
//! processes. [`resolve_agent_identity_with_storage`] is the thin glue
//! that real call sites use: it wires up the real `/proc` reader and
//! the storage-backed watcher rows.

use std::env;

use chrono::{DateTime, Utc};

use super::{OPERATOR_PREFIX, missing_agent_id_error, resolve_agent_identity_from};
use crate::error::{BeadsError, Result};
use crate::storage::SqliteStorage;
use crate::storage::watchers::{WATCHER_TTL_SECONDS, WatcherRow};

/// Resolve the calling agent's identity, falling back to live-`bd
/// watch` ancestry inference when `BD_AGENT_ID` is unset/empty.
///
/// `BD_AGENT_ID`, when set to a non-empty value, always wins — the
/// inference fallback below runs only when it is absent (or
/// whitespace-only, matching the existing trim semantics). When
/// inference succeeds, a one-line note is printed to stderr so
/// misattribution is caught fast; it is deliberately not silent.
///
/// # Errors
///
/// Returns a validation error if `BD_AGENT_ID` is set to the reserved
/// `operator` value, if inference is ambiguous (multiple live watchers
/// match equally), or if no identity can be determined by any means.
pub fn resolve_agent_identity_with_storage(storage: &SqliteStorage) -> Result<String> {
    let raw = env::var("BD_AGENT_ID").ok();
    if !raw.as_deref().unwrap_or("").trim().is_empty() {
        // BD_AGENT_ID is set to something non-empty: normal (strict)
        // resolution, no inference attempted. This also correctly
        // surfaces the "operator is reserved" error unmodified.
        return resolve_agent_identity_from(raw.as_deref());
    }

    let rows = storage.list_all_watchers()?;
    let reader = proc::ProcChainReader;
    let caller_pid = i64::from(std::process::id());

    match resolve_via_watchers(&reader, caller_pid, &rows, Utc::now(), WATCHER_TTL_SECONDS) {
        InferenceResult::Matched(identity) => {
            eprintln!(
                "identity: inferred '{}' from live bd watch (pid {}); set BD_AGENT_ID to silence this",
                identity.prefix, identity.pid
            );
            Ok(identity.prefix)
        }
        InferenceResult::Ambiguous(mut prefixes) => {
            prefixes.sort();
            prefixes.dedup();
            Err(BeadsError::validation(
                "identity",
                format!(
                    "BD_AGENT_ID is not set and multiple live `bd watch` prefixes match \
                     this process's ancestry equally ({}); set BD_AGENT_ID to disambiguate.",
                    prefixes.join(", ")
                ),
            ))
        }
        InferenceResult::NoMatch => Err(missing_agent_id_error(true)),
    }
}

/// Resolve the calling agent's identity the same way as
/// [`resolve_agent_identity_with_storage`], but for write paths that
/// must never hard-fail and must never print the "identity: inferred"
/// stderr note.
///
/// Two differences from the loud/strict variant:
/// - Every non-authoritative outcome (`BD_AGENT_ID` unset and no
///   process-ancestry match, an ambiguous match, or a storage error
///   reading the `watchers` table) collapses to `None` instead of an
///   `Err`. Falling back further (to `$USER`, then `"unknown"`) is the
///   *normal* case here — most callers run from a plain human shell
///   with no `BD_AGENT_ID` and no live watch, and that must keep
///   working.
/// - No stderr note is printed on a successful inference match. The
///   note in [`resolve_agent_identity_with_storage`] exists so
///   messaging/watch identity misattribution is caught immediately;
///   printing it on every `bd create` (or any other write) would be
///   noise for a value that's merely recorded as provenance, not acted
///   on synchronously. Callers that want the loud note (messaging,
///   `bd watch`) should keep calling the strict function instead.
///
/// Used by `created_by` / actor-provenance resolution (see
/// [`super::resolve_actor_with_storage`]).
pub fn resolve_agent_identity_quiet(storage: &SqliteStorage) -> Option<String> {
    let raw = env::var("BD_AGENT_ID").ok();
    if !raw.as_deref().unwrap_or("").trim().is_empty() {
        return resolve_agent_identity_from(raw.as_deref()).ok();
    }

    let rows = storage.list_all_watchers().ok()?;
    let reader = proc::ProcChainReader;
    let caller_pid = i64::from(std::process::id());

    match resolve_via_watchers(&reader, caller_pid, &rows, Utc::now(), WATCHER_TTL_SECONDS) {
        InferenceResult::Matched(identity) => Some(identity.prefix),
        InferenceResult::Ambiguous(_) | InferenceResult::NoMatch => None,
    }
}

/// A live watcher's ancestor chain, keyed by prefix, as plain data.
type WatcherChain = (String, Vec<i64>);

/// Outcome of the pure-data matcher.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MatchOutcome {
    Matched { prefix: String, pid: i64 },
    Ambiguous(Vec<String>),
    NoMatch,
}

/// Deepest-match identity inference over plain ancestor-chain data (no
/// I/O). `caller_chain` is the caller's own pid followed by its
/// ancestors, nearest first (`caller_chain[0]` is the caller's own
/// pid, absent if the caller's pid could not be determined at all).
/// Each `watchers` entry pairs a live watcher's prefix with its own
/// ancestor chain in the same nearest-first shape (`chain[0]` is the
/// watcher's own pid).
///
/// See the module docs for the full algorithm.
fn infer_prefix(caller_chain: &[i64], watchers: &[WatcherChain]) -> MatchOutcome {
    for &ancestor in caller_chain {
        // Trivial process-tree roots never match; reaching one means
        // the walk is over with no hit.
        if ancestor <= 1 {
            break;
        }

        let mut candidates: Vec<(&str, usize, i64)> = Vec::new();
        for (prefix, chain) in watchers {
            if let Some(dist) = chain.iter().position(|&p| p == ancestor) {
                let watcher_pid = chain.first().copied().unwrap_or(ancestor);
                candidates.push((prefix.as_str(), dist, watcher_pid));
            }
        }

        if candidates.is_empty() {
            continue;
        }

        let min_dist = candidates
            .iter()
            .map(|&(_, dist, _)| dist)
            .min()
            .expect("candidates is non-empty");

        let mut winners: Vec<(&str, i64)> = candidates
            .into_iter()
            .filter(|&(_, dist, _)| dist == min_dist)
            .map(|(prefix, _, pid)| (prefix, pid))
            .collect();
        winners.sort_unstable();
        winners.dedup();

        return if winners.len() == 1 {
            let (prefix, pid) = winners[0];
            MatchOutcome::Matched {
                prefix: prefix.to_string(),
                pid,
            }
        } else {
            let mut prefixes: Vec<String> =
                winners.into_iter().map(|(p, _)| p.to_string()).collect();
            prefixes.sort();
            prefixes.dedup();
            MatchOutcome::Ambiguous(prefixes)
        };
    }
    MatchOutcome::NoMatch
}

/// Reads process ancestry. Abstracted so the matching logic above can
/// be unit-tested without touching real processes; [`proc::ProcChainReader`]
/// is the real `/proc`-backed implementation used in production.
trait ChainReader {
    /// Ancestor chain for `pid`, nearest first (`pid` itself at index
    /// 0). Returns an empty vec if `pid` cannot be determined at all
    /// (already gone, `/proc` unavailable, unsupported OS).
    fn ancestor_chain(&self, pid: i64) -> Vec<i64>;

    /// Best-effort liveness check for `pid`.
    fn process_exists(&self, pid: i64) -> bool;
}

/// A successfully inferred identity, with the watcher pid that
/// justified it (surfaced in the stderr note for auditability).
#[derive(Debug, Clone, PartialEq, Eq)]
struct InferredIdentity {
    prefix: String,
    pid: i64,
}

/// Result of combining live storage rows with process-ancestry
/// matching.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InferenceResult {
    Matched(InferredIdentity),
    Ambiguous(Vec<String>),
    NoMatch,
}

/// Filters `rows` down to fresh, non-operator, still-alive watchers,
/// builds ancestor chains for each (and for the caller), and runs
/// [`infer_prefix`]. Pulled out from [`resolve_agent_identity_with_storage`]
/// purely for testability: callers inject a [`ChainReader`] instead of
/// touching real `/proc`.
fn resolve_via_watchers<R: ChainReader>(
    reader: &R,
    caller_pid: i64,
    rows: &[WatcherRow],
    now: DateTime<Utc>,
    ttl_seconds: i64,
) -> InferenceResult {
    let cutoff = now - chrono::Duration::seconds(ttl_seconds);

    let mut watcher_chains: Vec<WatcherChain> = Vec::new();
    for row in rows {
        // Stale rows are the pid-reuse guard: a crashed watcher's old
        // pid could otherwise get recycled by an unrelated process and
        // falsely "match".
        if row.last_seen < cutoff {
            continue;
        }
        // Never infer the reserved operator prefix.
        if row.prefix.trim().eq_ignore_ascii_case(OPERATOR_PREFIX) {
            continue;
        }
        if !reader.process_exists(row.pid) {
            continue;
        }
        let chain = reader.ancestor_chain(row.pid);
        if chain.is_empty() {
            continue;
        }
        watcher_chains.push((row.prefix.clone(), chain));
    }

    let caller_chain = reader.ancestor_chain(caller_pid);
    if caller_chain.is_empty() {
        // No /proc, unsupported OS, or the caller's own pid vanished
        // mid-lookup (shouldn't happen for `self`) — skip inference
        // silently, matching the "non-Linux or /proc missing" case.
        return InferenceResult::NoMatch;
    }

    match infer_prefix(&caller_chain, &watcher_chains) {
        MatchOutcome::Matched { prefix, pid } => {
            InferenceResult::Matched(InferredIdentity { prefix, pid })
        }
        MatchOutcome::Ambiguous(prefixes) => InferenceResult::Ambiguous(prefixes),
        MatchOutcome::NoMatch => InferenceResult::NoMatch,
    }
}

/// Real `/proc`-backed ancestor reading (Linux) with a silent no-op
/// fallback on other platforms.
mod proc {
    use super::ChainReader;

    /// Hard cap on ancestor-chain length: guards against ppid cycles
    /// or reparenting races producing an unbounded walk.
    #[cfg(target_os = "linux")]
    const MAX_CHAIN_DEPTH: usize = 128;

    pub struct ProcChainReader;

    #[cfg(target_os = "linux")]
    impl ChainReader for ProcChainReader {
        fn ancestor_chain(&self, pid: i64) -> Vec<i64> {
            linux::build_chain(pid, MAX_CHAIN_DEPTH)
        }

        fn process_exists(&self, pid: i64) -> bool {
            linux::process_exists(pid)
        }
    }

    #[cfg(not(target_os = "linux"))]
    impl ChainReader for ProcChainReader {
        fn ancestor_chain(&self, _pid: i64) -> Vec<i64> {
            Vec::new()
        }

        fn process_exists(&self, _pid: i64) -> bool {
            false
        }
    }

    #[cfg(target_os = "linux")]
    pub mod linux {
        use std::collections::HashSet;
        use std::fs;
        use std::path::Path;

        /// Reads `/proc/<pid>/status` (never `/proc/<pid>/stat` field
        /// 4 — `comm` there can contain spaces/parens and desyncs
        /// positional field parsing) for the `PPid:` line.
        pub fn read_ppid(pid: i64) -> Option<i64> {
            let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("PPid:") {
                    return rest.trim().parse::<i64>().ok();
                }
            }
            None
        }

        pub fn process_exists(pid: i64) -> bool {
            pid > 0 && Path::new(&format!("/proc/{pid}")).is_dir()
        }

        /// Walks `start_pid -> ppid -> ... -> 1`, nearest first,
        /// stopping at pid 1, a cycle, a vanished pid, or
        /// `max_depth` entries — whichever comes first.
        pub fn build_chain(start_pid: i64, max_depth: usize) -> Vec<i64> {
            let mut chain = Vec::with_capacity(8);
            let mut seen = HashSet::new();
            let mut current = start_pid;
            loop {
                if current <= 0 || chain.len() >= max_depth || !seen.insert(current) {
                    break;
                }
                chain.push(current);
                if current == 1 {
                    break;
                }
                match read_ppid(current) {
                    Some(parent) if parent > 0 => current = parent,
                    _ => break,
                }
            }
            chain
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::collections::{HashMap, HashSet};

    struct FakeReader {
        chains: HashMap<i64, Vec<i64>>,
        alive: HashSet<i64>,
    }

    impl FakeReader {
        fn new() -> Self {
            Self {
                chains: HashMap::new(),
                alive: HashSet::new(),
            }
        }

        /// Registers pid `pid`'s ancestor chain (nearest first,
        /// `pid` itself included at index 0) and marks it alive.
        fn with_chain(mut self, pid: i64, chain: Vec<i64>) -> Self {
            self.alive.insert(pid);
            self.chains.insert(pid, chain);
            self
        }

        fn dead(mut self, pid: i64) -> Self {
            self.alive.remove(&pid);
            self
        }
    }

    impl ChainReader for FakeReader {
        fn ancestor_chain(&self, pid: i64) -> Vec<i64> {
            self.chains.get(&pid).cloned().unwrap_or_default()
        }

        fn process_exists(&self, pid: i64) -> bool {
            self.alive.contains(&pid)
        }
    }

    fn row(prefix: &str, pid: i64, last_seen: DateTime<Utc>) -> WatcherRow {
        WatcherRow {
            prefix: prefix.to_string(),
            pid,
            started_at: last_seen,
            last_seen,
            cwd: String::new(),
            git_remote: String::new(),
        }
    }

    // ---- infer_prefix (pure matcher) --------------------------------

    #[test]
    fn own_agent_match() {
        // Caller: self(900) -> host(500) -> shell(200) -> 1
        // Own watcher's chain: watch(800) -> host(500) -> shell(200) -> 1
        // Nearest shared ancestor is host(500); exactly one watcher there.
        let caller_chain = vec![900, 500, 200, 1];
        let watchers = vec![("mine".to_string(), vec![800, 500, 200, 1])];
        assert_eq!(
            infer_prefix(&caller_chain, &watchers),
            MatchOutcome::Matched {
                prefix: "mine".to_string(),
                pid: 800,
            }
        );
    }

    #[test]
    fn caller_is_itself_a_watcher_pid() {
        // Degenerate but valid: the caller's own pid IS a live watcher
        // pid (e.g. bd watch calling this to resolve its own identity
        // mid-restart). Distance 0 in the watcher's own chain.
        let caller_chain = vec![800, 500, 200, 1];
        let watchers = vec![("mine".to_string(), vec![800, 500, 200, 1])];
        assert_eq!(
            infer_prefix(&caller_chain, &watchers),
            MatchOutcome::Matched {
                prefix: "mine".to_string(),
                pid: 800,
            }
        );
    }

    #[test]
    fn leader_vs_child_disambiguation() {
        // Leader host pid 500 is a shared ancestor of everyone, but a
        // caller running inside the CHILD must match the child's own
        // watcher (host 600, near) rather than the leader's watcher
        // (host 500, far) even though the leader's watcher chain also
        // eventually reaches 500 — deepest match wins.
        let caller_chain = vec![900, 600, 500, 200, 1];
        let child_watcher_chain = vec![700, 601, 600, 500, 200, 1];
        let leader_watcher_chain = vec![800, 500, 200, 1];
        let watchers = vec![
            ("child".to_string(), child_watcher_chain),
            ("leader".to_string(), leader_watcher_chain),
        ];
        assert_eq!(
            infer_prefix(&caller_chain, &watchers),
            MatchOutcome::Matched {
                prefix: "child".to_string(),
                pid: 700,
            }
        );
    }

    #[test]
    fn unrelated_agent_no_match_at_shell_depth() {
        // Two unrelated agents only converge at pid 1 (the excluded
        // trivial root) — e.g. separate top-level sessions. No watcher
        // for this caller exists; the walk must reach pid 1 and stop
        // without matching the unrelated agent's watcher.
        let caller_chain = vec![900, 600, 500, 200, 1];
        let unrelated_watcher_chain = vec![800, 700, 650, 200 + 1000, 1];
        let watchers = vec![("other".to_string(), unrelated_watcher_chain)];
        assert_eq!(
            infer_prefix(&caller_chain, &watchers),
            MatchOutcome::NoMatch
        );
    }

    #[test]
    fn tie_produces_ambiguity() {
        // Two sibling agents share the exact same immediate ancestor
        // at the same distance (e.g. the caller IS that shared
        // ancestor, or a process directly under it) — no way to break
        // the tie, must error out asking for BD_AGENT_ID.
        let caller_chain = vec![500, 200, 1];
        let watchers = vec![
            ("sib-a".to_string(), vec![801, 500, 200, 1]),
            ("sib-b".to_string(), vec![802, 500, 200, 1]),
        ];
        assert_eq!(
            infer_prefix(&caller_chain, &watchers),
            MatchOutcome::Ambiguous(vec!["sib-a".to_string(), "sib-b".to_string()])
        );
    }

    #[test]
    fn pid_one_never_matches() {
        // Even if some (buggy) watcher chain literally lists pid 1,
        // it must never be treated as a match.
        let caller_chain = vec![1];
        let watchers = vec![("weird".to_string(), vec![1])];
        assert_eq!(infer_prefix(&caller_chain, &watchers), MatchOutcome::NoMatch);
    }

    #[test]
    fn pid_zero_never_matches() {
        let caller_chain = vec![900, 0];
        let watchers = vec![("weird".to_string(), vec![0])];
        assert_eq!(infer_prefix(&caller_chain, &watchers), MatchOutcome::NoMatch);
    }

    #[test]
    fn empty_watcher_list_is_no_match() {
        let caller_chain = vec![900, 500, 200, 1];
        assert_eq!(infer_prefix(&caller_chain, &[]), MatchOutcome::NoMatch);
    }

    // ---- resolve_via_watchers (freshness / operator / liveness) ----

    #[test]
    fn stale_watcher_excluded() {
        let now = Utc::now();
        let reader = FakeReader::new()
            .with_chain(900, vec![900, 500, 200, 1])
            .with_chain(800, vec![800, 500, 200, 1]);
        let rows = vec![row("stale", 800, now - Duration::seconds(300))];
        let result = resolve_via_watchers(&reader, 900, &rows, now, 60);
        assert_eq!(result, InferenceResult::NoMatch);
    }

    #[test]
    fn fresh_watcher_matches() {
        let now = Utc::now();
        let reader = FakeReader::new()
            .with_chain(900, vec![900, 500, 200, 1])
            .with_chain(800, vec![800, 500, 200, 1]);
        let rows = vec![row("mine", 800, now)];
        let result = resolve_via_watchers(&reader, 900, &rows, now, 60);
        assert_eq!(
            result,
            InferenceResult::Matched(InferredIdentity {
                prefix: "mine".to_string(),
                pid: 800,
            })
        );
    }

    #[test]
    fn operator_row_excluded() {
        let now = Utc::now();
        let reader = FakeReader::new()
            .with_chain(900, vec![900, 500, 200, 1])
            .with_chain(800, vec![800, 500, 200, 1]);
        let rows = vec![row(super::super::OPERATOR_PREFIX, 800, now)];
        let result = resolve_via_watchers(&reader, 900, &rows, now, 60);
        assert_eq!(result, InferenceResult::NoMatch);
    }

    #[test]
    fn operator_row_excluded_case_insensitive() {
        let now = Utc::now();
        let reader = FakeReader::new()
            .with_chain(900, vec![900, 500, 200, 1])
            .with_chain(800, vec![800, 500, 200, 1]);
        let rows = vec![row("OPERATOR", 800, now)];
        let result = resolve_via_watchers(&reader, 900, &rows, now, 60);
        assert_eq!(result, InferenceResult::NoMatch);
    }

    #[test]
    fn vanished_pid_excluded() {
        // Row exists in the DB (crashed watcher, not yet swept) but the
        // pid no longer exists on the box.
        let now = Utc::now();
        let reader = FakeReader::new()
            .with_chain(900, vec![900, 500, 200, 1])
            .with_chain(800, vec![800, 500, 200, 1])
            .dead(800);
        let rows = vec![row("gone", 800, now)];
        let result = resolve_via_watchers(&reader, 900, &rows, now, 60);
        assert_eq!(result, InferenceResult::NoMatch);
    }

    #[test]
    fn no_proc_support_is_silent_no_match() {
        // Simulates non-Linux / missing /proc: the reader can't even
        // determine the caller's own chain.
        let now = Utc::now();
        let reader = FakeReader::new().with_chain(800, vec![800, 500, 200, 1]);
        let rows = vec![row("mine", 800, now)];
        let result = resolve_via_watchers(&reader, 900, &rows, now, 60);
        assert_eq!(result, InferenceResult::NoMatch);
    }

    // ---- Real /proc reader integration (Linux only) -----------------

    #[test]
    #[cfg(target_os = "linux")]
    fn proc_chain_reader_reaches_pid_1_on_self() {
        let reader = ProcChainReaderForTest;
        let self_pid = i64::from(std::process::id());
        let chain = reader.ancestor_chain(self_pid);
        assert!(!chain.is_empty(), "must read at least our own pid");
        assert_eq!(chain[0], self_pid);
        assert_eq!(
            *chain.last().unwrap(),
            1,
            "our own ancestor chain must reach pid 1: {chain:?}"
        );
        assert!(reader.process_exists(self_pid));
        assert!(reader.process_exists(1));
    }

    #[cfg(target_os = "linux")]
    use super::proc::ProcChainReader as ProcChainReaderForTest;
}
