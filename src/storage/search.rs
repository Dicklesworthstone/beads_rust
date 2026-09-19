//! Unicode-aware discovery without full hydration of nonmatching issues.
//!
//! These explicitly named helpers do not change the ASCII-folding contract of
//! `search_issues`, its text projection, or `count_closed_search_matches`.

use super::{ListFilters, SqliteStorage};
use crate::error::{BeadsError, Result};
use crate::model::Issue;
use regex::{Regex, RegexBuilder};
use std::collections::{HashMap, HashSet};

const SEARCH_BATCH_SIZE: usize = 256;

impl SqliteStorage {
    /// Match a literal query with Unicode case folding before client pagination.
    ///
    /// The existing description projection supplies candidates, retaining its
    /// full-row fallback for unsupported filters. Only matches are then hydrated
    /// in bounded batches. Comments are searched but never attached to results.
    /// Callers still own client filters, final sorting, offsets and page limits.
    ///
    /// Like the existing search paths, this is not a single-snapshot read.
    /// Detectable disappearance or changes to matched text fail rather than
    /// silently returning an incomplete page or a stale field match.
    ///
    /// # Errors
    ///
    /// Rejects blank/invalid queries, failed reads and inconsistent hydration.
    pub(crate) fn search_unicode_issues_unpaginated(
        &self,
        query: &str,
        filters: &ListFilters,
    ) -> Result<Vec<Issue>> {
        let candidates = self.unicode_search_candidates(query, filters)?;
        hydrate_matches(&candidates, |ids| self.get_issues_by_ids(ids))
    }

    /// Count matching issues without hydrating their unrelated full fields.
    ///
    /// Filters select the corpus; pagination is deliberately ignored. The CLI
    /// supplies a closed-only corpus for hidden-history counts. Callers needing
    /// client-only filters must instead filter the full matching records.
    ///
    /// # Errors
    ///
    /// Returns query-validation or candidate/comment read errors.
    pub(crate) fn count_unicode_search_matches_unpaginated(
        &self,
        query: &str,
        filters: &ListFilters,
    ) -> Result<usize> {
        Ok(self.unicode_search_candidates(query, filters)?.len())
    }

    fn unicode_search_candidates(&self, query: &str, filters: &ListFilters) -> Result<Vec<Issue>> {
        let query = query.trim();
        if query.is_empty() {
            return Err(BeadsError::Validation {
                field: "query".to_string(),
                reason: "search query cannot be empty".to_string(),
            });
        }
        let matcher = RegexBuilder::new(&regex::escape(query))
            .case_insensitive(true)
            .build()
            .map_err(|error| BeadsError::Validation {
                field: "query".to_string(),
                reason: format!("cannot compile Unicode search query: {error}"),
            })?;
        let mut filters = filters.clone();
        filters.limit = None;
        filters.offset = None;
        let mut candidates = self.list_lint_issues_for_command_output(&filters)?;
        let mut matched_ids = HashSet::new();
        for batch in candidates.chunks(SEARCH_BATCH_SIZE) {
            let mut comment_ids = Vec::new();
            for issue in batch {
                if unicode_issue_fields_match(issue, &matcher) {
                    matched_ids.insert(issue.id.clone());
                } else {
                    comment_ids.push(issue.id.clone());
                }
            }
            // Use the existing bound-parameter API, searching all history. A
            // direct field hit needs no comments, and repeated comment hits
            // must still count as one issue.
            if comment_ids.is_empty() {
                continue;
            }
            for (id, comments) in self.get_comments_for_issues(&comment_ids)? {
                if !comment_ids.contains(&id) {
                    return Err(BeadsError::Internal {
                        message: "Unicode search returned comments for an unexpected issue"
                            .to_string(),
                    });
                }
                if comments
                    .iter()
                    .any(|comment| matcher.is_match(&comment.body))
                {
                    matched_ids.insert(id);
                }
            }
        }
        candidates.retain(|issue| matched_ids.contains(&issue.id));
        Ok(candidates)
    }
}

// `pub`, not `pub(crate)`: `mod search` is private (src/storage/mod.rs:18) and
// the only way out is the `pub(crate) use` re-export on line 22, so this stays
// crate-visible either way and `pub(crate)` here is what
// `clippy::redundant_pub_crate` rejects. Widen the re-export, not this, if the
// function ever needs to leave the crate.
pub fn unicode_issue_fields_match(issue: &Issue, matcher: &Regex) -> bool {
    matcher.is_match(&issue.id)
        || matcher.is_match(&issue.title)
        || issue
            .description
            .as_deref()
            .is_some_and(|description| matcher.is_match(description))
}

fn hydrate_matches(
    candidates: &[Issue],
    mut read: impl FnMut(&[String]) -> Result<Vec<Issue>>,
) -> Result<Vec<Issue>> {
    let mut result = Vec::with_capacity(candidates.len());
    for batch in candidates.chunks(SEARCH_BATCH_SIZE) {
        let ids = batch
            .iter()
            .map(|issue| issue.id.clone())
            .collect::<Vec<_>>();
        let mut hydrated = HashMap::with_capacity(batch.len());
        for issue in read(&ids)? {
            if !ids.contains(&issue.id) {
                return Err(BeadsError::Internal {
                    message: "Unicode search hydration returned an unexpected issue".to_string(),
                });
            }
            if hydrated.insert(issue.id.clone(), issue).is_some() {
                return Err(BeadsError::Internal {
                    message: "Unicode search hydration returned a duplicate issue".to_string(),
                });
            }
        }
        for candidate in batch {
            let issue =
                hydrated
                    .remove(&candidate.id)
                    .ok_or_else(|| BeadsError::IssueNotFound {
                        id: candidate.id.clone(),
                    })?;
            // These are the fields the narrow projection actually preserves;
            // do not compare its synthetic/default values for unrelated fields.
            if issue.title != candidate.title || issue.description != candidate.description {
                return Err(BeadsError::Internal {
                    message: format!(
                        "Issue {} changed during Unicode search; retry the search",
                        candidate.id
                    ),
                });
            }
            result.push(issue);
        }
    }
    Ok(result)
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
