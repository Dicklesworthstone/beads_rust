//! Invariants over the recorded snapshot corpus ITSELF, not over the code
//! that produced it.
//!
//! WHY THIS FILE EXISTS. A snapshot test knows one thing: whether output
//! CHANGED. It knows nothing about whether output is CORRECT. Whatever was in
//! the buffer on the day someone ran `cargo insta accept` is promoted to
//! ground truth, and from then on the test defends it — including any defect
//! it happened to contain. That is not a hypothetical here: for one release
//! `cli_output__search_output.snap` recorded a `br search` line with the
//! `[task]` type badge missing, because the console markup parser had eaten
//! it. The test passed for as long as the bug lived, and would have FAILED
//! the fix (see the comment on `snapshot_search_output` in
//! `tests/snapshots/cli_output.rs`).
//!
//! The checks below are the ones that can be made about a snapshot without
//! knowing what the command was supposed to print. They are deliberately
//! about SHAPE — a recorded value that is empty, unreferenced, or visibly
//! mangled is wrong no matter what the command is. Anything that needs to
//! know the intended text belongs in a semantic test
//! (`tests/e2e_markup_escaping.rs`), not here.
//!
//! WHAT RE-RECORDING PROVES. `cargo insta accept` proves that the new bytes
//! are the bytes the code currently emits. It proves nothing about whether
//! those bytes are right. When one of these tests fails, the fix is to read
//! the recorded value and decide whether the command SHOULD print it — never
//! to re-record until the check goes quiet.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Directory holding every recorded `.snap` in the tree.
const SNAPSHOT_DIR: &str = "tests/snapshots/snapshots";

/// A parsed `.snap` file: insta's YAML header plus the recorded value.
struct Snapshot {
    /// File name, for failure messages.
    file: String,
    /// Short snapshot name — the last `__`-separated segment of the file
    /// name, which is the string passed to `assert_snapshot!("...")`.
    name: String,
    /// Header fields (`source`, `expression`, `assertion_line`, ...).
    header: BTreeMap<String, String>,
    /// The recorded value, verbatim, with no surrounding blank lines.
    body: String,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Parse every `.snap` in [`SNAPSHOT_DIR`].
///
/// The corpus is asserted non-empty: a hygiene suite that silently finds
/// nothing to check is the same failure it exists to prevent.
fn load_corpus() -> Vec<Snapshot> {
    let dir = repo_root().join(SNAPSHOT_DIR);
    let mut snapshots = Vec::new();

    for entry in fs::read_dir(&dir).expect("snapshot dir readable") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("snap") {
            continue;
        }
        snapshots.push(parse_snapshot(&path));
    }

    assert!(
        !snapshots.is_empty(),
        "no .snap files found under {} — these checks would pass vacuously",
        dir.display()
    );
    snapshots.sort_by(|a, b| a.file.cmp(&b.file));
    snapshots
}

fn parse_snapshot(path: &Path) -> Snapshot {
    let file = path
        .file_name()
        .and_then(|f| f.to_str())
        .expect("utf8 file name")
        .to_string();
    let name = file
        .trim_end_matches(".snap")
        .rsplit("__")
        .next()
        .expect("split yields at least one segment")
        .to_string();

    let contents = fs::read_to_string(path).expect("snapshot readable");
    let mut lines = contents.lines();
    assert_eq!(
        lines.next(),
        Some("---"),
        "{file}: insta snapshots open with a `---` header fence"
    );

    let mut header = BTreeMap::new();
    for line in lines.by_ref() {
        if line == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            header.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    let body = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    Snapshot {
        file,
        name,
        header,
        body,
    }
}

/// A recorded snapshot must name where it came from.
///
/// `source` and `expression` are the only provenance a reviewer gets: they
/// say which test file produced the value and what expression was captured.
/// Without them a `.snap` is an anonymous blob of text that nobody can
/// evaluate for correctness — which is precisely the state that lets a
/// damaged value survive review.
#[test]
fn every_snapshot_declares_its_provenance() {
    for snap in load_corpus() {
        for field in ["source", "expression"] {
            assert!(
                snap.header.contains_key(field),
                "{}: missing `{field}:` header. A snapshot with no provenance \
                 cannot be reviewed for correctness, only for stability.",
                snap.file
            );
        }
    }
}

/// Every `.snap` must be named by a live assertion.
///
/// An unreferenced snapshot is a recorded expectation that nothing checks:
/// it can be arbitrarily wrong forever, and it looks exactly like a passing
/// test to anyone reading the directory. `tests/snapshots/snapshots` held six
/// of these (`where_*`, `info_*`) after the `where`/`info` subcommands moved;
/// one of them recorded `Database: <dir>` where `br where` prints the path of
/// the `.db` FILE, so the tree carried a wrong expected value with no test
/// able to fail on it.
#[test]
fn every_snapshot_is_referenced_by_an_assertion() {
    let root = repo_root();
    let mut sources: BTreeMap<String, String> = BTreeMap::new();

    for snap in load_corpus() {
        let source = snap
            .header
            .get("source")
            .unwrap_or_else(|| panic!("{}: no source header", snap.file));
        let text = sources.entry(source.clone()).or_insert_with(|| {
            fs::read_to_string(root.join(source))
                .unwrap_or_else(|e| panic!("{}: source {source} unreadable: {e}", snap.file))
        });
        assert!(
            text.contains(&format!("\"{}\"", snap.name)),
            "{}: no assertion in {source} names the snapshot \"{}\". \
             An unreferenced snapshot is an expectation nothing can fail on — \
             delete it, or restore the test that owned it.",
            snap.file,
            snap.name
        );
    }
}

/// No snapshot may record an empty value.
///
/// "Empty committed as truth" is this repository's defining defect shape, and
/// an empty snapshot is that shape in test form: it asserts that the command
/// printed nothing, so it passes for a command that silently did nothing,
/// printed to the wrong stream, or lost its output entirely — as long as the
/// exit status stays zero. If a command genuinely has nothing to say, it
/// should say so ("No blocked issues", "Found 0 issue(s)" — both of which
/// this CLI already prints elsewhere), and the snapshot should record that
/// sentence.
#[test]
fn no_snapshot_records_an_empty_value() {
    let empty: Vec<String> = load_corpus()
        .into_iter()
        .filter(|snap| snap.body.is_empty())
        .map(|snap| snap.file)
        .collect();
    assert!(
        empty.is_empty(),
        "these snapshots record an empty expected value: {empty:?}\n\
         An empty expectation cannot distinguish 'printed nothing on purpose' \
         from 'lost its output'. Give the command something to say."
    );
}

/// A snapshot whose value is trivially satisfiable must be backed by a live
/// control assertion in the test that owns it.
///
/// `[]`, `{}`, `{"count": 0}` and a zero-byte file are all values that the
/// command prints when it is WORKING and also when it is completely BROKEN.
/// Recorded against a fixture that contains nothing, they assert nothing
/// while occupying a line in the passing-test count. Five snapshots in this
/// tree were in that state: four "empty result" cases ran on a bare `init`,
/// and `orphans_json` recorded `[]` produced by a guard clause six early
/// returns ahead of the scan the test was named for.
///
/// The convention, and what this enforces: if the correct answer really is
/// empty, the test must first prove — in the same workspace, with the same
/// command — that a non-empty answer is reachable. That control is what
/// distinguishes "the filter correctly excluded everything" from "this
/// command has stopped working". Marked by the string `control failed` in
/// the control's assertion message, which is both the machine-checkable
/// marker and the text a failing run prints.
///
/// The alternative to a control is to make the value non-trivial: give the
/// fixture something to find, or compose the value with the surrounding
/// facts (see `compose_invocation` in `tests/snapshots/mod.rs`).
#[test]
fn every_trivial_snapshot_has_a_live_control() {
    let root = repo_root();
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    let mut offenders = Vec::new();

    for snap in load_corpus() {
        if !is_trivially_satisfiable(&snap.body) {
            continue;
        }
        let Some(source) = snap.header.get("source") else {
            continue;
        };
        let text = sources
            .entry(source.clone())
            .or_insert_with(|| fs::read_to_string(root.join(source)).unwrap_or_default());
        let Some(owner) = enclosing_test_body(text, &snap.name) else {
            offenders.push(format!(
                "{}: records `{}` and no test in {source} could be located",
                snap.file, snap.body
            ));
            continue;
        };
        if !owner.contains("control failed") {
            offenders.push(format!(
                "{}: records `{}` with no live control in its test",
                snap.file, snap.body
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "these snapshots record a value that a totally broken command also \
         produces:\n  {}\n\
         Before asserting the empty answer, prove in the same test that the \
         same command returns something in the same workspace (assert it with \
         a message containing `control failed`), or give the fixture data that \
         makes the value non-trivial.",
        offenders.join("\n  ")
    );
}

/// Whether a recorded value is one that a completely broken command would
/// also produce: nothing at all, an empty collection, or a structure whose
/// every field is zero/empty.
fn is_trivially_satisfiable(body: &str) -> bool {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return true;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return false;
    };
    is_trivial_json(&value)
}

fn is_trivial_json(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Bool(b) => !b,
        serde_json::Value::Number(n) => n.as_f64() == Some(0.0),
        serde_json::Value::String(s) => s.is_empty(),
        serde_json::Value::Array(items) => items.is_empty(),
        serde_json::Value::Object(map) => map.values().all(is_trivial_json),
    }
}

/// Return the source text of the `#[test]` function that asserts `name`.
///
/// Finds the assertion by its snapshot name, walks back to the `#[test]`
/// attribute that opens the function, and forward to the closing brace at
/// column zero.
fn enclosing_test_body(source: &str, name: &str) -> Option<String> {
    let needle = format!("\"{name}\"");
    let at = source.find(&needle)?;
    let start = source[..at].rfind("#[test]")?;
    let end = source[at..]
        .find("\n}\n")
        .map_or(source.len(), |rel| at + rel);
    Some(source[start..end].to_string())
}

/// No snapshot may record a value that the test harness itself mangled.
///
/// The normalizer that runs before recording used to redact any hyphenated
/// token as an issue ID, so `--dry-run`, `--no-color`, `--external-ref`,
/// `parent-child`, `auto-discover` and `Auto-import` were all replaced by
/// `ID-REDACTED` on their way into the file. That is the same failure as the
/// markup parser eating `[task]` — content destroyed before it reached the
/// recorded value — and it left `create_help.snap` unable to detect a rename
/// of the very flags it exists to document.
///
/// A redaction placeholder attached to a flag (`--ID-REDACTED`) is the crisp,
/// zero-false-positive signature of that over-reach: an issue ID is never a
/// flag name.
#[test]
fn no_snapshot_records_an_over_redacted_flag_name() {
    let mut offenders = Vec::new();
    for snap in load_corpus() {
        for (idx, line) in snap.body.lines().enumerate() {
            if line.contains("--ID-REDACTED") || line.contains("--ISSUE_ID") {
                offenders.push(format!("{}:{}: {line}", snap.file, idx + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the normalizer redacted a flag name as if it were an issue ID:\n  {}\n\
         The recorded value has lost the text it was meant to pin. Narrow the \
         redaction rule (tests/snapshots/mod.rs) and re-record — the re-record \
         RESTORES eaten content, it does not bless it.",
        offenders.join("\n  ")
    );
}

/// No snapshot may record a markup escape that reached the screen.
///
/// The mirror image of the eaten-badge bug: a `\` applied at a sink that
/// does not parse markup prints the backslash literally. `br show` shipped
/// exactly that for one release, rendering `use \[bold] for headings` for a
/// body that said `use [bold] for headings`. If such output is ever recorded,
/// this fails instead of the snapshot quietly agreeing with it.
#[test]
fn no_snapshot_records_a_markup_escape_artifact() {
    let mut offenders = Vec::new();
    for snap in load_corpus() {
        for (idx, line) in snap.body.lines().enumerate() {
            if line.contains("\\[") || line.contains("\\]") {
                offenders.push(format!("{}:{}: {line}", snap.file, idx + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a markup escape reached recorded output:\n  {}\n\
         Text bound for a non-markup sink must not be escaped; the backslash \
         is not in the stored value and must not be in the expectation.",
        offenders.join("\n  ")
    );
}
