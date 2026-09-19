//! Acceptance-aware hydration for the narrow lint projection.

use super::{ListFilters, SqliteStorage};
use crate::error::{BeadsError, Result};
use crate::model::Issue;
use fsqlite_types::value::SqliteValue;
use std::collections::HashMap;

const ACCEPTANCE_BATCH_SIZE: usize = 256;

type AcceptanceById = HashMap<String, Option<String>>;

impl SqliteStorage {
    /// Read lint inputs without hydrating unrelated issue fields or relations.
    ///
    /// Reuse the existing projection's filtering and ordering, then fetch only
    /// the acceptance column for those IDs in bounded batches, retaining the
    /// projection's ordering. This method does not change workspace locking.
    ///
    /// # Errors
    ///
    /// Returns an error on query failure, malformed rows, or an issue disappearing
    /// during hydration. Missing rows must not silently become missing criteria.
    pub(crate) fn list_lint_issues_with_acceptance(
        &self,
        filters: &ListFilters,
    ) -> Result<Vec<Issue>> {
        let mut issues = self.list_lint_issues_for_command_output(filters)?;
        hydrate_acceptance(&mut issues, |batch| {
            let ids = batch
                .iter()
                .map(|issue| issue.id.as_str())
                .collect::<Vec<_>>();
            let query = acceptance_query(&ids);
            decode_acceptance_rows(self.execute_raw_query(&query)?)
        })?;
        Ok(issues)
    }
}

fn hydrate_acceptance(
    issues: &mut [Issue],
    mut read: impl FnMut(&[Issue]) -> Result<AcceptanceById>,
) -> Result<()> {
    for batch in issues.chunks_mut(ACCEPTANCE_BATCH_SIZE) {
        let mut criteria = read(batch)?;
        for issue in batch {
            // Match the canonical row reader, which maps this column through
            // `get_non_empty_str` and so never yields `Some("")`. Writes store
            // a missing value as "" rather than NULL
            // (`acceptance_criteria.as_deref().unwrap_or("")`), so without this
            // filter the narrow lint projection reports `Some("")` where
            // `list_issues` reports `None`, and lint stops seeing those issues
            // as missing acceptance criteria. `decode_acceptance_rows` stays
            // deliberately faithful to what SQL returned; the Issue-level
            // contract is applied here.
            issue.acceptance_criteria = criteria
                .remove(&issue.id)
                .ok_or_else(|| BeadsError::IssueNotFound {
                    id: issue.id.clone(),
                })?
                .filter(|text| !text.is_empty());
        }
        if !criteria.is_empty() {
            return Err(BeadsError::internal(
                "Lint acceptance query returned unexpected issue IDs",
            ));
        }
    }
    Ok(())
}

fn acceptance_query(ids: &[&str]) -> String {
    // The raw-query API has no binding argument. Quote every ID as a SQLite
    // string literal, including IDs imported from legacy or external stores.
    // Never interpolate criteria: only IDs from the filtered projection enter SQL.
    let literals = ids
        .iter()
        .map(|id| format!("'{}'", id.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT id, acceptance_criteria FROM issues WHERE id IN ({literals})")
}

fn decode_acceptance_rows(rows: Vec<Vec<SqliteValue>>) -> Result<AcceptanceById> {
    let mut criteria = HashMap::with_capacity(rows.len());
    for row in rows {
        let [id, value] = row.as_slice() else {
            return Err(BeadsError::internal(
                "Lint acceptance query returned an invalid column count",
            ));
        };
        let id = id.as_text().ok_or_else(|| {
            BeadsError::internal("Lint acceptance query returned a non-text issue ID")
        })?;
        let value = match value {
            SqliteValue::Null => None,
            value => Some(
                value
                    .as_text()
                    .ok_or_else(|| {
                        BeadsError::internal(format!(
                            "Lint acceptance query returned non-text criteria for {id}"
                        ))
                    })?
                    .to_string(),
            ),
        };
        if criteria.insert(id.to_string(), value).is_some() {
            return Err(BeadsError::internal(format!(
                "Lint acceptance query returned duplicate issue ID {id}"
            )));
        }
    }
    Ok(criteria)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(id: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: "Lint hydration test".to_string(),
            ..Issue::default()
        }
    }

    fn text(value: &str) -> SqliteValue {
        SqliteValue::Text(value.to_string().into())
    }

    #[test]
    fn hydration_is_bounded_preserves_order_and_does_not_query_empty_input() {
        for count in [0, 1, ACCEPTANCE_BATCH_SIZE, ACCEPTANCE_BATCH_SIZE + 1, 777] {
            let mut issues = (0..count)
                .rev()
                .map(|index| issue(&format!("bd-{index}")))
                .collect::<Vec<_>>();
            let expected_ids = issues
                .iter()
                .map(|issue| issue.id.clone())
                .collect::<Vec<_>>();
            let mut batches = Vec::new();
            hydrate_acceptance(&mut issues, |batch| {
                batches.push(batch.len());
                Ok(batch
                    .iter()
                    .rev()
                    .map(|issue| {
                        (
                            issue.id.clone(),
                            Some(format!("criterion for {}", issue.id)),
                        )
                    })
                    .collect())
            })
            .unwrap();
            assert_eq!(batches.len(), count.div_ceil(ACCEPTANCE_BATCH_SIZE));
            assert!(batches.iter().all(|size| *size <= ACCEPTANCE_BATCH_SIZE));
            assert_eq!(batches.iter().sum::<usize>(), count);
            for (issue, expected_id) in issues.iter().zip(&expected_ids) {
                assert_eq!(&issue.id, expected_id);
                assert_eq!(
                    issue.acceptance_criteria,
                    Some(format!("criterion for {expected_id}"))
                );
            }
        }
    }

    #[test]
    fn null_blank_and_large_criteria_survive_without_normalization() {
        let values = [
            None,
            Some(String::new()),
            Some(" \t\r\n\u{2003}".to_string()),
            Some("- [ ] exact criteria\r\n".repeat(8192)),
        ];
        let rows = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                vec![
                    text(&format!("bd-{index}")),
                    value.as_deref().map_or(SqliteValue::Null, text),
                ]
            })
            .collect();
        let mut decoded = decode_acceptance_rows(rows).unwrap();
        for (index, expected) in values.into_iter().enumerate() {
            assert_eq!(decoded.remove(&format!("bd-{index}")).unwrap(), expected);
        }
        assert!(decoded.is_empty());
    }

    #[test]
    fn malformed_or_duplicate_rows_are_errors_not_empty_criteria() {
        for rows in [
            vec![vec![text("bd-1")]],
            vec![vec![SqliteValue::Null, text("criterion")]],
            vec![vec![text("bd-1"), SqliteValue::Integer(1)]],
            vec![
                vec![text("bd-1"), SqliteValue::Null],
                vec![text("bd-1"), text("duplicate")],
            ],
        ] {
            assert!(decode_acceptance_rows(rows).is_err());
        }
    }

    #[test]
    fn failed_incomplete_or_extra_reads_cannot_produce_a_clean_scan() {
        let mut issues = vec![issue("bd-1")];
        assert!(
            hydrate_acceptance(&mut issues, |_| {
                Err(BeadsError::internal("read failed"))
            })
            .is_err()
        );
        assert!(matches!(
            hydrate_acceptance(&mut issues, |_| Ok(HashMap::new())),
            Err(BeadsError::IssueNotFound { id }) if id == "bd-1"
        ));
        assert!(
            hydrate_acceptance(&mut issues, |_| {
                Ok(HashMap::from([
                    ("bd-1".to_string(), None),
                    ("bd-unrequested".to_string(), Some("unexpected".to_string())),
                ]))
            })
            .is_err()
        );
    }

    #[test]
    fn query_quotes_ids_as_literals() {
        assert_eq!(
            acceptance_query(&["bd-safe", "bd-quote' OR 1=1 --"]),
            "SELECT id, acceptance_criteria FROM issues WHERE id IN ('bd-safe', 'bd-quote'' OR 1=1 --')"
        );
        let storage = SqliteStorage::open_memory().unwrap();
        // Even a malicious imported ID is a single literal, not executable SQL.
        let rows = storage
            .execute_raw_query(&acceptance_query(&["bd-x'); DROP TABLE issues; --"]))
            .unwrap();
        assert!(rows.is_empty());
        assert!(storage.execute_raw_query("SELECT id FROM issues").is_ok());
    }
}
