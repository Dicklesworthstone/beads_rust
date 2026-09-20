//! Unicode-aware discovery without full hydration of nonmatching issues.
//!
//! These explicitly named helpers do not change the ASCII-folding contract of
//! `search_issues`, its text projection, or `count_closed_search_matches`.

use super::{ListFilters, SqliteStorage};
use crate::error::{BeadsError, Result};
use crate::model::{Comment, Issue};
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

    /// Select a default-ordered Unicode page BEFORE loading full issue records.
    ///
    /// The candidate query already orders by priority ascending, creation time
    /// descending and ID ascending. Preserve that order rather than sorting the
    /// lint projection's synthetic priority values. Only the selected matches
    /// are hydrated; offset rows and matches beyond the limit are not. Callers
    /// may request one extra row to detect truncation without counting all hits.
    ///
    /// This path must not be used before client-only filters or for a different
    /// sort order. `None` and zero limits mean unlimited, as in other list APIs.
    /// The candidate query still materializes its narrow corpus, and unsupported
    /// SQL filter shapes retain the existing full-record fallback.
    ///
    /// # Errors
    ///
    /// Rejects non-default ordering, invalid queries and failed/inconsistent reads.
    pub(crate) fn search_unicode_issues_default_page(
        &self,
        query: &str,
        filters: &ListFilters,
    ) -> Result<Vec<Issue>> {
        if filters.reverse || !matches!(filters.sort.as_deref(), None | Some("priority")) {
            return Err(BeadsError::Validation {
                field: "sort".to_string(),
                reason: "paged Unicode discovery requires default priority ordering".to_string(),
            });
        }
        let mut candidate_filters = filters.clone();
        // Explicit --sort priority means the same order, not a reason to force
        // the projection's full-row fallback.
        candidate_filters.sort = None;
        let candidates = self.unicode_search_candidates_in_window(
            query,
            &candidate_filters,
            filters.offset.unwrap_or(0),
            filters.limit.unwrap_or(0),
        )?;
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
        self.unicode_search_candidates_in_window(query, filters, 0, 0)
    }

    fn unicode_search_candidates_in_window(
        &self,
        query: &str,
        filters: &ListFilters,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Issue>> {
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
        // Applying these in SQL would page NONmatches and lose comment-only
        // hits. Windowing belongs after the complete per-issue predicate.
        filters.limit = None;
        filters.offset = None;
        let candidates = self.list_lint_issues_for_command_output(&filters)?;
        select_matching_window(candidates, &matcher, offset, limit, |ids| {
            self.get_comments_for_issues(ids)
        })
    }
}

/// Scan in candidate order, combining field and historical-comment hits before
/// consuming offset/limit. A batch-local hit set avoids retaining every matched
/// ID. Once a bounded page is complete, later comment batches are not read.
fn select_matching_window(
    candidates: Vec<Issue>,
    matcher: &Regex,
    mut offset: usize,
    limit: usize,
    mut read_comments: impl FnMut(&[String]) -> Result<HashMap<String, Vec<Comment>>>,
) -> Result<Vec<Issue>> {
    let mut candidates = candidates.into_iter();
    let mut selected = Vec::new();
    loop {
        let batch = candidates
            .by_ref()
            .take(SEARCH_BATCH_SIZE)
            .collect::<Vec<_>>();
        if batch.is_empty() {
            return Ok(selected);
        }
        let mut matched_ids = HashSet::new();
        let mut comment_ids = Vec::new();
        for issue in &batch {
            if unicode_issue_fields_match(issue, matcher) {
                matched_ids.insert(issue.id.clone());
            } else {
                comment_ids.push(issue.id.clone());
            }
        }
        // Search ALL comment history, but only for issues without a field hit.
        // Merge hits before selecting rows: processing direct hits first would
        // incorrectly push earlier comment-only issues behind them.
        if !comment_ids.is_empty() {
            for (id, comments) in read_comments(&comment_ids)? {
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
        for issue in batch {
            if !matched_ids.contains(&issue.id) {
                continue;
            }
            if offset > 0 {
                offset -= 1;
                continue;
            }
            selected.push(issue);
            // Do not add offset + limit: both are user-controlled usize values.
            // No allocation is sized directly from an untrusted page bound.
            if limit > 0 && selected.len() == limit {
                return Ok(selected);
            }
        }
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
            // Compare only fields the narrow projection actually preserves.
            // updated_at also detects ordinary priority/assignment mutations
            // that could otherwise move an issue across the page boundary.
            if issue.title != candidate.title
                || issue.description != candidate.description
                || issue.status != candidate.status
                || issue.issue_type != candidate.issue_type
                || issue.created_at != candidate.created_at
                || issue.updated_at != candidate.updated_at
            {
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

#[cfg(test)]
#[path = "search_page_tests.rs"]
mod page_tests;
