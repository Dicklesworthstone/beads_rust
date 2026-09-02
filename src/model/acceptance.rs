//! Structured view of the `acceptance_criteria` checklist (GitHub #477).
//!
//! `acceptance_criteria` is stored as free-form markdown. The checklist
//! grammar recognised here is the one the close policy already uses to find
//! unchecked items (`close_policy::find_unchecked_acceptance_criteria`): a
//! line whose first non-blank characters are a list bullet (`-`, `*`, `+`),
//! optional whitespace, `[`, exactly one marker character, `]`. A whitespace
//! marker is unchecked, `x`/`X` is checked, and anything else (`[-]`, `[/]`)
//! is not a checklist item. Lines inside fenced code blocks are ignored.
//!
//! Every edit is byte-preserving: ticking or unticking an item rewrites only
//! that item's marker character, and appending adds one line while leaving
//! the existing bytes (including trailing newlines) untouched. This is what
//! lets `br update --check-acceptance` bypass the #467 overwrite guard
//! without weakening it: the operation cannot destroy content.

use std::ops::Range;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One checklist item of an acceptance-criteria field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AcceptanceItem {
    /// One-based position among the checklist items.
    pub index: usize,
    /// Item text without the bullet and checkbox.
    pub text: String,
    /// Whether the box is ticked.
    pub checked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    item: AcceptanceItem,
    /// Byte range of the marker character inside the field body.
    marker: Range<usize>,
    /// Bullet character of the line (`-`, `*`, `+`).
    bullet: char,
}

/// Parsed checklist of an acceptance-criteria field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceChecklist {
    body: String,
    entries: Vec<Entry>,
    /// The body ends inside a code fence that was never closed; anything
    /// appended after it would be swallowed by the fence.
    ends_in_open_fence: bool,
}

/// How a caller refers to a checklist item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptanceSelector {
    /// 1-based item number.
    Index(usize),
    /// Case-insensitive text match: an exact match wins, otherwise the
    /// selector must be a substring of exactly one item.
    Text(String),
}

impl AcceptanceSelector {
    /// Parse one CLI value into selectors.
    ///
    /// A value made only of comma-separated positive integers (`1,4,5`)
    /// selects by item number; any other value is a single text selector,
    /// taken whole so item text containing commas stays matchable.
    pub fn parse_list(raw: &str) -> Result<Vec<Self>, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("acceptance item selector must not be empty".to_string());
        }
        let pieces: Vec<&str> = trimmed.split(',').map(str::trim).collect();
        let all_numeric = pieces
            .iter()
            .all(|piece| !piece.is_empty() && piece.bytes().all(|byte| byte.is_ascii_digit()));
        if !all_numeric {
            return Ok(vec![Self::Text(trimmed.to_string())]);
        }
        pieces
            .into_iter()
            .map(|piece| match piece.parse::<usize>() {
                Ok(0) => Err("acceptance item numbers are 1-based; 0 is not an item".to_string()),
                Ok(index) => Ok(Self::Index(index)),
                Err(_) => Err(format!("acceptance item number '{piece}' is out of range")),
            })
            .collect()
    }
}

/// Result of applying check/uncheck/append edits to a checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceEdit {
    /// The field body after the edit (byte-identical to the input outside
    /// the touched markers and appended lines).
    pub body: String,
    /// 1-based indexes that were requested to be checked (validated).
    pub checked: Vec<usize>,
    /// 1-based indexes that were requested to be unchecked (validated).
    pub unchecked: Vec<usize>,
    /// 1-based indexes of the appended items, in the resulting checklist.
    pub added: Vec<usize>,
}

impl AcceptanceEdit {
    /// True when the edit produced a body different from the input.
    #[must_use]
    pub fn changed_from(&self, original: &str) -> bool {
        self.body != original
    }
}

/// Why an acceptance-edit request was rejected, keyed by the argument at
/// fault (`check-acceptance`, `uncheck-acceptance`, or `acceptance`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptanceRequestError {
    pub field: &'static str,
    pub reason: String,
}

/// Resolve raw selector lists against `body` and apply the edit, validating
/// the whole request before any byte is rewritten. `br update` and the MCP
/// `update_issue` tool both go through here so the two surfaces accept,
/// reject, and rewrite identically.
///
/// # Errors
///
/// Unparseable selectors, a body with no checklist while items are selected,
/// out-of-range or ambiguous selectors, and contradictory check/uncheck
/// requests are rejected with the argument at fault.
pub fn plan_acceptance_edit(
    body: &str,
    check: &[String],
    uncheck: &[String],
    append: &[String],
) -> Result<AcceptanceEdit, AcceptanceRequestError> {
    let parse_all = |field: &'static str, raw: &[String]| {
        let mut selectors = Vec::new();
        for value in raw {
            selectors.extend(
                AcceptanceSelector::parse_list(value)
                    .map_err(|reason| AcceptanceRequestError { field, reason })?,
            );
        }
        Ok::<_, AcceptanceRequestError>(selectors)
    };
    let check_selectors = parse_all("check-acceptance", check)?;
    let uncheck_selectors = parse_all("uncheck-acceptance", uncheck)?;
    let checklist = AcceptanceChecklist::parse(body);
    if checklist.is_empty() && !(check_selectors.is_empty() && uncheck_selectors.is_empty()) {
        return Err(AcceptanceRequestError {
            field: "check-acceptance",
            reason: "acceptance_criteria has no checklist items to tick; append items first \
                     (--add-acceptance / add_acceptance) or set the whole field \
                     (--acceptance-criteria)"
                .to_string(),
        });
    }
    let resolve_all = |field: &'static str, selectors: &[AcceptanceSelector]| {
        selectors
            .iter()
            .map(|selector| {
                checklist
                    .resolve(selector)
                    .map_err(|reason| AcceptanceRequestError { field, reason })
            })
            .collect::<Result<Vec<usize>, AcceptanceRequestError>>()
    };
    let check = resolve_all("check-acceptance", &check_selectors)?;
    let uncheck = resolve_all("uncheck-acceptance", &uncheck_selectors)?;
    checklist
        .edit(&check, &uncheck, append)
        .map_err(|reason| AcceptanceRequestError {
            field: "acceptance",
            reason,
        })
}

/// Structured acceptance-checklist state after an in-place edit, reported by
/// `br update --json` and the MCP `update_issue` tool (GitHub #477).
#[derive(Debug, Clone, Serialize)]
pub struct AcceptanceCriteriaOutput {
    /// Every checklist item, in document order, with 1-based `index`.
    pub items: Vec<AcceptanceItem>,
    pub checked_count: usize,
    pub total: usize,
    pub remaining: usize,
    /// 1-based indexes this call requested to check.
    pub checked: Vec<usize>,
    /// 1-based indexes this call requested to uncheck.
    pub unchecked: Vec<usize>,
    /// 1-based indexes of the items this call appended.
    pub added: Vec<usize>,
}

impl AcceptanceCriteriaOutput {
    /// Summarise the checklist as it stands after `edit`.
    #[must_use]
    pub fn from_edit(edit: &AcceptanceEdit) -> Self {
        let checklist = AcceptanceChecklist::parse(&edit.body);
        let total = checklist.len();
        let checked_count = checklist.checked_count();
        Self {
            items: checklist.items(),
            checked_count,
            total,
            remaining: total.saturating_sub(checked_count),
            checked: edit.checked.clone(),
            unchecked: edit.unchecked.clone(),
            added: edit.added.clone(),
        }
    }
}

impl AcceptanceChecklist {
    /// Parse the checklist items of an acceptance-criteria body.
    #[must_use]
    pub fn parse(body: &str) -> Self {
        let mut entries = Vec::new();
        let mut fence_marker: Option<char> = None;
        let mut offset = 0_usize;
        for line in body.split_inclusive('\n') {
            let line_start = offset;
            offset += line.len();
            if update_code_fence(line, &mut fence_marker) || fence_marker.is_some() {
                continue;
            }
            let content = line.trim_end_matches(['\n', '\r']);
            let trimmed = content.trim_start();
            let trimmed_start = line_start + (content.len() - trimmed.len());
            if let Some((bullet, marker, checked, text)) = parse_checklist_line(trimmed) {
                let index = entries.len() + 1;
                entries.push(Entry {
                    item: AcceptanceItem {
                        index,
                        text: text.to_string(),
                        checked,
                    },
                    marker: (trimmed_start + marker.start)..(trimmed_start + marker.end),
                    bullet,
                });
            }
        }
        Self {
            body: body.to_string(),
            entries,
            ends_in_open_fence: fence_marker.is_some(),
        }
    }

    /// The body this checklist was parsed from.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Checklist items in document order.
    #[must_use]
    pub fn items(&self) -> Vec<AcceptanceItem> {
        self.entries
            .iter()
            .map(|entry| entry.item.clone())
            .collect()
    }

    /// Number of checklist items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the body contains no checklist items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of ticked items.
    #[must_use]
    pub fn checked_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.item.checked)
            .count()
    }

    /// Resolve a selector to a 1-based item index.
    pub fn resolve(&self, selector: &AcceptanceSelector) -> Result<usize, String> {
        match selector {
            AcceptanceSelector::Index(index) => {
                if (1..=self.entries.len()).contains(index) {
                    Ok(*index)
                } else {
                    Err(format!(
                        "acceptance item {index} does not exist ({} item{} present)",
                        self.entries.len(),
                        if self.entries.len() == 1 { "" } else { "s" }
                    ))
                }
            }
            AcceptanceSelector::Text(needle) => {
                let needle_lower = needle.to_lowercase();
                let exact: Vec<&Entry> = self
                    .entries
                    .iter()
                    .filter(|entry| entry.item.text.to_lowercase() == needle_lower)
                    .collect();
                if let [only] = exact.as_slice() {
                    return Ok(only.item.index);
                }
                let matches: Vec<&Entry> = self
                    .entries
                    .iter()
                    .filter(|entry| entry.item.text.to_lowercase().contains(&needle_lower))
                    .collect();
                match matches.as_slice() {
                    [only] => Ok(only.item.index),
                    [] => Err(format!(
                        "no acceptance item matches \"{needle}\"; items are:\n{}",
                        self.describe_items()
                    )),
                    several => Err(format!(
                        "\"{needle}\" matches {} acceptance items; use the item number instead:\n{}",
                        several.len(),
                        several
                            .iter()
                            .map(|entry| describe_item(&entry.item))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )),
                }
            }
        }
    }

    /// Apply checks, unchecks, and appends, producing a new body.
    ///
    /// Indexes are 1-based and refer to the checklist as it is *before* the
    /// edit; appended items land after every existing line. Every index is
    /// validated before any byte is rewritten, so an invalid request leaves
    /// the caller with an error and no partial edit. Items already in the
    /// requested state are left byte-for-byte alone (an existing `X` stays
    /// `X`).
    pub fn edit(
        &self,
        check: &[usize],
        uncheck: &[usize],
        append: &[String],
    ) -> Result<AcceptanceEdit, String> {
        for index in check.iter().chain(uncheck) {
            self.resolve(&AcceptanceSelector::Index(*index))?;
        }
        if let Some(both) = check.iter().find(|index| uncheck.contains(index)) {
            return Err(format!(
                "acceptance item {both} is requested both checked and unchecked"
            ));
        }
        if !append.is_empty() && self.ends_in_open_fence {
            return Err(
                "acceptance_criteria ends inside an unclosed code fence, so an appended item would \
                 not be a checklist item; close the fence first"
                    .to_string(),
            );
        }
        for text in append {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Err("acceptance criterion text must not be empty".to_string());
            }
            if trimmed.contains(['\n', '\r']) {
                return Err(
                    "acceptance criterion text must be a single line; pass --add-acceptance once per item"
                        .to_string(),
                );
            }
        }

        let mut body = String::with_capacity(self.body.len() + append.len() * 32);
        let mut cursor = 0_usize;
        for entry in &self.entries {
            let want_checked = if check.contains(&entry.item.index) {
                Some(true)
            } else if uncheck.contains(&entry.item.index) {
                Some(false)
            } else {
                None
            };
            let Some(want_checked) = want_checked else {
                continue;
            };
            if want_checked == entry.item.checked {
                continue;
            }
            body.push_str(&self.body[cursor..entry.marker.start]);
            body.push(if want_checked { 'x' } else { ' ' });
            cursor = entry.marker.end;
        }
        body.push_str(&self.body[cursor..]);

        let mut added = Vec::with_capacity(append.len());
        if !append.is_empty() {
            let bullet = self.entries.last().map_or('-', |entry| entry.bullet);
            let newline = if body.contains("\r\n") { "\r\n" } else { "\n" };
            // Keep the existing trailing newline(s) after the appended lines;
            // a blank/whitespace-only field is simply replaced.
            let tail = if body.trim().is_empty() {
                body.clear();
                String::new()
            } else {
                let head_len = body.trim_end_matches(['\n', '\r']).len();
                let tail = body[head_len..].to_string();
                body.truncate(head_len);
                tail
            };
            for (next_index, text) in (self.entries.len() + 1..).zip(append) {
                if !body.is_empty() {
                    body.push_str(newline);
                }
                body.push(bullet);
                body.push_str(" [ ] ");
                body.push_str(text.trim());
                added.push(next_index);
            }
            body.push_str(&tail);
        }

        let mut checked = check.to_vec();
        checked.sort_unstable();
        checked.dedup();
        let mut unchecked = uncheck.to_vec();
        unchecked.sort_unstable();
        unchecked.dedup();
        Ok(AcceptanceEdit {
            body,
            checked,
            unchecked,
            added,
        })
    }

    fn describe_items(&self) -> String {
        self.entries
            .iter()
            .map(|entry| describe_item(&entry.item))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn describe_item(item: &AcceptanceItem) -> String {
    format!(
        "  [{}] {}  {}",
        if item.checked { 'x' } else { ' ' },
        item.index,
        item.text
    )
}

/// Parse one trimmed line as a checklist item.
///
/// Returns the bullet, the byte range of the marker character relative to
/// `trimmed`, whether the item is checked, and the item text.
fn parse_checklist_line(trimmed: &str) -> Option<(char, Range<usize>, bool, &str)> {
    let mut chars = trimmed.char_indices();
    let (_, bullet) = chars.next()?;
    if !matches!(bullet, '-' | '*' | '+') {
        return None;
    }
    let mut next = chars.next()?;
    while next.1.is_whitespace() {
        next = chars.next()?;
    }
    if next.1 != '[' {
        return None;
    }
    let (marker_start, marker) = chars.next()?;
    let checked = match marker {
        'x' | 'X' => true,
        other if other.is_whitespace() => false,
        _ => return None,
    };
    let (close_start, close) = chars.next()?;
    if close != ']' {
        return None;
    }
    let text = trimmed[close_start + close.len_utf8()..].trim();
    Some((bullet, marker_start..close_start, checked, text))
}

/// Track fenced code blocks (``` or ~~~, three or more, same marker closes).
/// Mirrors the close policy's fence handling so both agree on which lines
/// are checklist items.
fn update_code_fence(line: &str, fence_marker: &mut Option<char>) -> bool {
    let trimmed = line.trim_start();
    let Some(marker @ ('`' | '~')) = trimmed.chars().next() else {
        return false;
    };
    let marker_len = trimmed.chars().take_while(|ch| *ch == marker).count();
    if marker_len < 3 {
        return false;
    }
    if fence_marker.is_some_and(|open_marker| open_marker == marker) {
        *fence_marker = None;
    } else if fence_marker.is_none() {
        *fence_marker = Some(marker);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "## Acceptance Criteria\n\
- [x] schema migration applied\n\
  * [ ] rollback path exercised\n\
+[X] telemetry counter emitted\n\
- [-] not a checkbox\n\
```\n\
- [ ] fenced example is ignored\n\
```\n\
-  [\u{a0}] CLI reference regenerated\n\
plain prose line\n";

    #[test]
    fn parses_items_with_all_bullets_markers_and_fences() {
        let checklist = AcceptanceChecklist::parse(SAMPLE);
        let items = checklist.items();
        assert_eq!(items.len(), 4, "{items:?}");
        assert_eq!(items[0].index, 1);
        assert_eq!(items[0].text, "schema migration applied");
        assert!(items[0].checked);
        assert_eq!(items[1].text, "rollback path exercised");
        assert!(!items[1].checked);
        assert_eq!(items[2].text, "telemetry counter emitted");
        assert!(items[2].checked);
        assert_eq!(items[3].text, "CLI reference regenerated");
        assert!(!items[3].checked);
        assert_eq!(checklist.checked_count(), 2);
        assert_eq!(checklist.len(), 4);
    }

    #[test]
    fn agrees_with_close_policy_unchecked_finder() {
        let checklist = AcceptanceChecklist::parse(SAMPLE);
        let unchecked_here: Vec<String> = checklist
            .items()
            .into_iter()
            .filter(|item| !item.checked)
            .map(|item| item.text)
            .collect();
        let unchecked_policy = crate::close_policy::find_unchecked_acceptance_criteria(SAMPLE);
        assert_eq!(unchecked_here, unchecked_policy);
    }

    #[test]
    fn check_rewrites_only_the_marker_byte() {
        let checklist = AcceptanceChecklist::parse(SAMPLE);
        let edit = checklist.edit(&[2, 4], &[], &[]).unwrap();
        let expected = SAMPLE
            .replacen("* [ ] rollback", "* [x] rollback", 1)
            .replacen("[\u{a0}] CLI", "[x] CLI", 1);
        assert_eq!(edit.body, expected);
        assert_eq!(edit.checked, vec![2, 4]);
        assert!(edit.changed_from(SAMPLE));
        let after = AcceptanceChecklist::parse(&edit.body);
        assert_eq!(after.checked_count(), 4);
        assert_eq!(after.len(), 4);
    }

    #[test]
    fn uncheck_keeps_untouched_items_byte_identical() {
        let checklist = AcceptanceChecklist::parse(SAMPLE);
        let edit = checklist.edit(&[], &[1], &[]).unwrap();
        // Item 3 keeps its upper-case `X`; only item 1 changes.
        assert_eq!(
            edit.body,
            SAMPLE.replacen("- [x] schema", "- [ ] schema", 1)
        );
        assert!(edit.body.contains("+[X] telemetry"));
    }

    #[test]
    fn already_in_state_is_a_byte_noop() {
        let checklist = AcceptanceChecklist::parse(SAMPLE);
        let edit = checklist.edit(&[1, 3], &[2], &[]).unwrap();
        assert_eq!(edit.body, SAMPLE);
        assert!(!edit.changed_from(SAMPLE));
    }

    #[test]
    fn out_of_range_index_rejects_without_partial_edit() {
        let checklist = AcceptanceChecklist::parse(SAMPLE);
        let err = checklist.edit(&[2, 9], &[], &[]).unwrap_err();
        assert!(err.contains("item 9 does not exist"), "{err}");
        assert!(err.contains("4 items present"), "{err}");
        let err = checklist.edit(&[2], &[2], &[]).unwrap_err();
        assert!(err.contains("both checked and unchecked"), "{err}");
    }

    #[test]
    fn selector_parsing_distinguishes_numbers_from_text() {
        assert_eq!(
            AcceptanceSelector::parse_list(" 1, 4 ,5 ").unwrap(),
            vec![
                AcceptanceSelector::Index(1),
                AcceptanceSelector::Index(4),
                AcceptanceSelector::Index(5)
            ]
        );
        assert_eq!(
            AcceptanceSelector::parse_list("telemetry, counter").unwrap(),
            vec![AcceptanceSelector::Text("telemetry, counter".to_string())]
        );
        assert!(
            AcceptanceSelector::parse_list("0")
                .unwrap_err()
                .contains("1-based")
        );
        assert!(AcceptanceSelector::parse_list("  ").is_err());
        // An empty piece means the value is not a clean number list, so it
        // becomes a text selector rather than a silently truncated index list.
        assert_eq!(
            AcceptanceSelector::parse_list("1,,2").unwrap(),
            vec![AcceptanceSelector::Text("1,,2".to_string())]
        );
    }

    #[test]
    fn append_refuses_body_ending_in_open_fence() {
        let open = AcceptanceChecklist::parse("- [ ] a\n```\n- [ ] fenced\n");
        assert_eq!(open.len(), 1);
        let err = open.edit(&[], &[], &["b".to_string()]).unwrap_err();
        assert!(err.contains("unclosed code fence"), "{err}");
        // Ticking is still fine; only appends are refused.
        assert!(open.edit(&[1], &[], &[]).is_ok());
    }

    #[test]
    fn append_replaces_whitespace_only_body() {
        let blank = AcceptanceChecklist::parse("  \n");
        let edit = blank.edit(&[], &[], &["first".to_string()]).unwrap();
        assert_eq!(edit.body, "- [ ] first");
    }

    #[test]
    fn text_selector_prefers_exact_then_unique_substring() {
        let body = "- [ ] build\n- [ ] build docs\n- [ ] Telemetry counter\n";
        let checklist = AcceptanceChecklist::parse(body);
        assert_eq!(
            checklist
                .resolve(&AcceptanceSelector::Text("BUILD".to_string()))
                .unwrap(),
            1
        );
        assert_eq!(
            checklist
                .resolve(&AcceptanceSelector::Text("telemetry".to_string()))
                .unwrap(),
            3
        );
        let err = checklist
            .resolve(&AcceptanceSelector::Text("buil".to_string()))
            .unwrap_err();
        assert!(err.contains("matches 2 acceptance items"), "{err}");
        let err = checklist
            .resolve(&AcceptanceSelector::Text("missing".to_string()))
            .unwrap_err();
        assert!(err.contains("no acceptance item matches"), "{err}");
        assert!(err.contains("[ ] 2  build docs"), "{err}");
    }

    #[test]
    fn append_preserves_bullet_style_and_trailing_newline() {
        let body = "* [x] one\n* [ ] two\n\n";
        let checklist = AcceptanceChecklist::parse(body);
        let edit = checklist
            .edit(&[], &[], &["three".to_string(), " four ".to_string()])
            .unwrap();
        assert_eq!(
            edit.body,
            "* [x] one\n* [ ] two\n* [ ] three\n* [ ] four\n\n"
        );
        assert_eq!(edit.added, vec![3, 4]);

        let empty = AcceptanceChecklist::parse("");
        let edit = empty.edit(&[], &[], &["first".to_string()]).unwrap();
        assert_eq!(edit.body, "- [ ] first");
        assert_eq!(edit.added, vec![1]);

        let crlf = AcceptanceChecklist::parse("- [ ] a\r\n");
        let edit = crlf.edit(&[], &[], &["b".to_string()]).unwrap();
        assert_eq!(edit.body, "- [ ] a\r\n- [ ] b\r\n");
    }

    #[test]
    fn append_rejects_empty_and_multi_line_text() {
        let checklist = AcceptanceChecklist::parse("- [ ] a\n");
        assert!(
            checklist
                .edit(&[], &[], &["  ".to_string()])
                .unwrap_err()
                .contains("must not be empty")
        );
        assert!(
            checklist
                .edit(&[], &[], &["a\nb".to_string()])
                .unwrap_err()
                .contains("single line")
        );
    }

    #[test]
    fn check_and_append_in_one_edit_index_against_the_original_list() {
        let checklist = AcceptanceChecklist::parse("- [ ] a\n- [ ] b");
        let edit = checklist.edit(&[2], &[], &["c".to_string()]).unwrap();
        assert_eq!(edit.body, "- [ ] a\n- [x] b\n- [ ] c");
        assert_eq!(edit.added, vec![3]);
    }
}
