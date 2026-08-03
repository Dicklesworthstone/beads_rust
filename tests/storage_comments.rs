//! Storage-level tests for the comment substrate behind `bd comments`.
//!
//! Three properties matter here and each has its own failure mode:
//!
//! 1. Appending is an insert, so it can never shorten an existing log. This
//!    is the whole reason the command exists instead of teaching `--notes`
//!    to append: appending to a scalar field is a read-modify-write, and a
//!    short read silently destroys the tail on write-back.
//! 2. Listings need counts and recency in one batched query, and must never
//!    carry bodies.
//! 3. Search must reach comment text. Otherwise moving an annotation out of
//!    `notes` and into a comment trades a truncation bug for an amnesia bug.

mod common;

use beads_rust::storage::{IssueUpdate, ListFilters, SqliteStorage};
use common::{fixtures, test_db};

fn issue_with_id(storage: &mut SqliteStorage, id: &str, title: &str) -> String {
    let mut issue = fixtures::issue(title);
    issue.id = id.to_string();
    storage.create_issue(&issue, "tester").unwrap();
    id.to_string()
}

// ============================================================================
// APPEND SEMANTICS
// ============================================================================

/// The core safety property: a long body followed by an append yields both,
/// intact. A read-modify-write on a scalar field can lose the first; an
/// insert cannot.
#[test]
fn appending_never_shortens_an_existing_comment() {
    let mut storage = test_db();
    let id = issue_with_id(&mut storage, "test-append", "Append safety");

    let long_body = "x".repeat(40 * 1024);
    storage.add_comment(&id, "alice", &long_body).unwrap();
    storage.add_comment(&id, "bob", "a short follow-up").unwrap();

    let comments = storage.get_comments(&id).unwrap();
    assert_eq!(comments.len(), 2);
    assert_eq!(comments[0].body.len(), long_body.len());
    assert_eq!(comments[1].body, "a short follow-up");
}

/// Chronological order, and attribution per entry rather than per issue:
/// "who said this, and when" is the information `notes` cannot hold.
#[test]
fn comments_are_returned_chronologically_with_authors() {
    let mut storage = test_db();
    let id = issue_with_id(&mut storage, "test-order", "Ordering");

    for (author, body) in [
        ("planner", "picked option A"),
        ("coder", "implemented option A"),
        ("reviewer", "option A looks right"),
    ] {
        storage.add_comment(&id, author, body).unwrap();
    }

    let comments = storage.get_comments(&id).unwrap();
    let authors: Vec<&str> = comments.iter().map(|c| c.author.as_str()).collect();
    assert_eq!(authors, vec!["planner", "coder", "reviewer"]);
    assert!(comments.windows(2).all(|w| w[0].created_at <= w[1].created_at));
}

// ============================================================================
// LISTING STATS
// ============================================================================

#[test]
fn comment_stats_report_count_and_newest_timestamp() {
    let mut storage = test_db();
    let busy = issue_with_id(&mut storage, "test-busy", "Busy");
    let quiet = issue_with_id(&mut storage, "test-quiet", "Quiet");
    let silent = issue_with_id(&mut storage, "test-silent", "Silent");

    for n in 0..4 {
        storage.add_comment(&busy, "alice", &format!("note {n}")).unwrap();
    }
    storage.add_comment(&quiet, "bob", "only one").unwrap();

    let ids = vec![busy.clone(), quiet.clone(), silent.clone()];
    let stats = storage.comment_stats_for_issues(&ids).unwrap();

    assert_eq!(stats.get(&busy).unwrap().count, 4);
    assert_eq!(stats.get(&quiet).unwrap().count, 1);
    // Issues with no history are absent rather than zero-valued: a listing
    // then says nothing at all about them instead of printing a zero on
    // every row.
    assert!(!stats.contains_key(&silent));

    let newest = storage.get_comments(&busy).unwrap().last().unwrap().created_at;
    assert_eq!(stats.get(&busy).unwrap().last_comment_at, Some(newest));
}

#[test]
fn comment_stats_for_no_issues_is_empty() {
    let storage = test_db();
    assert!(storage.comment_stats_for_issues(&[]).unwrap().is_empty());
}

// ============================================================================
// SEARCH REACHES COMMENT TEXT
// ============================================================================

/// The blocking requirement: an annotation living in a comment is findable.
#[test]
fn search_finds_issues_by_comment_text() {
    let mut storage = test_db();
    let target = issue_with_id(&mut storage, "test-found", "Nothing telling in the title");
    let other = issue_with_id(&mut storage, "test-other", "Unrelated");

    storage
        .add_comment(&target, "planner", "OPTION A authorized, proceed")
        .unwrap();
    storage.add_comment(&other, "planner", "unrelated chatter").unwrap();

    let hits = storage
        .search_issues("authorized", &ListFilters::default())
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, vec![target.as_str()]);
}

/// A query that matches nothing anywhere still matches nothing: widening
/// search to comments must not make it indiscriminate.
#[test]
fn search_still_misses_what_is_written_nowhere() {
    let mut storage = test_db();
    let id = issue_with_id(&mut storage, "test-miss", "Title");
    storage.add_comment(&id, "alice", "some commentary").unwrap();

    let hits = storage
        .search_issues("nonexistent-phrase", &ListFilters::default())
        .unwrap();
    assert!(hits.is_empty());
}

/// An issue is returned once even if several of its comments match — a hit
/// list is a list of issues, not of matches.
#[test]
fn search_does_not_duplicate_issues_with_several_matching_comments() {
    let mut storage = test_db();
    let id = issue_with_id(&mut storage, "test-dupes", "Title");
    for n in 0..3 {
        storage
            .add_comment(&id, "alice", &format!("decision {n}: ship it"))
            .unwrap();
    }

    let hits = storage.search_issues("ship it", &ListFilters::default()).unwrap();
    assert_eq!(hits.len(), 1);
}

/// Filters still apply to comment-sourced hits: a comment match does not
/// smuggle a closed issue into a search scoped to open ones.
#[test]
fn comment_matches_respect_status_filters() {
    let mut storage = test_db();
    let id = issue_with_id(&mut storage, "test-filtered", "Title");
    storage.add_comment(&id, "alice", "distinctive-token").unwrap();
    storage
        .update_issue(
            &id,
            &IssueUpdate {
                status: Some(beads_rust::model::Status::Closed),
                ..IssueUpdate::default()
            },
            "tester",
        )
        .unwrap();

    let filters = ListFilters {
        statuses: Some(vec![beads_rust::model::Status::Open]),
        ..ListFilters::default()
    };
    let hits = storage.search_issues("distinctive-token", &filters).unwrap();
    assert!(hits.is_empty());
}

/// LIKE metacharacters in the query are escaped, so searching for `100%`
/// does not degenerate into "match anything".
#[test]
fn comment_search_escapes_like_wildcards() {
    let mut storage = test_db();
    let literal = issue_with_id(&mut storage, "test-literal", "Literal");
    let decoy = issue_with_id(&mut storage, "test-decoy", "Decoy");
    storage.add_comment(&literal, "alice", "coverage is 100% now").unwrap();
    storage.add_comment(&decoy, "bob", "coverage is fine").unwrap();

    let hits = storage.search_issues("100%", &ListFilters::default()).unwrap();
    let ids: Vec<&str> = hits.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, vec![literal.as_str()]);
}

// ============================================================================
// MATCHING-COMMENT LOOKUP (search snippets)
// ============================================================================

#[test]
fn find_matching_comments_returns_the_newest_match_per_issue() {
    let mut storage = test_db();
    let id = issue_with_id(&mut storage, "test-newest", "Title");
    storage.add_comment(&id, "alice", "token: first").unwrap();
    storage.add_comment(&id, "bob", "no token here at all").unwrap();
    storage.add_comment(&id, "carol", "token: latest").unwrap();

    let matches = storage
        .find_matching_comments("token:", std::slice::from_ref(&id))
        .unwrap();
    let hit = matches.get(&id).expect("issue has a matching comment");
    assert_eq!(hit.body, "token: latest");
    // Attribution comes along, because "who said this" is the point.
    assert_eq!(hit.author, "carol");
}

#[test]
fn find_matching_comments_omits_issues_without_a_match() {
    let mut storage = test_db();
    let id = issue_with_id(&mut storage, "test-nomatch", "Title");
    storage.add_comment(&id, "alice", "nothing relevant").unwrap();

    let matches = storage.find_matching_comments("absent", std::slice::from_ref(&id)).unwrap();
    assert!(matches.is_empty());
}

#[test]
fn find_matching_comments_is_empty_for_blank_query_or_no_issues() {
    let mut storage = test_db();
    let id = issue_with_id(&mut storage, "test-blank", "Title");
    storage.add_comment(&id, "alice", "something").unwrap();

    assert!(
        storage
            .find_matching_comments("   ", std::slice::from_ref(&id))
            .unwrap()
            .is_empty()
    );
    assert!(storage.find_matching_comments("something", &[]).unwrap().is_empty());
}
