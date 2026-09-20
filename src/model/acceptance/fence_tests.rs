//! Code examples are neither editable requirements nor completion evidence.

use super::*;
use crate::close_policy::{Workflow, evaluate_transition_required_fields};

const MISLEADING_CLOSE: &str = "```markdown\n\
```not-a-close\n\
- [x] finished example\n\
```\n\
- [ ] unfinished prerequisite\n";

fn item_texts(checklist: &AcceptanceChecklist) -> Vec<String> {
    checklist
        .items()
        .into_iter()
        .map(|item| item.text)
        .collect()
}

fn prerequisite_violations(body: &str) -> Vec<crate::close_policy::PolicyViolation> {
    let workflow: Workflow =
        serde_yml::from_str("required_fields:\n  handoff: [prerequisites_complete]\n").unwrap();
    workflow.validate_required_fields().unwrap();
    evaluate_transition_required_fields(
        &workflow,
        "bd-fence",
        Some("planning"),
        "handoff",
        None,
        Some(body),
        None,
    )
}

#[test]
fn long_fences_keep_embedded_checkboxes_out_of_item_indexes() {
    let body = "````markdown\r\n```rust\r\n- [ ] example only\r\n```\r\n````\r\n  * [\u{a0}] Réal requirement\r\n";
    let checklist = AcceptanceChecklist::parse(body);
    assert_eq!(item_texts(&checklist), ["Réal requirement"]);
    assert_eq!(checklist.items()[0].index, 1);
    let edit = checklist.edit(&[1], &[], &[]).unwrap();
    assert_eq!(edit.body, body.replacen("[\u{a0}] Réal", "[x] Réal", 1));
    assert!(edit.body.contains("- [ ] example only\r\n"));
    assert_eq!(
        checklist.body(),
        body,
        "parsing and editing do not mutate the source"
    );
}

#[test]
fn only_matching_bare_closers_end_fences() {
    for marker in ["`", "~"] {
        for width in 3..=7 {
            let fence = marker.repeat(width);
            let other = if marker == "`" { "~" } else { "`" };
            for candidate in [
                marker.repeat(width - 1),
                other.repeat(width + 2),
                format!("{fence} language"),
                format!("{fence} trailing`"),
                format!("{fence}\u{a0}"),
            ] {
                for newline in ["\n", "\r\n"] {
                    let body = format!(
                        "{fence}markdown{newline}{candidate}{newline}- [ ] example{newline}{} \t{newline}- [ ] real{newline}",
                        marker.repeat(width + 2)
                    );
                    let checklist = AcceptanceChecklist::parse(&body);
                    assert_eq!(item_texts(&checklist), ["real"], "{body:?}");
                    assert!(!checklist.ends_in_open_fence, "{body:?}");
                }
            }
        }
    }
}

#[test]
fn invalid_backtick_info_strings_do_not_hide_real_checklists() {
    for opener in ["``` inline `code`", "```` info```", "  ``` `"] {
        let body = format!("{opener}\n- [ ] real\n");
        let checklist = AcceptanceChecklist::parse(&body);
        assert_eq!(item_texts(&checklist), ["real"], "{body:?}");
        let edit = checklist.edit(&[], &[], &["new item".into()]).unwrap();
        assert_eq!(edit.added, [2]);
        assert_eq!(edit.body, format!("{opener}\n- [ ] real\n- [ ] new item\n"));
    }
    // The backtick restriction belongs to backtick openers only.
    let tilde =
        AcceptanceChecklist::parse("~~~ language `code` ~~~\n- [x] example\n~~~\n- [ ] real");
    assert_eq!(item_texts(&tilde), ["real"]);
}

#[test]
fn unclosed_fences_refuse_appends_without_changing_other_markers() {
    for ending in ["```", "````language", "~~~~", "````\u{a0}"] {
        let body = format!("- [ ] existing\n````markdown\n{ending}\n- [x] example\n");
        let checklist = AcceptanceChecklist::parse(&body);
        assert_eq!(item_texts(&checklist), ["existing"]);
        assert!(checklist.ends_in_open_fence, "{body:?}");
        let error = plan_acceptance_edit(&body, &["1".into()], &[], &["new item".into()])
            .expect_err("combined check and unsafe append must be rejected as a whole");
        assert_eq!(error.field, "acceptance");
        assert!(error.reason.contains("unclosed code fence"));
        assert_eq!(checklist.body(), body);
        let check_only = plan_acceptance_edit(&body, &["1".into()], &[], &[]).unwrap();
        assert_eq!(
            check_only.body,
            body.replacen("[ ] existing", "[x] existing", 1)
        );
    }
}

#[test]
fn valid_long_closers_allow_appends_and_keep_trailing_bytes() {
    for marker in ["`", "~"] {
        let body = format!(
            "* [X] existing\r\n{}markdown\r\n- [ ] example\r\n{} \t\r\n\r\n",
            marker.repeat(4),
            marker.repeat(6)
        );
        let edit = plan_acceptance_edit(&body, &[], &[], &["Réal new item".into()]).unwrap();
        assert_eq!(edit.added, [2]);
        let prefix = body.strip_suffix("\r\n\r\n").unwrap();
        assert_eq!(
            edit.body,
            format!("{prefix}\r\n* [ ] Réal new item\r\n\r\n")
        );
        let output = AcceptanceCriteriaOutput::from_edit(&edit);
        assert_eq!(
            (output.total, output.checked_count, output.remaining),
            (2, 1, 1)
        );
    }
}

#[test]
fn selectors_and_summaries_use_real_items_only() {
    let body =
        "````markdown\n```\n- [ ] Same text\n- [x] hidden only\n```\n````\n- [ ] Same text\n";
    let edit = plan_acceptance_edit(body, &["Same text".into()], &[], &[]).unwrap();
    assert_eq!(edit.checked, [1]);
    assert_eq!(
        edit.body,
        body.replacen("````\n- [ ] Same text", "````\n- [x] Same text", 1)
    );
    let output = AcceptanceCriteriaOutput::from_edit(&edit);
    assert_eq!(
        (output.total, output.checked_count, output.remaining),
        (1, 1, 0)
    );
    let error = plan_acceptance_edit(body, &["1".into(), "hidden only".into()], &[], &[])
        .expect_err("a selector for a fenced example cannot cause a partial edit");
    assert_eq!(error.field, "check-acceptance");
    assert!(error.reason.contains("no acceptance item matches"));
    let unchecked = plan_acceptance_edit(&edit.body, &[], &["1".into()], &[]).unwrap();
    assert_eq!(unchecked.body, body);
}

#[test]
fn prerequisite_gate_cannot_use_fenced_examples_to_hide_unfinished_work() {
    let checklist = AcceptanceChecklist::parse(MISLEADING_CLOSE);
    assert_eq!(item_texts(&checklist), ["unfinished prerequisite"]);
    assert_eq!(checklist.checked_count(), 0);
    let violations = prerequisite_violations(MISLEADING_CLOSE);
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert_eq!(violations[0].gate, "transition_prerequisites_incomplete");
}

#[test]
fn prerequisite_gate_rejects_only_examples_and_accepts_real_completion() {
    let examples_only = "````markdown\n```\n- [x] example\n```\n````\n";
    assert!(AcceptanceChecklist::parse(examples_only).is_empty());
    let violations = prerequisite_violations(examples_only);
    assert_eq!(
        violations.len(),
        1,
        "examples are not a nonempty completed checklist"
    );
    assert_eq!(violations[0].gate, "transition_prerequisites_incomplete");
    let complete =
        MISLEADING_CLOSE.replace("[ ] unfinished prerequisite", "[x] unfinished prerequisite");
    assert!(prerequisite_violations(&complete).is_empty());
    // An unchecked example must not turn genuine completed work into a refusal.
    let with_unchecked_example =
        format!("{examples_only}- [x] real prerequisite\n").replace("[x] example", "[ ] example");
    assert!(prerequisite_violations(&with_unchecked_example).is_empty());
}
