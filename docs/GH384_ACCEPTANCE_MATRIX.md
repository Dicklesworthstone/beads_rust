# GitHub #384 Acceptance Matrix

> Atomic workflow capacity limits and agent admission control — every
> acceptance bullet from the issue mapped to the test(s) that prove it.
>
> Phases: 1-2 core limits/admission/warnings, 3 hierarchy counting,
> 4 audited exemptions, 5 multi-agent scopes, 6 observability (this doc).
> Beads: `beads_rust-8nbk.1` … `beads_rust-8nbk.6`.
>
> Maintained by hand; `gh384_acceptance_matrix_names_real_tests` (in
> `tests/e2e_workflow_capacity_scopes.rs`) fails if any backticked test
> name below stops existing, so renames must update this file.

| # | Acceptance criterion | Proven by |
|---|----------------------|-----------|
| 1 | Capacity supports arbitrary statuses declared by a custom workflow | `workflow_capacity_group_composes_multiple_custom_statuses` |
| 2 | Policy rejects capacity definitions for undeclared statuses | `capacity_rejects_undeclared_status_and_inverted_thresholds` |
| 3 | A transition reaching exactly the hard limit succeeds | `workflow_capacity_create_reaches_hard_limit_then_rolls_back_next_insert` |
| 4 | The next transition fails without modifying issue state | `workflow_capacity_rejected_update_preserves_original_issue`, `e2e_workflow_capacity_rejection_is_structured_and_atomic` |
| 5 | Two concurrent transitions competing for one slot produce exactly one success | `workflow_capacity_concurrent_last_slot_has_exactly_one_winner` |
| 6 | Leaving an over-cap status succeeds | `workflow_capacity_allows_transitions_that_drain_an_overfull_status` |
| 7 | Same-status updates do not affect capacity | `workflow_capacity_same_status_update_does_not_affect_capacity` |
| 8 | Batch transitions exceeding capacity are wholly rejected | `workflow_capacity_atomic_batch_rejection_rolls_back_every_issue_and_field`, `e2e_workflow_capacity_batch_rejection_rolls_back_every_issue` |
| 9 | Soft-limit transitions succeed with a warning | `workflow_capacity_soft_warnings_are_structured_deterministic_and_consumed`, `e2e_workflow_capacity_soft_limit_emits_structured_batch_warning` |
| 10 | Group limits apply even when the target status has remaining capacity | `workflow_capacity_group_composes_multiple_custom_statuses` |
| 11 | Every applicable repository/actor/harness/session/subtree scope composes correctly | `capacity_scope_repository_entry_composes_with_top_level_limits`, `capacity_scope_actor_limits_each_actor_partition_independently`, `capacity_scope_harness_and_session_key_on_attribution_and_skip_when_absent`, `capacity_scope_subtree_counts_by_root_ancestor`, `capacity_scope_assignee_keys_on_prospective_assignee_and_skips_unassigned` |
| 12 | Active parent plus active child consumes one slot under `leaf_work` | `capacity_leaf_work_counts_the_github_384_example_as_two_slots` |
| 13 | A multi-level hierarchy counts active leaves exactly once | `capacity_leaf_work_counts_the_github_384_example_as_two_slots`, `capacity_hierarchy_counts_every_member_of_a_dependency_cycle` |
| 14 | A parent begins counting after its final active descendant leaves the group | `capacity_leaf_work_starts_counting_a_parent_when_its_last_child_leaves` |
| 15 | An explicitly independent parent counts in addition to its children | `capacity_weighted_applies_issue_and_type_weights` |
| 16 | `blocks` and `related` dependencies do not affect hierarchy accounting | `capacity_leaf_work_ignores_blocks_edges` |
| 17 | Specific approved exemptions affect only their named issue and capacity | `capacity_exemption_admits_beyond_hard_limit_and_separates_counted_exempt_totals`, `e2e_capacity_exemption_lifecycle_admits_reports_and_revokes` |
| 18 | Unauthorized, expired, or reasonless exemptions fail | `capacity_exemption_grant_enforces_expiry_policy`, `capacity_exemption_expires_lazily_with_audited_record_and_counts_again`, `capacity_exemption_effect_is_withdrawn_when_provider_leaves_policy` |
| 19 | Counted, aggregate-excluded, and exempt totals are separately observable | `e2e_capacity_observability_in_stats_and_coordination`, `capacity_exemption_under_leaf_work_excludes_only_counting_members`, `e2e_workflow_capacity_leaf_work_excludes_aggregate_parents_and_reports_rollup` |
| 20 | Human failures identify the rejecting capacity, scope, counts, policy path, and remediation hint | `e2e_workflow_capacity_rejection_is_structured_and_atomic`, `capacity_scope_evidence_display_names_the_partition_key` |
| 21 | JSON/TOON failures provide stable structured fields | `workflow_capacity_error_preserves_machine_readable_evidence`, `workflow_capacity_error_reports_hierarchy_counting_evidence`, `e2e_workflow_capacity_create_preserves_legacy_shape_until_warning_exists` |
| 22 | Concurrent claims cannot exceed actor or harness limits | `capacity_scope_actor_limits_each_actor_partition_independently`, `e2e_capacity_scope_actor_partitions_admission_with_structured_evidence`, `workflow_capacity_concurrent_last_slot_has_exactly_one_winner` |
| 23 | A configured fresh-work guard rejects `open -> in_progress` when the observed queue is at its threshold | `workflow_capacity_admission_blocks_fresh_work_but_allows_rework_transition` |
| 24 | That rejection leaves the issue unchanged and identifies the downstream queue that must be drained | `workflow_capacity_admission_blocks_fresh_work_but_allows_rework_transition`, `e2e_workflow_capacity_rejection_is_structured_and_atomic` |
| 25 | A source-scoped fresh-work guard does not reject `rework -> in_progress` merely because fresh backlog admission is paused | `workflow_capacity_admission_blocks_fresh_work_but_allows_rework_transition` |
| 26 | Concurrent fresh-work claims cannot bypass a downstream backlog guard | `workflow_capacity_batch_preflight_allows_admission_before_matching_drain`, `workflow_capacity_concurrent_last_slot_has_exactly_one_winner` |

Additional phase-5/6 guarantees beyond the issue's bullet list:

| Guarantee | Proven by |
|-----------|-----------|
| Scoped drains keyed to the admitting partition keep same-key swaps admissible at the cap | `capacity_scope_finish_and_claim_swap_is_scope_neutral` |
| A transition with no key for a scope is not subject to that scope | `capacity_scope_harness_and_session_key_on_attribution_and_skip_when_absent`, `e2e_capacity_scope_harness_and_session_key_on_env_attribution` |
| Exemptions free scoped slots too | `capacity_scope_exemption_frees_the_scoped_slot` |
| Occupancy attribution is recorded on every committed status transition | `capacity_occupancy_records_the_admitting_attribution` |
| Scoped soft limits warn with partition evidence | `capacity_scope_soft_limit_warns_with_scope_evidence`, `e2e_capacity_scope_soft_limit_warns_in_json_without_rejecting` |
| Scope misconfiguration fails closed at policy load | `capacity_scope_validation_rejects_bad_shapes`, `loader_parses_and_validates_capacity_scopes` |
| Stats/coordination omit the capacity block when unconfigured (legacy shape) | `e2e_capacity_observability_in_stats_and_coordination` |
