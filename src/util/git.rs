//! Git remote URL helpers.
//!
//! The canonical-form normalization is shared with the `ghwatch` agent —
//! both processes need to produce byte-identical strings so the
//! cross-database join (`ghwatch.watch_state.repo == watchers.git_remote`)
//! works without adapter SQL.
//!
//! The function below is dropped in verbatim from ghwatch's source
//! (commit 17352e6 of the `inceptionpointai/ghwatch` repo). Test
//! contract is `tests/normalize_fixture.tsv` in this repo, mirrored
//! at the same path in ghwatch — both sides assert against it.

/// Normalize a remote URL or already-host-qualified repo string into the
/// canonical `host/owner/repo` form.
///
/// Step order:
/// 1. Strip protocol prefix (https://, http://, ssh://, git://)
/// 2. Strip auth: drop through the first `@`, but only when `@` appears
///    before the first `/` in what remains (i.e. inside the authority).
/// 3. SSH `host:path` → `host/path`: if `:` precedes the first `/`,
///    replace it with `/`.
/// 4. Strip trailing `.git`.
/// 5. Strip trailing `/`.
/// 6. Lowercase.
#[must_use]
pub fn canonicalize_repo_url(input: &str) -> String {
    let s = input.trim();

    // 1. Strip protocol prefix.
    let s = ["https://", "http://", "ssh://", "git://"]
        .iter()
        .find_map(|p| s.strip_prefix(p))
        .unwrap_or(s);

    // 2. Strip auth, but only if `@` is inside the authority (before any `/`).
    let s = match (s.find('@'), s.find('/')) {
        (Some(at), Some(slash)) if at < slash => &s[at + 1..],
        (Some(at), None) => &s[at + 1..],
        _ => s,
    };

    // 3. SSH `host:path` → `host/path` when `:` precedes the first `/`.
    let s_owned: String = match (s.find(':'), s.find('/')) {
        (Some(colon), Some(slash)) if colon < slash => {
            let mut t = String::with_capacity(s.len());
            t.push_str(&s[..colon]);
            t.push('/');
            t.push_str(&s[colon + 1..]);
            t
        }
        (Some(colon), None) => {
            let mut t = String::with_capacity(s.len());
            t.push_str(&s[..colon]);
            t.push('/');
            t.push_str(&s[colon + 1..]);
            t
        }
        _ => s.to_string(),
    };

    // 4. Strip trailing `.git`.
    let s = s_owned
        .strip_suffix(".git")
        .map(str::to_owned)
        .unwrap_or(s_owned);

    // 5. Strip trailing `/`.
    let s = s.trim_end_matches('/');

    // 6. Lowercase.
    s.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::canonicalize_repo_url;

    /// The shared fixture — same file path lives in ghwatch's repo.
    /// Format: TSV `input\texpected`, `#` line comments and blank
    /// lines allowed.
    const FIXTURE: &str = include_str!("../../tests/normalize_fixture.tsv");

    #[test]
    fn fixture_round_trip() {
        let mut checked = 0usize;
        for (line_num, raw) in FIXTURE.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (input, expected) = line.split_once('\t').unwrap_or_else(|| {
                panic!("fixture line {}: missing tab", line_num + 1)
            });
            let got = canonicalize_repo_url(input);
            assert_eq!(
                got,
                expected,
                "fixture line {}: canonicalize_repo_url({input:?}) -> {got:?}, expected {expected:?}",
                line_num + 1
            );
            checked += 1;
        }
        assert!(checked > 0, "fixture appears empty");
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(canonicalize_repo_url(""), "");
    }

    #[test]
    fn whitespace_only_returns_empty() {
        assert_eq!(canonicalize_repo_url("   \t  \n  "), "");
    }
}
