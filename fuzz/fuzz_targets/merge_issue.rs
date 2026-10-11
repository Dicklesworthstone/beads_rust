#![no_main]

mod common;

use beads_rust::model::{Comment, Dependency, DependencyType, Issue, IssueType, Priority, Status};
use beads_rust::sync::{
    ConflictResolution, ConflictType, MergeContext, MergeReport, MergeResult, merge_issue,
    three_way_merge,
};
use beads_rust::util::content_hash;
use beads_rust::validation::IssueValidator;
use chrono::{DateTime, Duration, Utc};
use common::{ByteCursor, TrimmedCustomIssueCursorExt};
use libfuzzer_sys::fuzz_target;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

const MAX_INPUT_BYTES: usize = 8 * 1024;
const MAX_FIELD_BYTES: usize = 96;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let result = run_merge_case(data);
    assert!(
        result.is_ok(),
        "merge_issue fuzz invariant failed: {result:?}"
    );
});

#[derive(Debug, Clone)]
struct MergeFuzzCase {
    base: Option<Issue>,
    local: Option<Issue>,
    external: Option<Issue>,
    tombstone_protected: bool,
}

#[derive(Debug, Deserialize)]
struct JsonCase {
    id: Option<String>,
    base: Option<JsonIssue>,
    local: Option<JsonIssue>,
    external: Option<JsonIssue>,
    tombstone_protected: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct JsonIssue {
    id: Option<String>,
    title: Option<String>,
    description: Option<String>,
    status: Option<String>,
    priority: Option<i32>,
    issue_type: Option<String>,
    assignee: Option<String>,
    updated_at: Option<i64>,
    labels: Option<Vec<String>>,
    dependencies: Option<Vec<String>>,
    comments: Option<Vec<String>>,
}

fn run_merge_case(data: &[u8]) -> Result<(), String> {
    let case = parse_json_case(data).unwrap_or_else(|| random_case(data));
    assert_input_hashes_are_consistent(&case)?;

    for strategy in merge_strategies() {
        let first = merge_issue(
            case.base.as_ref(),
            case.local.as_ref(),
            case.external.as_ref(),
            strategy,
        );
        let second = merge_issue(
            case.base.as_ref(),
            case.local.as_ref(),
            case.external.as_ref(),
            strategy,
        );

        if first != second {
            return Err(format!(
                "merge_issue was not deterministic for strategy {strategy:?}: first={first:?} second={second:?}"
            ));
        }

        assert_merge_result_invariants(&first, &case)?;
        assert_validator_on_output(&first)?;
        assert_no_silent_conflict_resolution(&case, strategy, &first)?;
        assert_uncontested_changes_preserved(&case, &first)?;
        assert_three_way_merge_invariants(&case, strategy)?;
    }

    Ok(())
}

fn parse_json_case(data: &[u8]) -> Option<MergeFuzzCase> {
    let json: JsonCase = serde_json::from_slice(data).ok()?;
    let default_id = clean_id(json.id.as_deref().unwrap_or("bd-json-seed"));
    let base = json
        .base
        .as_ref()
        .map(|spec| issue_from_json_spec(&default_id, spec, 0));
    let local = json
        .local
        .as_ref()
        .map(|spec| issue_from_json_spec(&default_id, spec, 10));
    let external = json
        .external
        .as_ref()
        .map(|spec| issue_from_json_spec(&default_id, spec, 20));

    Some(ensure_nonempty_case(MergeFuzzCase {
        base,
        local,
        external,
        tombstone_protected: json.tombstone_protected.unwrap_or(false),
    }))
}

fn issue_from_json_spec(default_id: &str, spec: &JsonIssue, time_salt: i64) -> Issue {
    let updated_at = fixed_time(spec.updated_at.unwrap_or(1_700_000_000 + time_salt));
    let created_at = updated_at - Duration::seconds(60);
    let title = non_empty(
        spec.title
            .as_deref()
            .map(|value| truncate(value, MAX_FIELD_BYTES))
            .unwrap_or_else(|| format!("seed issue {time_salt}")),
        "seed issue",
    );

    let mut issue = Issue {
        id: clean_id(spec.id.as_deref().unwrap_or(default_id)),
        title,
        description: spec
            .description
            .as_deref()
            .map(|value| truncate(value, MAX_FIELD_BYTES)),
        status: parse_status(spec.status.as_deref()),
        priority: Priority(spec.priority.unwrap_or(2).clamp(0, 4)),
        issue_type: parse_issue_type(spec.issue_type.as_deref()),
        assignee: spec
            .assignee
            .as_deref()
            .map(|value| truncate(value, MAX_FIELD_BYTES)),
        created_at,
        updated_at,
        labels: spec
            .labels
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|label| truncate(&label, 48))
            .collect(),
        ..Issue::default()
    };

    issue.dependencies = spec
        .dependencies
        .clone()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(index, depends_on_id)| dependency(&issue.id, &clean_id(&depends_on_id), index))
        .collect();
    issue.comments = spec
        .comments
        .clone()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(index, body)| comment(&issue.id, i64::try_from(index + 1).unwrap_or(1), &body))
        .collect();

    normalize_issue(issue)
}

fn random_case(data: &[u8]) -> MergeFuzzCase {
    let mut cursor = ByteCursor::new(data);
    let id = format!(
        "{}-{:02x}{:02x}",
        cursor.prefix(),
        cursor.next_byte(),
        cursor.next_byte()
    );

    let base = if cursor.next_byte() % 5 == 0 {
        None
    } else {
        Some(random_issue(&mut cursor, &id, "base"))
    };
    let local = random_side(&mut cursor, base.as_ref(), &id, "local");
    let external = random_side(&mut cursor, base.as_ref(), &id, "external");

    ensure_nonempty_case(MergeFuzzCase {
        base,
        local,
        external,
        tombstone_protected: cursor.next_bool(),
    })
}

fn random_side(
    cursor: &mut ByteCursor<'_>,
    base: Option<&Issue>,
    id: &str,
    side: &str,
) -> Option<Issue> {
    match cursor.next_byte() % 5 {
        0 => None,
        1 => base
            .cloned()
            .map(|issue| maybe_change_id(cursor, issue, side)),
        _ => {
            let mut issue = base
                .cloned()
                .unwrap_or_else(|| random_issue(cursor, id, side));
            mutate_issue(cursor, &mut issue, side);
            Some(maybe_change_id(cursor, issue, side))
        }
    }
}

fn random_issue(cursor: &mut ByteCursor<'_>, id: &str, side: &str) -> Issue {
    let updated_at = fixed_time(1_700_000_000 + i64::from(cursor.next_u16()));
    let created_at = updated_at - Duration::seconds(i64::from(cursor.next_byte() % 120));
    let mut issue = Issue {
        id: clean_id(id),
        title: non_empty(cursor.text(MAX_FIELD_BYTES), &format!("{side} title")),
        description: cursor.optional_text(MAX_FIELD_BYTES),
        design: cursor.optional_text(MAX_FIELD_BYTES),
        acceptance_criteria: cursor.optional_text(MAX_FIELD_BYTES),
        notes: cursor.optional_text(MAX_FIELD_BYTES),
        status: cursor.status(),
        priority: Priority(i32::from(cursor.next_byte() % 5)),
        issue_type: cursor.issue_type(),
        assignee: cursor.optional_text(48),
        owner: cursor.optional_text(48),
        estimated_minutes: if cursor.next_bool() {
            Some(i32::from(cursor.next_byte()))
        } else {
            None
        },
        created_at,
        created_by: Some(format!("{side}-fuzzer")),
        updated_at,
        external_ref: cursor.optional_text(48),
        source_system: cursor.optional_text(48),
        source_repo: Some("fuzz".to_string()),
        pinned: cursor.next_bool(),
        is_template: cursor.next_bool(),
        labels: random_labels(cursor),
        ..Issue::default()
    };

    issue.dependencies = random_dependencies(cursor, &issue.id);
    issue.comments = random_comments(cursor, &issue.id);
    normalize_issue(issue)
}

fn maybe_change_id(cursor: &mut ByteCursor<'_>, mut issue: Issue, side: &str) -> Issue {
    if cursor.next_byte() % 17 == 0 {
        issue.id = format!("{side}-{}", cursor.next_byte());
    }
    normalize_issue(issue)
}

fn mutate_issue(cursor: &mut ByteCursor<'_>, issue: &mut Issue, side: &str) {
    let steps = 1 + usize::from(cursor.next_byte() % 4);
    for _ in 0..steps {
        match cursor.next_byte() % 11 {
            0 => issue.title = non_empty(cursor.text(MAX_FIELD_BYTES), &format!("{side} title")),
            1 => issue.description = cursor.optional_text(MAX_FIELD_BYTES),
            2 => issue.design = cursor.optional_text(MAX_FIELD_BYTES),
            3 => issue.acceptance_criteria = cursor.optional_text(MAX_FIELD_BYTES),
            4 => issue.notes = cursor.optional_text(MAX_FIELD_BYTES),
            5 => issue.status = cursor.status(),
            6 => issue.priority = Priority(i32::from(cursor.next_byte() % 5)),
            7 => issue.issue_type = cursor.issue_type(),
            8 => issue.labels.push(non_empty(cursor.text(32), "fuzz-label")),
            9 => issue.dependencies.push(dependency(
                &issue.id,
                &format!("external:fuzz:{}", cursor.next_byte()),
                0,
            )),
            _ => issue.comments.push(comment(
                &issue.id,
                i64::from(cursor.next_byte()),
                &cursor.text(64),
            )),
        }
    }

    issue.updated_at += Duration::seconds(1 + i64::from(cursor.next_byte()));
    *issue = normalize_issue(issue.clone());
}

fn ensure_nonempty_case(mut case: MergeFuzzCase) -> MergeFuzzCase {
    if case.base.is_none() && case.local.is_none() && case.external.is_none() {
        let mut cursor = ByteCursor::new(b"default-merge-case");
        case.local = Some(random_issue(&mut cursor, "bd-default", "local"));
    }
    case
}

fn assert_input_hashes_are_consistent(case: &MergeFuzzCase) -> Result<(), String> {
    for issue in [&case.base, &case.local, &case.external]
        .into_iter()
        .flatten()
    {
        assert_issue_invariants(issue)?;
    }
    Ok(())
}

fn assert_merge_result_invariants(
    result: &MergeResult,
    case: &MergeFuzzCase,
) -> Result<(), String> {
    match result {
        MergeResult::NoAction => {
            if case.base.is_some() || case.local.is_some() || case.external.is_some() {
                return Err(format!("NoAction returned for non-empty case: {case:?}"));
            }
        }
        MergeResult::Keep(issue) | MergeResult::KeepWithNote(issue, _) => {
            assert_issue_invariants(issue)?;
        }
        MergeResult::Delete | MergeResult::Conflict(_) => {}
    }
    Ok(())
}

fn assert_validator_on_output(result: &MergeResult) -> Result<(), String> {
    match result {
        MergeResult::Keep(issue) | MergeResult::KeepWithNote(issue, _) => {
            IssueValidator::validate(issue).map_err(|errors| {
                format!(
                    "merge output {} failed IssueValidator::validate: {errors:?}",
                    issue.id
                )
            })
        }
        _ => Ok(()),
    }
}

fn assert_issue_invariants(issue: &Issue) -> Result<(), String> {
    if issue.id.trim().is_empty() {
        return Err("kept issue has an empty id".to_string());
    }
    if issue.title.trim().is_empty() {
        return Err(format!("kept issue {} has an empty title", issue.id));
    }
    if !(0..=4).contains(&issue.priority.0) {
        return Err(format!(
            "kept issue {} has invalid priority {}",
            issue.id, issue.priority.0
        ));
    }

    let expected_hash = content_hash(issue);
    if issue.content_hash.as_deref() != Some(expected_hash.as_str()) {
        return Err(format!(
            "kept issue {} has stale content_hash {:?}; expected {}",
            issue.id, issue.content_hash, expected_hash
        ));
    }

    assert_unique_labels(issue)?;
    assert_unique_dependencies(issue)?;
    assert_unique_comments(issue)?;
    Ok(())
}

fn assert_unique_labels(issue: &Issue) -> Result<(), String> {
    let mut seen = HashSet::new();
    for label in &issue.labels {
        if label.trim().is_empty() {
            return Err(format!("issue {} has an empty label", issue.id));
        }
        if !seen.insert(label) {
            return Err(format!("issue {} has duplicate label {label}", issue.id));
        }
    }
    Ok(())
}

fn assert_unique_dependencies(issue: &Issue) -> Result<(), String> {
    let mut seen = HashSet::new();
    for dependency in &issue.dependencies {
        if dependency.issue_id != issue.id {
            return Err(format!(
                "dependency source {} disagrees with owning issue {}",
                dependency.issue_id, issue.id
            ));
        }
        if dependency.depends_on_id.trim().is_empty() {
            return Err(format!("issue {} has an empty dependency target", issue.id));
        }
        let key = (
            dependency.depends_on_id.as_str(),
            dependency.dep_type.as_str(),
        );
        if !seen.insert(key) {
            return Err(format!(
                "issue {} has duplicate dependency {:?}",
                issue.id, key
            ));
        }
    }
    Ok(())
}

fn assert_unique_comments(issue: &Issue) -> Result<(), String> {
    let mut seen = HashSet::new();
    for comment in &issue.comments {
        if comment.issue_id != issue.id {
            return Err(format!(
                "comment source {} disagrees with owning issue {}",
                comment.issue_id, issue.id
            ));
        }
        let key = (comment.id, comment.author.as_str(), comment.body.as_str());
        if !seen.insert(key) {
            return Err(format!(
                "issue {} has duplicate comment {:?}",
                issue.id, key
            ));
        }
    }
    Ok(())
}

fn assert_no_silent_conflict_resolution(
    case: &MergeFuzzCase,
    strategy: ConflictResolution,
    result: &MergeResult,
) -> Result<(), String> {
    let Some(conflict_type) = expected_manual_conflict(case) else {
        return Ok(());
    };

    match strategy {
        ConflictResolution::Manual => {
            if !matches!(result, MergeResult::Conflict(actual) if *actual == conflict_type) {
                return Err(format!(
                    "manual merge did not report {conflict_type:?}: {result:?}"
                ));
            }
        }
        ConflictResolution::PreferNewer => match conflict_type {
            ConflictType::AmbiguousBase => {
                return Err(
                    "common-ancestor merge unexpectedly reported export-anchor ambiguity"
                        .to_string(),
                );
            }
            ConflictType::BothModified | ConflictType::ConvergentCreation => {
                let expected = newer_issue(case.local.as_ref(), case.external.as_ref());
                assert_kept_matches(result, expected, "prefer-newer conflict winner")?;
                if !matches!(result, MergeResult::KeepWithNote(_, note) if !note.is_empty()) {
                    return Err(format!(
                        "prefer-newer conflict winner lacked an explanatory note: {result:?}"
                    ));
                }
            }
            ConflictType::DeleteVsModify => {
                let expected = case.local.as_ref().or(case.external.as_ref());
                assert_kept_matches(result, expected, "prefer-newer delete-vs-modify winner")?;
                if !matches!(result, MergeResult::KeepWithNote(_, note) if !note.is_empty()) {
                    return Err(format!(
                        "prefer-newer delete-vs-modify winner lacked an explanatory note: {result:?}"
                    ));
                }
            }
        },
        ConflictResolution::PreferLocal => {
            assert_force_winner(result, case.local.as_ref(), "force-db/local")?;
        }
        ConflictResolution::PreferExternal => {
            assert_force_winner(result, case.external.as_ref(), "force-jsonl/external")?;
        }
    }

    Ok(())
}

fn expected_manual_conflict(case: &MergeFuzzCase) -> Option<ConflictType> {
    match (&case.base, &case.local, &case.external) {
        (None, Some(local), Some(external)) if !local.sync_equals(external) => {
            Some(ConflictType::ConvergentCreation)
        }
        (Some(base), Some(local), None) if !local.sync_equals(base) => {
            Some(ConflictType::DeleteVsModify)
        }
        (Some(base), None, Some(external)) if !external.sync_equals(base) => {
            Some(ConflictType::DeleteVsModify)
        }
        (Some(base), Some(local), Some(external))
            if !local.sync_equals(external)
                && !local.sync_equals(base)
                && !external.sync_equals(base) =>
        {
            Some(ConflictType::BothModified)
        }
        _ => None,
    }
}

fn assert_uncontested_changes_preserved(
    case: &MergeFuzzCase,
    result: &MergeResult,
) -> Result<(), String> {
    match (&case.base, &case.local, &case.external) {
        (Some(base), Some(local), Some(external))
            if !local.sync_equals(base) && external.sync_equals(base) =>
        {
            assert_kept_matches(result, Some(local), "local-only change")
        }
        (Some(base), Some(local), Some(external))
            if local.sync_equals(base) && !external.sync_equals(base) =>
        {
            assert_kept_matches(result, Some(external), "external-only change")
        }
        (Some(base), Some(local), None) if local.sync_equals(base) => {
            assert_deleted(result, "external deletion of unchanged local")
        }
        (Some(base), None, Some(external)) if external.sync_equals(base) => {
            assert_deleted(result, "local deletion of unchanged external")
        }
        _ => Ok(()),
    }
}

fn assert_three_way_merge_invariants(
    case: &MergeFuzzCase,
    strategy: ConflictResolution,
) -> Result<(), String> {
    let context = MergeContext::new(
        map_issue(case.base.clone()),
        map_issue(case.local.clone()),
        map_issue(case.external.clone()),
    );
    let tombstones = tombstone_ids(case);
    let report = if case.tombstone_protected {
        three_way_merge(&context, strategy, Some(&tombstones))
    } else {
        three_way_merge(&context, strategy, None)
    };

    assert_merge_report_invariants(&report)?;
    if case.tombstone_protected {
        assert_no_tombstone_resurrection(case, &report)?;
    }
    Ok(())
}

fn assert_merge_report_invariants(report: &MergeReport) -> Result<(), String> {
    let mut kept_ids = HashSet::new();
    for issue in &report.kept {
        assert_issue_invariants(issue)?;
        IssueValidator::validate(issue).map_err(|errors| {
            format!(
                "three_way_merge kept issue {} failed IssueValidator::validate: {errors:?}",
                issue.id
            )
        })?;
        if !kept_ids.insert(issue.id.as_str()) {
            return Err(format!("three_way_merge kept duplicate issue {}", issue.id));
        }
    }

    assert_unique_strings(&report.deleted, "deleted issue")?;
    let conflict_ids: Vec<String> = report.conflicts.iter().map(|(id, _)| id.clone()).collect();
    assert_unique_strings(&conflict_ids, "conflict issue")?;
    assert_unique_strings(&report.tombstone_protected, "tombstone-protected issue")?;
    Ok(())
}

fn assert_no_tombstone_resurrection(
    case: &MergeFuzzCase,
    report: &MergeReport,
) -> Result<(), String> {
    let Some(local) = &case.local else {
        return Ok(());
    };
    let Some(external) = &case.external else {
        return Ok(());
    };
    if local.id != external.id
        || local.status != Status::Tombstone
        || external.status == Status::Tombstone
    {
        return Ok(());
    }

    if report
        .kept
        .iter()
        .any(|issue| issue.id == local.id && issue.status != Status::Tombstone)
    {
        return Err(format!(
            "three_way_merge resurrected tombstone {} from external side",
            local.id
        ));
    }

    Ok(())
}

fn assert_force_winner(
    result: &MergeResult,
    expected: Option<&Issue>,
    context: &str,
) -> Result<(), String> {
    match expected {
        Some(issue) => assert_kept_matches(result, Some(issue), context),
        None => assert_deleted(result, context),
    }
}

fn assert_kept_matches(
    result: &MergeResult,
    expected: Option<&Issue>,
    context: &str,
) -> Result<(), String> {
    let Some(expected) = expected else {
        return assert_deleted(result, context);
    };

    let kept = match result {
        MergeResult::Keep(issue) | MergeResult::KeepWithNote(issue, _) => issue,
        other => {
            return Err(format!(
                "{context}: expected kept issue {}, got {other:?}",
                expected.id
            ));
        }
    };

    if !kept.sync_equals(expected) || kept.updated_at != expected.updated_at {
        return Err(format!(
            "{context}: kept issue did not match expected side; kept={kept:?} expected={expected:?}"
        ));
    }
    Ok(())
}

fn assert_deleted(result: &MergeResult, context: &str) -> Result<(), String> {
    if matches!(result, MergeResult::Delete) {
        Ok(())
    } else {
        Err(format!("{context}: expected Delete, got {result:?}"))
    }
}

fn newer_issue<'a>(local: Option<&'a Issue>, external: Option<&'a Issue>) -> Option<&'a Issue> {
    match (local, external) {
        (Some(local), Some(external)) if external.updated_at > local.updated_at => Some(external),
        (Some(local), Some(_)) => Some(local),
        (Some(local), None) => Some(local),
        (None, Some(external)) => Some(external),
        (None, None) => None,
    }
}

fn map_issue(issue: Option<Issue>) -> HashMap<String, Issue> {
    issue
        .map(|issue| HashMap::from([(issue.id.clone(), issue)]))
        .unwrap_or_default()
}

fn tombstone_ids(case: &MergeFuzzCase) -> HashSet<String> {
    [&case.base, &case.local, &case.external]
        .into_iter()
        .flatten()
        .filter(|issue| issue.status == Status::Tombstone)
        .map(|issue| issue.id.clone())
        .collect()
}

fn assert_unique_strings(values: &[String], context: &str) -> Result<(), String> {
    let mut seen = HashSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(format!("duplicate {context}: {value}"));
        }
    }
    Ok(())
}

fn merge_strategies() -> [ConflictResolution; 4] {
    [
        ConflictResolution::PreferLocal,
        ConflictResolution::PreferExternal,
        ConflictResolution::PreferNewer,
        ConflictResolution::Manual,
    ]
}

fn normalize_issue(mut issue: Issue) -> Issue {
    issue.id = valid_issue_id(&clean_id(&issue.id));
    issue.title = non_empty(truncate(&issue.title, MAX_FIELD_BYTES), "fuzz-title");
    issue.priority = Priority(issue.priority.0.clamp(0, 4));
    normalize_status_fields(&mut issue);
    normalize_relations(&mut issue);
    sanitize_external_ref(&mut issue);
    issue.content_hash = Some(content_hash(&issue));
    issue
}

fn valid_issue_id(raw: &str) -> String {
    let lower: String = raw
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect();
    let trimmed = lower.trim_matches('-');
    if trimmed.is_empty() || !trimmed.contains('-') {
        "bd-fuzz".to_string()
    } else {
        trimmed.to_string()
    }
}

fn sanitize_external_ref(issue: &mut Issue) {
    if let Some(ext_ref) = &mut issue.external_ref {
        ext_ref.retain(|c| !c.is_whitespace());
        if ext_ref.is_empty() || ext_ref.len() > 200 {
            issue.external_ref = None;
        }
    }
}

fn normalize_status_fields(issue: &mut Issue) {
    match issue.status {
        Status::Closed => {
            issue.closed_at.get_or_insert(issue.updated_at);
            issue
                .close_reason
                .get_or_insert_with(|| "fuzz close".to_string());
        }
        Status::Tombstone => {
            issue.deleted_at.get_or_insert(issue.updated_at);
            issue
                .deleted_by
                .get_or_insert_with(|| "merge-fuzzer".to_string());
            issue
                .delete_reason
                .get_or_insert_with(|| "fuzz tombstone".to_string());
            issue
                .original_type
                .get_or_insert_with(|| issue.issue_type.as_str().to_string());
        }
        _ => {
            issue.closed_at = None;
            issue.close_reason = None;
        }
    }
}

fn normalize_relations(issue: &mut Issue) {
    issue.labels = unique_nonempty(issue.labels.drain(..).collect());

    let mut dependency_keys = HashSet::new();
    issue.dependencies.retain_mut(|dependency| {
        dependency.issue_id.clone_from(&issue.id);
        dependency.depends_on_id = clean_id(&dependency.depends_on_id);
        !dependency.depends_on_id.is_empty()
            && dependency_keys.insert((
                dependency.depends_on_id.clone(),
                dependency.dep_type.as_str().to_string(),
            ))
    });

    let mut comment_keys = HashSet::new();
    issue.comments.retain_mut(|comment| {
        comment.issue_id.clone_from(&issue.id);
        !comment.body.is_empty()
            && comment_keys.insert((comment.id, comment.author.clone(), comment.body.clone()))
    });
}

fn unique_nonempty(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for value in values {
        let value = truncate(&value, 48);
        if !value.trim().is_empty() && seen.insert(value.clone()) {
            unique.push(value);
        }
    }
    unique.sort();
    unique
}

fn random_labels(cursor: &mut ByteCursor<'_>) -> Vec<String> {
    let count = usize::from(cursor.next_byte() % 4);
    let mut labels = Vec::with_capacity(count);
    for _ in 0..count {
        labels.push(cursor.text(32));
    }
    unique_nonempty(labels)
}

fn random_dependencies(cursor: &mut ByteCursor<'_>, issue_id: &str) -> Vec<Dependency> {
    let count = usize::from(cursor.next_byte() % 3);
    let mut dependencies = Vec::with_capacity(count);
    for index in 0..count {
        let target = if cursor.next_bool() {
            format!("external:fuzz:{}", cursor.next_byte())
        } else {
            format!("bd-target-{}", cursor.next_byte())
        };
        dependencies.push(dependency(issue_id, &target, index));
    }
    dependencies
}

fn random_comments(cursor: &mut ByteCursor<'_>, issue_id: &str) -> Vec<Comment> {
    let count = usize::from(cursor.next_byte() % 3);
    let mut comments = Vec::with_capacity(count);
    for index in 0..count {
        comments.push(comment(
            issue_id,
            i64::try_from(index + 1).unwrap_or(1),
            &cursor.text(64),
        ));
    }
    comments
}

fn dependency(issue_id: &str, depends_on_id: &str, index: usize) -> Dependency {
    Dependency {
        issue_id: issue_id.to_string(),
        depends_on_id: clean_id(depends_on_id),
        dep_type: if index.is_multiple_of(2) {
            DependencyType::Blocks
        } else {
            DependencyType::Related
        },
        created_at: fixed_time(1_700_000_000 + i64::try_from(index).unwrap_or(0)),
        created_by: Some("merge-fuzzer".to_string()),
        metadata: None,
        thread_id: Some(format!("fuzz-thread-{index}")),
    }
}

fn comment(issue_id: &str, id: i64, body: &str) -> Comment {
    Comment {
        id: id.max(1),
        issue_id: issue_id.to_string(),
        author: "merge-fuzzer".to_string(),
        body: non_empty(truncate(body, MAX_FIELD_BYTES), "fuzz comment"),
        created_at: fixed_time(1_700_000_000 + id.max(1)),
    }
}

fn fixed_time(seconds: i64) -> DateTime<Utc> {
    let seconds = seconds.clamp(0, 4_102_444_800);
    DateTime::from_timestamp(seconds, 0).expect("bounded timestamp is valid")
}

fn parse_status(value: Option<&str>) -> Status {
    match value.unwrap_or("open").to_ascii_lowercase().as_str() {
        "open" => Status::Open,
        "in_progress" | "inprogress" => Status::InProgress,
        "blocked" => Status::Blocked,
        "deferred" => Status::Deferred,
        "draft" => Status::Draft,
        "closed" => Status::Closed,
        "tombstone" => Status::Tombstone,
        "pinned" => Status::Pinned,
        other => Status::Custom(non_empty(truncate(other, 32), "custom-status")),
    }
}

fn parse_issue_type(value: Option<&str>) -> IssueType {
    match value.unwrap_or("task").to_ascii_lowercase().as_str() {
        "task" => IssueType::Task,
        "bug" => IssueType::Bug,
        "feature" => IssueType::Feature,
        "epic" => IssueType::Epic,
        "chore" => IssueType::Chore,
        "docs" => IssueType::Docs,
        "question" => IssueType::Question,
        other => IssueType::Custom(non_empty(truncate(other, 32), "custom-type")),
    }
}

fn clean_id(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.'))
        .take(64)
        .collect();
    non_empty(cleaned, "bd-fuzz")
}

fn truncate(value: &str, max_len: usize) -> String {
    value.chars().take(max_len).collect()
}

fn non_empty(value: String, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value
    }
}
