//! Pure contracts for future `.write.lock` write combining.
//!
//! This module deliberately does not route commands through a combiner. It
//! defines the compatibility allowlist and request/result envelope shapes that a
//! future explicit combiner can use once output parity and failure-injection
//! tests exist.

use crate::cli::{
    CloseArgs, Commands, CommentAddArgs, CommentCommands, ConfigCommands, CreateArgs, DepAddArgs,
    DepCommands, DepRemoveArgs, HistoryArgs, HistoryCommands, ReopenArgs, UpdateArgs,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

/// Schema version for write-combining request and result envelopes.
pub const WRITE_COMBINING_SCHEMA_VERSION: &str = "br.write-combining.v1";
/// Conservative default request count for one future combined batch.
pub const DEFAULT_MAX_COMBINED_ENVELOPES: usize = 64;
/// Conservative default serialized argument budget for one future combined batch.
pub const DEFAULT_MAX_COMBINED_ARGUMENT_BYTES: usize = 1024 * 1024;

/// Whether a CLI command can enter the future write-combining queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "class", content = "value")]
pub enum CommandCompatibility {
    /// The command is in the conservative allowlist.
    Candidate(CompatibleMutation),
    /// The command must use the existing direct path.
    DirectOnly(DirectOnlyReason),
}

impl CommandCompatibility {
    /// Return the compatible mutation family, if this command can be queued.
    #[must_use]
    pub const fn candidate_family(self) -> Option<CompatibleMutation> {
        match self {
            Self::Candidate(family) => Some(family),
            Self::DirectOnly(_) => None,
        }
    }
}

/// Mutation families that are eligible for future write-combining proofs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibleMutation {
    /// A simple issue create with explicit CLI fields and no bulk file input.
    CreateIssue,
    /// An explicit inline comment add.
    AddComment,
    /// An explicit issue update limited to direct issue fields or labels.
    UpdateIssue,
    /// An explicit issue close without follow-up suggestion output.
    CloseIssue,
    /// An explicit issue reopen.
    ReopenIssue,
    /// An explicit dependency add without metadata side payloads.
    AddDependency,
    /// An explicit dependency removal.
    RemoveDependency,
}

/// Why a command must stay on the direct path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectOnlyReason {
    /// The command is read-only, so queueing would only add overhead.
    ReadOnly,
    /// The command can affect storage topology, external files, repair policy,
    /// or another side effect outside the first combiner allowlist.
    UnsafeCommand,
    /// The command uses file or stdin-style payloads that need direct handling.
    FileInput,
    /// The command is a preview and should not enter a mutating queue.
    DryRun,
    /// The command relies on last-touched state or an implicit target.
    MissingExplicitTarget,
    /// The command has no direct payload after validation.
    MissingPayload,
    /// The command uses a real but not-yet-proven option for this family.
    UnsupportedOption,
}

/// Output mode requested by a queued caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CombinedOutputMode {
    /// Machine-readable JSON.
    Json,
    /// Token-efficient TOON.
    Toon,
    /// Plain human text.
    Plain,
    /// Quiet mode.
    Quiet,
}

/// Auto-flush result attached to a combined mutation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CombinedFlushOutcome {
    /// The caller disabled or did not request auto-flush.
    NotRequested,
    /// The shared flush succeeded for this caller's committed mutation.
    Succeeded,
    /// SQLite committed, but the JSONL export failed.
    FailedAfterCommit,
}

/// Request envelope for a future explicit combiner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationEnvelope {
    /// Envelope schema version.
    pub schema_version: String,
    /// Caller-provided key used to detect accepted requests after crashes.
    pub idempotency_key: String,
    /// Actor to preserve in audit events.
    pub actor: String,
    /// Compatible mutation family.
    pub family: CompatibleMutation,
    /// Requested output mode.
    pub output_mode: CombinedOutputMode,
    /// Absolute caller deadline in Unix milliseconds.
    pub deadline_unix_ms: u64,
    /// Serialized command arguments for the compatible family.
    pub arguments: Value,
}

impl MutationEnvelope {
    /// Build an envelope with the current schema version.
    #[must_use]
    pub fn new(
        idempotency_key: impl Into<String>,
        actor: impl Into<String>,
        family: CompatibleMutation,
        output_mode: CombinedOutputMode,
        deadline_unix_ms: u64,
        arguments: Value,
    ) -> Self {
        Self {
            schema_version: WRITE_COMBINING_SCHEMA_VERSION.to_string(),
            idempotency_key: idempotency_key.into(),
            actor: actor.into(),
            family,
            output_mode,
            deadline_unix_ms,
            arguments,
        }
    }

    /// Validate envelope invariants that are independent of storage state.
    pub fn validate(&self) -> std::result::Result<(), EnvelopeValidationError> {
        if self.schema_version != WRITE_COMBINING_SCHEMA_VERSION {
            return Err(EnvelopeValidationError::UnsupportedSchemaVersion);
        }
        if self.idempotency_key.trim().is_empty() {
            return Err(EnvelopeValidationError::EmptyIdempotencyKey);
        }
        if self.actor.trim().is_empty() {
            return Err(EnvelopeValidationError::EmptyActor);
        }
        if self.deadline_unix_ms == 0 {
            return Err(EnvelopeValidationError::MissingDeadline);
        }
        Ok(())
    }
}

/// Storage-independent envelope validation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeValidationError {
    /// The envelope uses a schema version this binary does not support.
    UnsupportedSchemaVersion,
    /// The idempotency key is empty or whitespace.
    EmptyIdempotencyKey,
    /// The actor is empty or whitespace.
    EmptyActor,
    /// The caller did not provide a bounded deadline.
    MissingDeadline,
}

/// Per-caller result shape for a future combiner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationResult {
    /// The idempotency key from the accepted envelope.
    pub idempotency_key: String,
    /// Mutation family that was applied.
    pub family: CompatibleMutation,
    /// Process-style exit code to preserve CLI surfaces.
    pub exit_code: i32,
    /// Captured stdout for this caller.
    pub stdout: String,
    /// Captured stderr for this caller.
    pub stderr: String,
    /// Auto-flush result visible to this caller.
    pub flush: CombinedFlushOutcome,
}

impl MutationResult {
    /// Build a per-caller mutation result.
    #[must_use]
    pub fn new(
        idempotency_key: impl Into<String>,
        family: CompatibleMutation,
        exit_code: i32,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
        flush: CombinedFlushOutcome,
    ) -> Self {
        Self {
            idempotency_key: idempotency_key.into(),
            family,
            exit_code,
            stdout: stdout.into(),
            stderr: stderr.into(),
            flush,
        }
    }
}

/// Resource caps for planning one future combined batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchLimits {
    /// Maximum number of envelopes accepted into one batch.
    pub max_envelopes: usize,
    /// Maximum compact JSON bytes for accepted envelope arguments.
    pub max_argument_bytes: usize,
}

impl BatchLimits {
    /// Build explicit batch limits.
    #[must_use]
    pub const fn new(max_envelopes: usize, max_argument_bytes: usize) -> Self {
        Self {
            max_envelopes,
            max_argument_bytes,
        }
    }

    /// Validate that both limits are usable.
    ///
    /// # Errors
    ///
    /// Returns an error when either bound is zero.
    pub const fn validate(self) -> std::result::Result<(), BatchLimitError> {
        if self.max_envelopes == 0 {
            return Err(BatchLimitError::ZeroMaxEnvelopes);
        }
        if self.max_argument_bytes == 0 {
            return Err(BatchLimitError::ZeroMaxArgumentBytes);
        }
        Ok(())
    }
}

impl Default for BatchLimits {
    fn default() -> Self {
        Self {
            max_envelopes: DEFAULT_MAX_COMBINED_ENVELOPES,
            max_argument_bytes: DEFAULT_MAX_COMBINED_ARGUMENT_BYTES,
        }
    }
}

/// Invalid batch-limit configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchLimitError {
    /// A batch must be able to accept at least one envelope.
    ZeroMaxEnvelopes,
    /// A batch must have a positive argument-byte budget.
    ZeroMaxArgumentBytes,
}

/// One accepted envelope in a planned homogeneous batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedMutation {
    /// Original zero-based queue position.
    pub index: usize,
    /// Compatible family for the accepted mutation.
    pub family: CompatibleMutation,
    /// Compact JSON byte size of the serialized argument payload.
    pub argument_bytes: usize,
}

/// One envelope not accepted into the planned batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedMutation {
    /// Original zero-based queue position.
    pub index: usize,
    /// Why the envelope was not accepted.
    pub reason: BatchSkipReason,
}

/// Why a queued envelope is not in the planned batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason", content = "detail")]
pub enum BatchSkipReason {
    /// Envelope-level validation failed before any side effects.
    InvalidEnvelope(EnvelopeValidationError),
    /// The caller deadline has already elapsed.
    Expired,
    /// A prior accepted envelope in this batch has the same idempotency key.
    DuplicateIdempotencyKey {
        /// Duplicated caller idempotency key.
        idempotency_key: String,
    },
    /// The batch accepted the maximum number of envelopes.
    QueueFull,
    /// Accepting this envelope would exceed the argument-byte budget.
    ArgumentBytesExceeded {
        /// Bytes already accepted before this envelope.
        used: usize,
        /// Bytes required by this envelope.
        needed: usize,
        /// Configured maximum bytes for the batch.
        limit: usize,
    },
    /// The planner hit a different compatible family after accepting a prefix.
    FamilyBoundary {
        /// Family accepted by the current batch.
        expected: CompatibleMutation,
        /// Family found at the boundary.
        found: CompatibleMutation,
    },
    /// The envelope was behind a capacity or family boundary and is left for a
    /// later direct path or later batch.
    BlockedByBatchBoundary,
}

/// Plan for one homogeneous write-combining batch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchPlan {
    /// Homogeneous family accepted by this batch.
    pub family: Option<CompatibleMutation>,
    /// Accepted envelopes in original queue order.
    pub accepted: Vec<PlannedMutation>,
    /// Envelopes rejected or deferred by the planner.
    pub skipped: Vec<SkippedMutation>,
    /// Sum of accepted compact argument JSON bytes.
    pub used_argument_bytes: usize,
}

/// Per-envelope response emitted by a combined batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchResponse {
    /// Original zero-based queue position.
    pub index: usize,
    /// The envelope's idempotency key.
    pub idempotency_key: String,
    /// The envelope's compatible family.
    pub family: CompatibleMutation,
    /// Output mode requested by the caller.
    pub output_mode: CombinedOutputMode,
    /// Applied result or pre-execution skip reason.
    pub outcome: BatchResponseOutcome,
}

/// Applied or skipped response for one queued envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum BatchResponseOutcome {
    /// The mutation ran and produced the caller-visible output.
    Applied(MutationResult),
    /// The mutation did not run.
    Skipped(BatchSkipReason),
}

/// Deterministic report for one future combined batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchReport {
    /// Report schema version.
    pub schema_version: String,
    /// Homogeneous family accepted by the batch, if any.
    pub family: Option<CompatibleMutation>,
    /// One response per input envelope, sorted by original queue position.
    pub responses: Vec<BatchResponse>,
}

/// Aggregated caller-visible outcomes for one combined batch report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchOutcomeSummary {
    /// Total responses emitted by the report.
    pub response_count: usize,
    /// Responses for envelopes that reached the executor.
    pub applied_count: usize,
    /// Responses for envelopes rejected before execution.
    pub skipped_count: usize,
    /// Applied mutations with process-style success exit codes.
    pub successful_mutations: usize,
    /// Applied mutations with non-zero process-style exit codes.
    pub failed_mutations: usize,
    /// Applied mutations whose SQLite commit succeeded but JSONL flush failed.
    pub flush_failures: usize,
    /// Original queue index for the first non-zero result, if any.
    pub first_failed_index: Option<usize>,
    /// Original queue index for the first post-commit flush failure, if any.
    pub first_flush_failure_index: Option<usize>,
}

impl BatchReport {
    /// Count applied responses.
    #[must_use]
    pub fn applied_count(&self) -> usize {
        self.responses
            .iter()
            .filter(|response| matches!(response.outcome, BatchResponseOutcome::Applied(_)))
            .count()
    }

    /// Count skipped responses.
    #[must_use]
    pub fn skipped_count(&self) -> usize {
        self.responses.len() - self.applied_count()
    }

    /// Summarize caller-visible failures without hiding per-envelope results.
    #[must_use]
    pub fn outcome_summary(&self) -> BatchOutcomeSummary {
        let mut summary = BatchOutcomeSummary {
            response_count: self.responses.len(),
            applied_count: 0,
            skipped_count: 0,
            successful_mutations: 0,
            failed_mutations: 0,
            flush_failures: 0,
            first_failed_index: None,
            first_flush_failure_index: None,
        };

        for response in &self.responses {
            match &response.outcome {
                BatchResponseOutcome::Applied(result) => {
                    summary.applied_count += 1;
                    if result.exit_code == 0 {
                        summary.successful_mutations += 1;
                    } else {
                        summary.failed_mutations += 1;
                        if summary.first_failed_index.is_none() {
                            summary.first_failed_index = Some(response.index);
                        }
                    }
                    if result.flush == CombinedFlushOutcome::FailedAfterCommit {
                        summary.flush_failures += 1;
                        if summary.first_flush_failure_index.is_none() {
                            summary.first_flush_failure_index = Some(response.index);
                        }
                    }
                }
                BatchResponseOutcome::Skipped(_) => {
                    summary.skipped_count += 1;
                }
            }
        }

        summary
    }
}

/// Invalid executor output while building a batch report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "error", content = "detail")]
pub enum BatchReportError {
    /// A planned accepted or skipped index does not exist in the input queue.
    PlannedIndexOutOfBounds {
        /// Planned index.
        index: usize,
        /// Number of input envelopes.
        envelope_count: usize,
    },
    /// The plan names the same queue index more than once.
    DuplicatePlannedIndex {
        /// Duplicated queue index.
        index: usize,
    },
    /// The plan did not account for every input envelope.
    IncompletePlan {
        /// Number of input envelopes.
        expected: usize,
        /// Number of responses assembled from the plan.
        planned: usize,
    },
    /// The accepted plan family does not match the original envelope.
    PlannedFamilyMismatch {
        /// Original queue index.
        index: usize,
        /// Envelope mutation family.
        expected: CompatibleMutation,
        /// Planned mutation family.
        found: CompatibleMutation,
    },
    /// The executor returned the same idempotency key more than once.
    DuplicateResult {
        /// Duplicated result idempotency key.
        idempotency_key: String,
    },
    /// The accepted plan contains duplicate idempotency keys.
    DuplicateAcceptedIdempotencyKey {
        /// Duplicated accepted idempotency key.
        idempotency_key: String,
    },
    /// An accepted envelope has no executor result.
    MissingResult {
        /// Original queue index.
        index: usize,
        /// Missing result idempotency key.
        idempotency_key: String,
    },
    /// The executor produced a result for an envelope that was not accepted.
    UnexpectedResult {
        /// Unexpected result idempotency key.
        idempotency_key: String,
    },
    /// The executor result does not match the accepted envelope family.
    ResultFamilyMismatch {
        /// Result idempotency key.
        idempotency_key: String,
        /// Expected mutation family.
        expected: CompatibleMutation,
        /// Reported mutation family.
        found: CompatibleMutation,
    },
}

/// Error from planning, executing, or assembling a combined batch harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "error", content = "detail")]
pub enum BatchExecutionError {
    /// The configured batch limits were unusable.
    BatchLimits(BatchLimitError),
    /// The executor output did not match the planned batch.
    Report(BatchReportError),
}

impl BatchPlan {
    /// Number of accepted envelopes.
    #[must_use]
    pub fn accepted_count(&self) -> usize {
        self.accepted.len()
    }

    /// Whether this batch would apply no mutations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.accepted.is_empty()
    }
}

/// Build a conservative homogeneous-prefix batch plan.
///
/// Invalid and expired envelopes are rejected before side effects and do not
/// block later valid entries. Once a valid envelope cannot enter the current
/// batch because of capacity or family boundaries, all later entries are left
/// for a later direct path or later batch. That preserves queue-prefix ordering
/// for every accepted mutation.
///
/// # Errors
///
/// Returns an error when `limits` has a zero request or byte bound.
pub fn plan_batch(
    envelopes: &[MutationEnvelope],
    limits: BatchLimits,
    now_unix_ms: u64,
) -> std::result::Result<BatchPlan, BatchLimitError> {
    limits.validate()?;

    let mut plan = BatchPlan::default();
    let mut accepted_idempotency_keys = BTreeSet::new();
    let mut boundary_seen = false;

    for (index, envelope) in envelopes.iter().enumerate() {
        if boundary_seen {
            plan.skipped
                .push(skipped(index, BatchSkipReason::BlockedByBatchBoundary));
            continue;
        }

        if let Err(err) = envelope.validate() {
            plan.skipped
                .push(skipped(index, BatchSkipReason::InvalidEnvelope(err)));
            continue;
        }

        if envelope.deadline_unix_ms <= now_unix_ms {
            plan.skipped.push(skipped(index, BatchSkipReason::Expired));
            continue;
        }

        if accepted_idempotency_keys.contains(envelope.idempotency_key.as_str()) {
            plan.skipped.push(skipped(
                index,
                BatchSkipReason::DuplicateIdempotencyKey {
                    idempotency_key: envelope.idempotency_key.clone(),
                },
            ));
            continue;
        }

        if let Some(expected) = plan.family
            && envelope.family != expected
        {
            plan.skipped.push(skipped(
                index,
                BatchSkipReason::FamilyBoundary {
                    expected,
                    found: envelope.family,
                },
            ));
            boundary_seen = true;
            continue;
        }

        if plan.accepted.len() >= limits.max_envelopes {
            plan.skipped
                .push(skipped(index, BatchSkipReason::QueueFull));
            boundary_seen = true;
            continue;
        }

        let Some(argument_bytes) = compact_json_len(&envelope.arguments) else {
            plan.skipped.push(skipped(
                index,
                BatchSkipReason::ArgumentBytesExceeded {
                    used: plan.used_argument_bytes,
                    needed: usize::MAX,
                    limit: limits.max_argument_bytes,
                },
            ));
            boundary_seen = true;
            continue;
        };
        let Some(next_bytes) = plan.used_argument_bytes.checked_add(argument_bytes) else {
            plan.skipped.push(skipped(
                index,
                BatchSkipReason::ArgumentBytesExceeded {
                    used: plan.used_argument_bytes,
                    needed: argument_bytes,
                    limit: limits.max_argument_bytes,
                },
            ));
            boundary_seen = true;
            continue;
        };
        if next_bytes > limits.max_argument_bytes {
            plan.skipped.push(skipped(
                index,
                BatchSkipReason::ArgumentBytesExceeded {
                    used: plan.used_argument_bytes,
                    needed: argument_bytes,
                    limit: limits.max_argument_bytes,
                },
            ));
            boundary_seen = true;
            continue;
        }

        plan.family.get_or_insert(envelope.family);
        accepted_idempotency_keys.insert(envelope.idempotency_key.as_str());
        plan.used_argument_bytes = next_bytes;
        plan.accepted.push(PlannedMutation {
            index,
            family: envelope.family,
            argument_bytes,
        });
    }

    Ok(plan)
}

/// Assemble deterministic per-envelope responses for a planned batch.
///
/// The future executor can apply only `plan.accepted`; this function then
/// merges those applied results with the pre-execution skips from `plan.skipped`
/// and validates that every input envelope receives exactly one response.
///
/// # Errors
///
/// Returns an error if the plan is inconsistent with the input envelopes or if
/// the applied results do not exactly match the accepted envelopes.
pub fn assemble_batch_report(
    envelopes: &[MutationEnvelope],
    plan: &BatchPlan,
    applied_results: &[MutationResult],
) -> std::result::Result<BatchReport, BatchReportError> {
    let mut results_by_key = BTreeMap::new();
    for result in applied_results {
        if results_by_key
            .insert(result.idempotency_key.as_str(), result)
            .is_some()
        {
            return Err(BatchReportError::DuplicateResult {
                idempotency_key: result.idempotency_key.clone(),
            });
        }
    }

    let mut accepted_keys = BTreeSet::new();
    let mut responses_by_index = BTreeMap::new();

    for skipped_mutation in &plan.skipped {
        let envelope = envelope_at(envelopes, skipped_mutation.index)?;
        insert_response(
            &mut responses_by_index,
            BatchResponse {
                index: skipped_mutation.index,
                idempotency_key: envelope.idempotency_key.clone(),
                family: envelope.family,
                output_mode: envelope.output_mode,
                outcome: BatchResponseOutcome::Skipped(skipped_mutation.reason.clone()),
            },
        )?;
    }

    for planned_mutation in &plan.accepted {
        let envelope = envelope_at(envelopes, planned_mutation.index)?;
        if planned_mutation.family != envelope.family {
            return Err(BatchReportError::PlannedFamilyMismatch {
                index: planned_mutation.index,
                expected: envelope.family,
                found: planned_mutation.family,
            });
        }
        if !accepted_keys.insert(envelope.idempotency_key.as_str()) {
            return Err(BatchReportError::DuplicateAcceptedIdempotencyKey {
                idempotency_key: envelope.idempotency_key.clone(),
            });
        }
        let Some(result) = results_by_key.get(envelope.idempotency_key.as_str()) else {
            return Err(BatchReportError::MissingResult {
                index: planned_mutation.index,
                idempotency_key: envelope.idempotency_key.clone(),
            });
        };
        if result.family != envelope.family {
            return Err(BatchReportError::ResultFamilyMismatch {
                idempotency_key: envelope.idempotency_key.clone(),
                expected: envelope.family,
                found: result.family,
            });
        }

        insert_response(
            &mut responses_by_index,
            BatchResponse {
                index: planned_mutation.index,
                idempotency_key: envelope.idempotency_key.clone(),
                family: envelope.family,
                output_mode: envelope.output_mode,
                outcome: BatchResponseOutcome::Applied((*result).clone()),
            },
        )?;
    }

    for result in applied_results {
        if !accepted_keys.contains(result.idempotency_key.as_str()) {
            return Err(BatchReportError::UnexpectedResult {
                idempotency_key: result.idempotency_key.clone(),
            });
        }
    }

    if responses_by_index.len() != envelopes.len() {
        return Err(BatchReportError::IncompletePlan {
            expected: envelopes.len(),
            planned: responses_by_index.len(),
        });
    }

    Ok(BatchReport {
        schema_version: WRITE_COMBINING_SCHEMA_VERSION.to_string(),
        family: plan.family,
        responses: responses_by_index.into_values().collect(),
    })
}

/// Plan one batch, run an injected executor for accepted envelopes, and report.
///
/// This is a storage-independent harness for future combiner wiring and tests.
/// It never executes skipped envelopes; `assemble_batch_report` remains the
/// single validation point for per-caller output parity.
///
/// # Errors
///
/// Returns a batch-limit error for invalid limits, or a report error if the
/// injected executor returns output inconsistent with the accepted envelopes.
pub fn execute_batch_with<F>(
    envelopes: &[MutationEnvelope],
    limits: BatchLimits,
    now_unix_ms: u64,
    mut execute: F,
) -> std::result::Result<BatchReport, BatchExecutionError>
where
    F: FnMut(&MutationEnvelope) -> MutationResult,
{
    let plan =
        plan_batch(envelopes, limits, now_unix_ms).map_err(BatchExecutionError::BatchLimits)?;
    let mut results = Vec::with_capacity(plan.accepted.len());
    for planned_mutation in &plan.accepted {
        let envelope = envelopes
            .get(planned_mutation.index)
            .ok_or(BatchExecutionError::Report(
                BatchReportError::PlannedIndexOutOfBounds {
                    index: planned_mutation.index,
                    envelope_count: envelopes.len(),
                },
            ))?;
        results.push(execute(envelope));
    }
    assemble_batch_report(envelopes, &plan, &results).map_err(BatchExecutionError::Report)
}

fn envelope_at(
    envelopes: &[MutationEnvelope],
    index: usize,
) -> std::result::Result<&MutationEnvelope, BatchReportError> {
    envelopes
        .get(index)
        .ok_or(BatchReportError::PlannedIndexOutOfBounds {
            index,
            envelope_count: envelopes.len(),
        })
}

fn insert_response(
    responses_by_index: &mut BTreeMap<usize, BatchResponse>,
    response: BatchResponse,
) -> std::result::Result<(), BatchReportError> {
    let index = response.index;
    if responses_by_index.insert(index, response).is_some() {
        return Err(BatchReportError::DuplicatePlannedIndex { index });
    }
    Ok(())
}

fn skipped(index: usize, reason: BatchSkipReason) -> SkippedMutation {
    SkippedMutation { index, reason }
}

fn compact_json_len(value: &Value) -> Option<usize> {
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.len)
}

#[derive(Default)]
struct CountingWriter {
    len: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.len = self
            .len
            .checked_add(buf.len())
            .ok_or_else(|| io::Error::other("compact JSON length overflow"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Classify a parsed CLI command for future write-combining eligibility.
#[must_use]
pub fn classify_command(command: &Commands) -> CommandCompatibility {
    match command {
        Commands::Create(args) => classify_create(args),
        Commands::Comments(args) => args.command.as_ref().map_or(
            CommandCompatibility::DirectOnly(DirectOnlyReason::ReadOnly),
            |command| match command {
                CommentCommands::Add(args) => classify_comment_add(args),
                CommentCommands::List(_) => {
                    CommandCompatibility::DirectOnly(DirectOnlyReason::ReadOnly)
                }
            },
        ),
        Commands::Update(args) => classify_update(args),
        Commands::Close(args) => classify_close(args),
        Commands::Reopen(args) => classify_reopen(args),
        Commands::Dep { command } => classify_dependency(command),
        Commands::Config { command } => classify_config(command),
        Commands::History(args) => classify_history(args),
        Commands::Sync(_) | Commands::Doctor(_) => {
            CommandCompatibility::DirectOnly(DirectOnlyReason::UnsafeCommand)
        }
        Commands::List(_)
        | Commands::Show(_)
        | Commands::Search(_)
        | Commands::Ready(_)
        | Commands::Blocked(_)
        | Commands::Count(_)
        | Commands::Stale(_)
        | Commands::Lint(_)
        | Commands::Stats(_)
        | Commands::Status(_)
        | Commands::Changelog(_)
        | Commands::Graph(_)
        | Commands::Info(_)
        | Commands::Schema(_)
        | Commands::Where
        | Commands::Version(_)
        | Commands::Completions(_)
        | Commands::Query { .. } => CommandCompatibility::DirectOnly(DirectOnlyReason::ReadOnly),
        _ => CommandCompatibility::DirectOnly(DirectOnlyReason::UnsafeCommand),
    }
}

fn classify_create(args: &CreateArgs) -> CommandCompatibility {
    if args.dry_run {
        return CommandCompatibility::DirectOnly(DirectOnlyReason::DryRun);
    }
    if args.file.is_some() {
        return CommandCompatibility::DirectOnly(DirectOnlyReason::FileInput);
    }
    if args.title.is_none() && args.title_flag.is_none() {
        return CommandCompatibility::DirectOnly(DirectOnlyReason::MissingPayload);
    }
    if args.ephemeral || args.parent.is_some() || !args.deps.is_empty() {
        return CommandCompatibility::DirectOnly(DirectOnlyReason::UnsupportedOption);
    }
    CommandCompatibility::Candidate(CompatibleMutation::CreateIssue)
}

fn classify_comment_add(args: &CommentAddArgs) -> CommandCompatibility {
    if args.file.is_some() {
        return CommandCompatibility::DirectOnly(DirectOnlyReason::FileInput);
    }
    if args.message.as_ref().is_none_or(String::is_empty) && args.text.is_empty() {
        return CommandCompatibility::DirectOnly(DirectOnlyReason::MissingPayload);
    }
    CommandCompatibility::Candidate(CompatibleMutation::AddComment)
}

fn classify_update(args: &UpdateArgs) -> CommandCompatibility {
    if args.ids.is_empty() {
        return CommandCompatibility::DirectOnly(DirectOnlyReason::MissingExplicitTarget);
    }
    if has_unsupported_update_option(args) {
        return CommandCompatibility::DirectOnly(DirectOnlyReason::UnsupportedOption);
    }
    if has_supported_update_payload(args) {
        CommandCompatibility::Candidate(CompatibleMutation::UpdateIssue)
    } else {
        CommandCompatibility::DirectOnly(DirectOnlyReason::MissingPayload)
    }
}

fn has_supported_update_payload(args: &UpdateArgs) -> bool {
    args.title.is_some()
        || args.description.is_some()
        || args.status.is_some()
        || args.priority.is_some()
        || args.type_.is_some()
        || args.assignee.is_some()
        || args.claim
        || !args.add_label.is_empty()
        || !args.remove_label.is_empty()
        || !args.set_labels.is_empty()
}

fn has_unsupported_update_option(args: &UpdateArgs) -> bool {
    args.design.is_some()
        || args.acceptance_criteria.is_some()
        || args.notes.is_some()
        || args.owner.is_some()
        || args.force
        || args.due.is_some()
        || args.defer.is_some()
        || args.estimate.is_some()
        || args.parent.is_some()
        || args.external_ref.is_some()
        || args.session.is_some()
}

fn classify_close(args: &CloseArgs) -> CommandCompatibility {
    if args.ids.is_empty() {
        return CommandCompatibility::DirectOnly(DirectOnlyReason::MissingExplicitTarget);
    }
    if args.force || args.suggest_next {
        return CommandCompatibility::DirectOnly(DirectOnlyReason::UnsupportedOption);
    }
    CommandCompatibility::Candidate(CompatibleMutation::CloseIssue)
}

fn classify_reopen(args: &ReopenArgs) -> CommandCompatibility {
    if args.ids.is_empty() {
        return CommandCompatibility::DirectOnly(DirectOnlyReason::MissingExplicitTarget);
    }
    CommandCompatibility::Candidate(CompatibleMutation::ReopenIssue)
}

fn classify_dependency(command: &DepCommands) -> CommandCompatibility {
    match command {
        DepCommands::Add(args) => classify_dependency_add(args),
        DepCommands::Import(_) => CommandCompatibility::DirectOnly(DirectOnlyReason::FileInput),
        DepCommands::Remove(args) => classify_dependency_remove(args),
        DepCommands::List(_) | DepCommands::Tree(_) | DepCommands::Cycles(_) => {
            CommandCompatibility::DirectOnly(DirectOnlyReason::ReadOnly)
        }
    }
}

fn classify_dependency_add(args: &DepAddArgs) -> CommandCompatibility {
    if args.metadata.is_some() {
        CommandCompatibility::DirectOnly(DirectOnlyReason::UnsupportedOption)
    } else {
        CommandCompatibility::Candidate(CompatibleMutation::AddDependency)
    }
}

fn classify_dependency_remove(_args: &DepRemoveArgs) -> CommandCompatibility {
    CommandCompatibility::Candidate(CompatibleMutation::RemoveDependency)
}

fn classify_config(command: &ConfigCommands) -> CommandCompatibility {
    match command {
        ConfigCommands::List { .. } | ConfigCommands::Get { .. } | ConfigCommands::Path => {
            CommandCompatibility::DirectOnly(DirectOnlyReason::ReadOnly)
        }
        ConfigCommands::Set { .. } | ConfigCommands::Delete { .. } | ConfigCommands::Edit => {
            CommandCompatibility::DirectOnly(DirectOnlyReason::UnsafeCommand)
        }
    }
}

fn classify_history(args: &HistoryArgs) -> CommandCompatibility {
    match args.command.as_ref() {
        None | Some(HistoryCommands::List | HistoryCommands::Diff { .. }) => {
            CommandCompatibility::DirectOnly(DirectOnlyReason::ReadOnly)
        }
        Some(HistoryCommands::Restore { .. } | HistoryCommands::Prune { .. }) => {
            CommandCompatibility::DirectOnly(DirectOnlyReason::UnsafeCommand)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, CommentListArgs, CommentsArgs, DepListArgs, DepRemoveArgs, DepTreeArgs};
    use crate::model::{Event, Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use crate::sync::{ExportConfig, export_to_jsonl_with_policy, finalize_export};
    use chrono::{TimeZone, Utc};
    use clap::Parser;
    use serde_json::{Value as JsonValue, json};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    const NOW_UNIX_MS: u64 = 1_000;
    const FUTURE_UNIX_MS: u64 = 2_000;

    fn envelope(
        key: &str,
        family: CompatibleMutation,
        deadline_unix_ms: u64,
        arguments: Value,
    ) -> MutationEnvelope {
        MutationEnvelope::new(
            key,
            "DustyPuma",
            family,
            CombinedOutputMode::Json,
            deadline_unix_ms,
            arguments,
        )
    }

    fn planned(result: std::result::Result<BatchPlan, BatchLimitError>) -> BatchPlan {
        assert!(result.is_ok(), "unexpected batch-limit error: {result:?}");
        match result {
            Ok(plan) => plan,
            Err(err) => {
                assert_eq!(Some(err), None);
                BatchPlan::default()
            }
        }
    }

    fn mutation_result(key: &str, family: CompatibleMutation) -> MutationResult {
        MutationResult::new(key, family, 0, "ok", "", CombinedFlushOutcome::Succeeded)
    }

    fn classify_argv(argv: &[&str]) -> CommandCompatibility {
        let cli = Cli::parse_from(argv.iter().copied());
        classify_command(&cli.command)
    }

    #[derive(Debug, PartialEq, Eq)]
    struct EventFingerprint {
        issue_id: String,
        event_type: String,
        actor: String,
        comment: Option<String>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct CreateBurstState {
        results: Vec<MutationResult>,
        issues: Vec<Issue>,
        event_order: Vec<EventFingerprint>,
        jsonl_bytes: String,
        dirty_ids: Vec<String>,
        needs_flush: Option<String>,
        export_hashes: Vec<(String, String)>,
    }

    fn create_burst_envelope(
        index: usize,
        title: &str,
        issue_type: &str,
        priority: i32,
        labels: &[&str],
    ) -> MutationEnvelope {
        envelope(
            &format!("create-{index}"),
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({
                "title": title,
                "description": format!("description for {title}"),
                "type": issue_type,
                "priority": priority.to_string(),
                "labels": labels,
            }),
        )
    }

    fn json_arg_str<'a>(arguments: &'a JsonValue, key: &str) -> &'a str {
        arguments
            .get(key)
            .and_then(JsonValue::as_str)
            .expect("missing string argument")
    }

    fn test_create_issue_from_envelope(envelope: &MutationEnvelope, sequence: usize) -> Issue {
        let arguments = &envelope.arguments;
        let timestamp_second = u32::try_from(sequence).expect("test sequence fits in timestamp");
        let created_at = Utc
            .with_ymd_and_hms(2026, 5, 8, 12, 0, timestamp_second)
            .single()
            .expect("valid deterministic timestamp");
        let labels = arguments
            .get("labels")
            .and_then(JsonValue::as_array)
            .expect("labels array")
            .iter()
            .map(|label| {
                label
                    .as_str()
                    .expect("label should be a string")
                    .to_string()
            })
            .collect();
        let priority =
            Priority::from_str(json_arg_str(arguments, "priority")).expect("valid priority");
        let issue_type =
            IssueType::from_str(json_arg_str(arguments, "type")).expect("valid issue type");

        let mut issue = Issue {
            id: format!("bd-burst-{sequence:03}"),
            title: json_arg_str(arguments, "title").to_string(),
            description: Some(json_arg_str(arguments, "description").to_string()),
            status: Status::Open,
            priority,
            issue_type,
            created_at,
            updated_at: created_at,
            created_by: Some(envelope.actor.clone()),
            source_repo: Some("write-combining-parity".to_string()),
            labels,
            ..Issue::default()
        };
        issue.content_hash = Some(issue.compute_content_hash());
        issue
    }

    fn success_result_for_created_issue(
        envelope: &MutationEnvelope,
        issue: &Issue,
    ) -> MutationResult {
        let stdout = serde_json::to_string(&json!({
            "id": issue.id,
            "title": issue.title,
            "status": issue.status.as_str(),
        }))
        .expect("serialize create result");
        MutationResult::new(
            envelope.idempotency_key.clone(),
            envelope.family,
            0,
            stdout,
            "",
            CombinedFlushOutcome::NotRequested,
        )
    }

    fn try_apply_test_create(
        storage: &mut SqliteStorage,
        envelope: &MutationEnvelope,
        sequence: usize,
    ) -> std::result::Result<MutationResult, String> {
        let issue = test_create_issue_from_envelope(envelope, sequence);
        storage
            .create_issue(&issue, &envelope.actor)
            .map_err(|err| err.to_string())?;
        Ok(success_result_for_created_issue(envelope, &issue))
    }

    fn apply_test_create(
        storage: &mut SqliteStorage,
        envelope: &MutationEnvelope,
        sequence: usize,
    ) -> MutationResult {
        try_apply_test_create(storage, envelope, sequence).expect("test create should write")
    }

    fn failed_result(
        envelope: &MutationEnvelope,
        exit_code: i32,
        stderr: impl Into<String>,
        flush: CombinedFlushOutcome,
    ) -> MutationResult {
        MutationResult::new(
            envelope.idempotency_key.clone(),
            envelope.family,
            exit_code,
            "",
            stderr,
            flush,
        )
    }

    fn flush_to_jsonl(storage: &mut SqliteStorage, jsonl_path: &Path) -> String {
        let config = ExportConfig {
            force: true,
            is_default_path: true,
            ..ExportConfig::default()
        };
        let (result, _) =
            export_to_jsonl_with_policy(storage, jsonl_path, &config).expect("export JSONL");
        let issue_hashes = result.issue_hashes.clone();
        finalize_export(storage, &result, Some(&issue_hashes), jsonl_path)
            .expect("finalize export");
        fs::read_to_string(jsonl_path).expect("read exported JSONL")
    }

    fn stored_issues(storage: &SqliteStorage) -> Vec<Issue> {
        let mut issues = storage
            .get_all_issues_for_export()
            .expect("read exported issues");
        issues.sort_by(|left, right| left.id.cmp(&right.id));
        issues
    }

    fn stored_issue_titles(storage: &SqliteStorage) -> Vec<String> {
        stored_issues(storage)
            .into_iter()
            .map(|issue| issue.title)
            .collect()
    }

    fn event_fingerprints(events: Vec<Event>) -> Vec<EventFingerprint> {
        events
            .into_iter()
            .rev()
            .map(|event| EventFingerprint {
                issue_id: event.issue_id,
                event_type: event.event_type.as_str().to_string(),
                actor: event.actor,
                comment: event.comment,
            })
            .collect()
    }

    fn stored_event_issue_ids(storage: &SqliteStorage) -> Vec<String> {
        event_fingerprints(storage.get_all_events(0).expect("read events"))
            .into_iter()
            .map(|event| event.issue_id)
            .collect()
    }

    fn stored_unique_event_issue_ids(storage: &SqliteStorage) -> Vec<String> {
        let mut issue_ids = stored_event_issue_ids(storage);
        issue_ids.sort();
        issue_ids.dedup();
        issue_ids
    }

    fn export_hashes_for(storage: &SqliteStorage, issues: &[Issue]) -> Vec<(String, String)> {
        issues
            .iter()
            .map(|issue| {
                let (hash, _) = storage
                    .get_export_hash(&issue.id)
                    .expect("read export hash")
                    .expect("missing export hash");
                (issue.id.clone(), hash)
            })
            .collect()
    }

    fn create_burst_state(
        storage: &SqliteStorage,
        jsonl_bytes: String,
        results: Vec<MutationResult>,
    ) -> CreateBurstState {
        let issues = stored_issues(storage);
        let event_order = event_fingerprints(storage.get_all_events(0).expect("read events"));
        let dirty_ids = storage.get_dirty_issue_ids().expect("read dirty ids");
        let needs_flush = storage
            .get_metadata("needs_flush")
            .expect("read needs_flush");
        let export_hashes = export_hashes_for(storage, &issues);

        CreateBurstState {
            results,
            issues,
            event_order,
            jsonl_bytes,
            dirty_ids,
            needs_flush,
            export_hashes,
        }
    }

    fn mark_results_flushed(results: &mut [MutationResult]) {
        for result in results {
            result.flush = CombinedFlushOutcome::Succeeded;
        }
    }

    fn mark_report_flushed(report: &mut BatchReport) {
        for response in &mut report.responses {
            if let BatchResponseOutcome::Applied(result) = &mut response.outcome {
                result.flush = CombinedFlushOutcome::Succeeded;
            }
        }
    }

    fn applied_results(report: &BatchReport) -> Vec<MutationResult> {
        report
            .responses
            .iter()
            .filter_map(|response| match &response.outcome {
                BatchResponseOutcome::Applied(result) => Some(result.clone()),
                BatchResponseOutcome::Skipped(_) => None,
            })
            .collect()
    }

    fn executed(result: std::result::Result<BatchReport, BatchExecutionError>) -> BatchReport {
        assert!(
            result.is_ok(),
            "unexpected batch execution error: {result:?}"
        );
        match result {
            Ok(report) => report,
            Err(err) => {
                assert_eq!(Some(err), None);
                BatchReport {
                    schema_version: String::new(),
                    family: None,
                    responses: Vec::new(),
                }
            }
        }
    }

    fn run_direct_create_burst(envelopes: &[MutationEnvelope]) -> CreateBurstState {
        let dir = tempfile::tempdir().expect("tempdir");
        let jsonl_path = dir.path().join("direct.jsonl");
        let mut storage = SqliteStorage::open_memory().expect("open direct storage");
        let mut results = Vec::with_capacity(envelopes.len());
        let mut jsonl_bytes = String::new();

        for (index, envelope) in envelopes.iter().enumerate() {
            let result = apply_test_create(&mut storage, envelope, index + 1);
            jsonl_bytes = flush_to_jsonl(&mut storage, &jsonl_path);
            results.push(result);
        }
        mark_results_flushed(&mut results);

        create_burst_state(&storage, jsonl_bytes, results)
    }

    fn run_combined_create_burst(envelopes: &[MutationEnvelope]) -> CreateBurstState {
        let dir = tempfile::tempdir().expect("tempdir");
        let jsonl_path = dir.path().join("combined.jsonl");
        let mut storage = SqliteStorage::open_memory().expect("open combined storage");
        let mut sequence = 0;
        let mut report = executed(execute_batch_with(
            envelopes,
            BatchLimits::default(),
            NOW_UNIX_MS,
            |envelope| {
                sequence += 1;
                apply_test_create(&mut storage, envelope, sequence)
            },
        ));
        let jsonl_bytes = flush_to_jsonl(&mut storage, &jsonl_path);
        mark_report_flushed(&mut report);
        create_burst_state(&storage, jsonl_bytes, applied_results(&report))
    }

    fn response_outcome(report: &BatchReport, index: usize) -> &BatchResponseOutcome {
        report
            .responses
            .iter()
            .find(|response| response.index == index)
            .map(|response| &response.outcome)
            .expect("response exists")
    }

    fn move_accepted_after_failure_to_skipped(plan: &BatchPlan, failed_index: usize) -> BatchPlan {
        let mut shrunk = BatchPlan {
            family: plan.family,
            accepted: Vec::new(),
            skipped: plan.skipped.clone(),
            used_argument_bytes: 0,
        };
        let mut failure_seen = false;

        for mutation in &plan.accepted {
            if failure_seen {
                shrunk.skipped.push(skipped(
                    mutation.index,
                    BatchSkipReason::BlockedByBatchBoundary,
                ));
                continue;
            }

            shrunk.used_argument_bytes += mutation.argument_bytes;
            shrunk.accepted.push(mutation.clone());
            if mutation.index == failed_index {
                failure_seen = true;
            }
        }

        shrunk
    }

    fn reported(result: std::result::Result<BatchReport, BatchReportError>) -> BatchReport {
        assert!(result.is_ok(), "unexpected batch-report error: {result:?}");
        match result {
            Ok(report) => report,
            Err(err) => {
                assert_eq!(Some(err), None);
                BatchReport {
                    schema_version: String::new(),
                    family: None,
                    responses: Vec::new(),
                }
            }
        }
    }

    #[test]
    fn classifies_simple_create_as_candidate() {
        let command = Commands::Create(CreateArgs {
            title: Some("claim ready work".to_string()),
            ..CreateArgs::default()
        });

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::Candidate(CompatibleMutation::CreateIssue)
        );
    }

    #[test]
    fn parsed_cli_candidate_shapes_match_allowlist() {
        let cases: &[(&[&str], CompatibleMutation)] = &[
            (
                &["br", "create", "Queueable issue"],
                CompatibleMutation::CreateIssue,
            ),
            (
                &["br", "create", "--title", "Queueable issue"],
                CompatibleMutation::CreateIssue,
            ),
            (
                &["br", "comments", "add", "br-1", "done"],
                CompatibleMutation::AddComment,
            ),
            (
                &["br", "comments", "add", "br-1", "--message", "done"],
                CompatibleMutation::AddComment,
            ),
            (
                &["br", "update", "br-1", "--status", "in_progress"],
                CompatibleMutation::UpdateIssue,
            ),
            (
                &["br", "update", "br-1", "--add-label", "ops"],
                CompatibleMutation::UpdateIssue,
            ),
            (&["br", "close", "br-1"], CompatibleMutation::CloseIssue),
            (&["br", "reopen", "br-1"], CompatibleMutation::ReopenIssue),
            (
                &["br", "dep", "add", "br-1", "br-2"],
                CompatibleMutation::AddDependency,
            ),
            (
                &["br", "dep", "remove", "br-1", "br-2"],
                CompatibleMutation::RemoveDependency,
            ),
        ];

        for (argv, expected) in cases {
            assert_eq!(
                classify_argv(argv),
                CommandCompatibility::Candidate(*expected),
                "argv: {argv:?}"
            );
        }
    }

    #[test]
    fn parsed_cli_direct_only_shapes_match_reasons() {
        let cases: &[(&[&str], DirectOnlyReason)] = &[
            (
                &["br", "create", "--dry-run", "Preview issue"],
                DirectOnlyReason::DryRun,
            ),
            (
                &["br", "create", "--file", "issues.md"],
                DirectOnlyReason::FileInput,
            ),
            (&["br", "create"], DirectOnlyReason::MissingPayload),
            (
                &["br", "create", "Child issue", "--parent", "br-parent"],
                DirectOnlyReason::UnsupportedOption,
            ),
            (
                &["br", "comments", "add", "br-1", "--file", "comment.md"],
                DirectOnlyReason::FileInput,
            ),
            (
                &["br", "comments", "add", "br-1"],
                DirectOnlyReason::MissingPayload,
            ),
            (
                &["br", "update", "--status", "open"],
                DirectOnlyReason::MissingExplicitTarget,
            ),
            (&["br", "update", "br-1"], DirectOnlyReason::MissingPayload),
            (
                &["br", "update", "br-1", "--parent", "br-parent"],
                DirectOnlyReason::UnsupportedOption,
            ),
            (
                &["br", "close", "br-1", "--suggest-next"],
                DirectOnlyReason::UnsupportedOption,
            ),
            (
                &["br", "dep", "add", "br-1", "br-2", "--metadata", "{}"],
                DirectOnlyReason::UnsupportedOption,
            ),
            (
                &["br", "audit", "record", "--stdin"],
                DirectOnlyReason::UnsafeCommand,
            ),
            (&["br", "sync", "--status"], DirectOnlyReason::UnsafeCommand),
            (
                &["br", "sync", "--flush-only"],
                DirectOnlyReason::UnsafeCommand,
            ),
            (
                &["br", "config", "get", "ui.color"],
                DirectOnlyReason::ReadOnly,
            ),
            (
                &["br", "config", "set", "ui.color", "never"],
                DirectOnlyReason::UnsafeCommand,
            ),
            (&["br", "history", "list"], DirectOnlyReason::ReadOnly),
            (
                &["br", "history", "restore", "issues.backup.jsonl"],
                DirectOnlyReason::UnsafeCommand,
            ),
            (&["br", "doctor"], DirectOnlyReason::UnsafeCommand),
            (
                &["br", "doctor", "--repair"],
                DirectOnlyReason::UnsafeCommand,
            ),
        ];

        for (argv, expected) in cases {
            assert_eq!(
                classify_argv(argv),
                CommandCompatibility::DirectOnly(*expected),
                "argv: {argv:?}"
            );
        }
    }

    #[test]
    fn create_with_bulk_file_stays_direct() {
        let command = Commands::Create(CreateArgs {
            file: Some(PathBuf::from("issues.md")),
            title: Some("bulk".to_string()),
            ..CreateArgs::default()
        });

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::DirectOnly(DirectOnlyReason::FileInput)
        );
    }

    #[test]
    fn create_with_dependency_stays_direct_until_proven() {
        let command = Commands::Create(CreateArgs {
            title: Some("child".to_string()),
            parent: Some("br-parent".to_string()),
            ..CreateArgs::default()
        });

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::DirectOnly(DirectOnlyReason::UnsupportedOption)
        );
    }

    #[test]
    fn comment_add_requires_inline_payload() {
        let command = Commands::Comments(CommentsArgs {
            command: Some(CommentCommands::Add(CommentAddArgs {
                id: "br-1".to_string(),
                text: Vec::new(),
                file: None,
                author: None,
                message: Some("done".to_string()),
            })),
            id: None,
            wrap: false,
        });

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::Candidate(CompatibleMutation::AddComment)
        );
    }

    #[test]
    fn comment_file_input_stays_direct() {
        let command = Commands::Comments(CommentsArgs {
            command: Some(CommentCommands::Add(CommentAddArgs {
                id: "br-1".to_string(),
                text: Vec::new(),
                file: Some(PathBuf::from("comment.md")),
                author: None,
                message: None,
            })),
            id: None,
            wrap: false,
        });

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::DirectOnly(DirectOnlyReason::FileInput)
        );
    }

    #[test]
    fn comment_list_is_read_only() {
        let command = Commands::Comments(CommentsArgs {
            command: Some(CommentCommands::List(CommentListArgs {
                id: "br-1".to_string(),
                wrap: false,
            })),
            id: None,
            wrap: false,
        });

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::DirectOnly(DirectOnlyReason::ReadOnly)
        );
    }

    #[test]
    fn explicit_status_update_is_candidate() {
        let command = Commands::Update(UpdateArgs {
            ids: vec!["br-1".to_string()],
            status: Some("in_progress".to_string()),
            ..UpdateArgs::default()
        });

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::Candidate(CompatibleMutation::UpdateIssue)
        );
    }

    #[test]
    fn implicit_update_target_stays_direct() {
        let command = Commands::Update(UpdateArgs {
            status: Some("in_progress".to_string()),
            ..UpdateArgs::default()
        });

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::DirectOnly(DirectOnlyReason::MissingExplicitTarget)
        );
    }

    #[test]
    fn unsupported_update_option_stays_direct() {
        let command = Commands::Update(UpdateArgs {
            ids: vec!["br-1".to_string()],
            parent: Some("br-parent".to_string()),
            ..UpdateArgs::default()
        });

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::DirectOnly(DirectOnlyReason::UnsupportedOption)
        );
    }

    #[test]
    fn close_and_reopen_need_explicit_ids() {
        let close = Commands::Close(CloseArgs {
            ids: vec!["br-1".to_string()],
            ..CloseArgs::default()
        });
        let reopen = Commands::Reopen(ReopenArgs {
            ids: vec!["br-1".to_string()],
            ..ReopenArgs::default()
        });

        assert_eq!(
            classify_command(&close),
            CommandCompatibility::Candidate(CompatibleMutation::CloseIssue)
        );
        assert_eq!(
            classify_command(&reopen),
            CommandCompatibility::Candidate(CompatibleMutation::ReopenIssue)
        );
    }

    #[test]
    fn close_suggest_next_stays_direct() {
        let command = Commands::Close(CloseArgs {
            ids: vec!["br-1".to_string()],
            suggest_next: true,
            ..CloseArgs::default()
        });

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::DirectOnly(DirectOnlyReason::UnsupportedOption)
        );
    }

    #[test]
    fn dependency_add_and_remove_are_candidates() {
        let add = Commands::Dep {
            command: DepCommands::Add(DepAddArgs {
                issue: "br-1".to_string(),
                depends_on: "br-2".to_string(),
                dep_type: "blocks".to_string(),
                metadata: None,
            }),
        };
        let remove = Commands::Dep {
            command: DepCommands::Remove(DepRemoveArgs {
                issue: "br-1".to_string(),
                depends_on: "br-2".to_string(),
            }),
        };

        assert_eq!(
            classify_command(&add),
            CommandCompatibility::Candidate(CompatibleMutation::AddDependency)
        );
        assert_eq!(
            classify_command(&remove),
            CommandCompatibility::Candidate(CompatibleMutation::RemoveDependency)
        );
    }

    #[test]
    fn dependency_metadata_stays_direct() {
        let command = Commands::Dep {
            command: DepCommands::Add(DepAddArgs {
                issue: "br-1".to_string(),
                depends_on: "br-2".to_string(),
                dep_type: "blocks".to_string(),
                metadata: Some("{\"source\":\"import\"}".to_string()),
            }),
        };

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::DirectOnly(DirectOnlyReason::UnsupportedOption)
        );
    }

    #[test]
    fn dependency_import_stays_direct_file_input() {
        let command = Commands::Dep {
            command: DepCommands::Import(crate::cli::DepImportArgs {
                path: "edges.jsonl".into(),
                robot: false,
            }),
        };

        assert_eq!(
            classify_command(&command),
            CommandCompatibility::DirectOnly(DirectOnlyReason::FileInput)
        );
    }

    #[test]
    fn read_only_dependency_commands_stay_direct() {
        let list = Commands::Dep {
            command: DepCommands::List(DepListArgs {
                issue: "br-1".to_string(),
                direction: crate::cli::DepDirection::Down,
                dep_type: None,
                format: None,
                stats: false,
            }),
        };
        let tree = Commands::Dep {
            command: DepCommands::Tree(DepTreeArgs {
                issue: "br-1".to_string(),
                direction: crate::cli::DepDirection::Down,
                max_depth: 10,
                format: "text".to_string(),
            }),
        };

        assert_eq!(
            classify_command(&list),
            CommandCompatibility::DirectOnly(DirectOnlyReason::ReadOnly)
        );
        assert_eq!(
            classify_command(&tree),
            CommandCompatibility::DirectOnly(DirectOnlyReason::ReadOnly)
        );
    }

    #[test]
    fn read_only_and_unsafe_commands_do_not_enter_queue() {
        assert_eq!(
            classify_command(&Commands::Where),
            CommandCompatibility::DirectOnly(DirectOnlyReason::ReadOnly)
        );
        assert_eq!(
            classify_command(&Commands::Init {
                prefix: None,
                force: false,
                backend: None,
            }),
            CommandCompatibility::DirectOnly(DirectOnlyReason::UnsafeCommand)
        );
    }

    #[test]
    fn envelope_new_sets_schema_and_validates() {
        let envelope = MutationEnvelope::new(
            "request-1",
            "DustyPuma",
            CompatibleMutation::CreateIssue,
            CombinedOutputMode::Json,
            1_900_000_000_000,
            json!({"title": "burst"}),
        );

        assert_eq!(envelope.schema_version, WRITE_COMBINING_SCHEMA_VERSION);
        assert_eq!(envelope.validate(), Ok(()));
    }

    #[test]
    fn envelope_validation_rejects_missing_identity_fields() {
        let mut envelope = MutationEnvelope::new(
            "request-1",
            "DustyPuma",
            CompatibleMutation::CreateIssue,
            CombinedOutputMode::Json,
            1,
            json!({}),
        );

        envelope.idempotency_key = "  ".to_string();
        assert_eq!(
            envelope.validate(),
            Err(EnvelopeValidationError::EmptyIdempotencyKey)
        );

        envelope.idempotency_key = "request-1".to_string();
        envelope.actor.clear();
        assert_eq!(
            envelope.validate(),
            Err(EnvelopeValidationError::EmptyActor)
        );

        envelope.actor = "DustyPuma".to_string();
        envelope.deadline_unix_ms = 0;
        assert_eq!(
            envelope.validate(),
            Err(EnvelopeValidationError::MissingDeadline)
        );
    }

    #[test]
    fn batch_accepts_homogeneous_prefix_in_order() {
        let envelopes = vec![
            envelope(
                "request-1",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "first"}),
            ),
            envelope(
                "request-2",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "second"}),
            ),
        ];

        let plan = planned(plan_batch(&envelopes, BatchLimits::default(), NOW_UNIX_MS));

        assert_eq!(plan.family, Some(CompatibleMutation::CreateIssue));
        assert_eq!(plan.accepted_count(), 2);
        assert_eq!(
            plan.accepted
                .iter()
                .map(|mutation| mutation.index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(plan.skipped.is_empty());
        assert!(plan.used_argument_bytes > 0);
    }

    #[test]
    fn batch_rejects_invalid_and_expired_without_blocking_later_valid() {
        let mut invalid = envelope(
            "invalid",
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({"title": "invalid"}),
        );
        invalid.schema_version = "br.write-combining.v0".to_string();
        let envelopes = vec![
            invalid,
            envelope(
                "expired",
                CompatibleMutation::CreateIssue,
                NOW_UNIX_MS,
                json!({"title": "expired"}),
            ),
            envelope(
                "valid",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "valid"}),
            ),
        ];

        let plan = planned(plan_batch(&envelopes, BatchLimits::default(), NOW_UNIX_MS));

        assert_eq!(plan.accepted_count(), 1);
        assert_eq!(plan.accepted[0].index, 2);
        assert_eq!(
            plan.skipped
                .iter()
                .map(|skipped| &skipped.reason)
                .collect::<Vec<_>>(),
            vec![
                &BatchSkipReason::InvalidEnvelope(
                    EnvelopeValidationError::UnsupportedSchemaVersion
                ),
                &BatchSkipReason::Expired
            ]
        );
    }

    #[test]
    fn batch_rejects_duplicate_idempotency_without_blocking_later_valid() {
        let envelopes = vec![
            envelope(
                "request-1",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "first"}),
            ),
            envelope(
                "request-1",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "duplicate"}),
            ),
            envelope(
                "request-2",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "second"}),
            ),
        ];

        let plan = planned(plan_batch(&envelopes, BatchLimits::default(), NOW_UNIX_MS));

        assert_eq!(plan.accepted_count(), 2);
        assert_eq!(
            plan.accepted
                .iter()
                .map(|mutation| mutation.index)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert_eq!(
            plan.skipped
                .iter()
                .map(|skipped| &skipped.reason)
                .collect::<Vec<_>>(),
            vec![&BatchSkipReason::DuplicateIdempotencyKey {
                idempotency_key: "request-1".to_string(),
            }]
        );
    }

    #[test]
    fn batch_stops_at_family_boundary() {
        let envelopes = vec![
            envelope(
                "create-1",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "first"}),
            ),
            envelope(
                "comment-1",
                CompatibleMutation::AddComment,
                FUTURE_UNIX_MS,
                json!({"id": "br-1", "message": "done"}),
            ),
            envelope(
                "create-2",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "third"}),
            ),
        ];

        let plan = planned(plan_batch(&envelopes, BatchLimits::default(), NOW_UNIX_MS));

        assert_eq!(plan.accepted_count(), 1);
        assert_eq!(
            plan.skipped
                .iter()
                .map(|skipped| &skipped.reason)
                .collect::<Vec<_>>(),
            vec![
                &BatchSkipReason::FamilyBoundary {
                    expected: CompatibleMutation::CreateIssue,
                    found: CompatibleMutation::AddComment,
                },
                &BatchSkipReason::BlockedByBatchBoundary,
            ]
        );
    }

    #[test]
    fn batch_limits_envelope_count_and_blocks_remainder() {
        let envelopes = vec![
            envelope(
                "create-1",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "first"}),
            ),
            envelope(
                "create-2",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "second"}),
            ),
            envelope(
                "create-3",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "third"}),
            ),
            envelope(
                "create-4",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "fourth"}),
            ),
        ];

        let plan = planned(plan_batch(
            &envelopes,
            BatchLimits::new(2, DEFAULT_MAX_COMBINED_ARGUMENT_BYTES),
            NOW_UNIX_MS,
        ));

        assert_eq!(plan.accepted_count(), 2);
        assert_eq!(
            plan.skipped
                .iter()
                .map(|skipped| &skipped.reason)
                .collect::<Vec<_>>(),
            vec![
                &BatchSkipReason::QueueFull,
                &BatchSkipReason::BlockedByBatchBoundary,
            ]
        );
    }

    #[test]
    fn batch_argument_byte_limit_blocks_remainder() {
        let first = envelope(
            "create-1",
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({"title": "a"}),
        );
        let second = envelope(
            "create-2",
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({"title": "this payload does not fit"}),
        );
        let third = envelope(
            "create-3",
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({"title": "c"}),
        );
        let first_bytes = compact_json_len(&first.arguments).unwrap_or(usize::MAX);
        let second_bytes = compact_json_len(&second.arguments).unwrap_or(usize::MAX);
        let envelopes = vec![first, second, third];

        let plan = planned(plan_batch(
            &envelopes,
            BatchLimits::new(8, first_bytes),
            NOW_UNIX_MS,
        ));

        assert_eq!(plan.accepted_count(), 1);
        assert_eq!(plan.used_argument_bytes, first_bytes);
        assert_eq!(
            plan.skipped
                .iter()
                .map(|skipped| &skipped.reason)
                .collect::<Vec<_>>(),
            vec![
                &BatchSkipReason::ArgumentBytesExceeded {
                    used: first_bytes,
                    needed: second_bytes,
                    limit: first_bytes,
                },
                &BatchSkipReason::BlockedByBatchBoundary,
            ]
        );
    }

    #[test]
    fn batch_rejects_zero_limits() {
        assert_eq!(
            plan_batch(&[], BatchLimits::new(0, 1), NOW_UNIX_MS),
            Err(BatchLimitError::ZeroMaxEnvelopes)
        );
        assert_eq!(
            plan_batch(&[], BatchLimits::new(1, 0), NOW_UNIX_MS),
            Err(BatchLimitError::ZeroMaxArgumentBytes)
        );
    }

    #[test]
    fn batch_report_preserves_original_queue_order() {
        let mut invalid = envelope(
            "invalid",
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({"title": "invalid"}),
        );
        invalid.schema_version = "br.write-combining.v0".to_string();
        let envelopes = vec![
            envelope(
                "request-1",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "first"}),
            ),
            invalid,
            envelope(
                "request-2",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "second"}),
            ),
        ];
        let plan = planned(plan_batch(&envelopes, BatchLimits::default(), NOW_UNIX_MS));
        let results = vec![
            mutation_result("request-1", CompatibleMutation::CreateIssue),
            mutation_result("request-2", CompatibleMutation::CreateIssue),
        ];

        let report = reported(assemble_batch_report(&envelopes, &plan, &results));

        assert_eq!(report.schema_version, WRITE_COMBINING_SCHEMA_VERSION);
        assert_eq!(report.family, Some(CompatibleMutation::CreateIssue));
        assert_eq!(report.applied_count(), 2);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(
            report
                .responses
                .iter()
                .map(|response| response.index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            report.responses[1].outcome,
            BatchResponseOutcome::Skipped(BatchSkipReason::InvalidEnvelope(
                EnvelopeValidationError::UnsupportedSchemaVersion
            ))
        );
    }

    #[test]
    fn batch_outcome_summary_preserves_failures_and_flush_failures() {
        let mut invalid = envelope(
            "invalid",
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({"title": "invalid"}),
        );
        invalid.schema_version = "br.write-combining.v0".to_string();
        let envelopes = vec![
            envelope(
                "request-1",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "first"}),
            ),
            envelope(
                "request-2",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "second"}),
            ),
            envelope(
                "request-3",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "third"}),
            ),
            invalid,
        ];
        let plan = planned(plan_batch(&envelopes, BatchLimits::default(), NOW_UNIX_MS));
        let results = vec![
            MutationResult::new(
                "request-1",
                CompatibleMutation::CreateIssue,
                0,
                r#"{"id":"one"}"#,
                "",
                CombinedFlushOutcome::Succeeded,
            ),
            MutationResult::new(
                "request-2",
                CompatibleMutation::CreateIssue,
                2,
                "",
                "validation failed",
                CombinedFlushOutcome::Succeeded,
            ),
            MutationResult::new(
                "request-3",
                CompatibleMutation::CreateIssue,
                0,
                r#"{"id":"three"}"#,
                "flush failed",
                CombinedFlushOutcome::FailedAfterCommit,
            ),
        ];

        let report = reported(assemble_batch_report(&envelopes, &plan, &results));
        let summary = report.outcome_summary();

        assert_eq!(
            summary,
            BatchOutcomeSummary {
                response_count: 4,
                applied_count: 3,
                skipped_count: 1,
                successful_mutations: 2,
                failed_mutations: 1,
                flush_failures: 1,
                first_failed_index: Some(1),
                first_flush_failure_index: Some(2),
            }
        );
        assert_eq!(
            report
                .responses
                .iter()
                .find(|response| response.index == 1)
                .map(|response| &response.outcome),
            Some(&BatchResponseOutcome::Applied(MutationResult::new(
                "request-2",
                CompatibleMutation::CreateIssue,
                2,
                "",
                "validation failed",
                CombinedFlushOutcome::Succeeded,
            )))
        );
        assert_eq!(
            report
                .responses
                .iter()
                .find(|response| response.index == 2)
                .map(|response| &response.outcome),
            Some(&BatchResponseOutcome::Applied(MutationResult::new(
                "request-3",
                CompatibleMutation::CreateIssue,
                0,
                r#"{"id":"three"}"#,
                "flush failed",
                CombinedFlushOutcome::FailedAfterCommit,
            )))
        );
    }

    #[test]
    fn batch_execution_harness_runs_only_accepted_envelopes() {
        let mut invalid = envelope(
            "invalid",
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({"title": "invalid"}),
        );
        invalid.schema_version = "br.write-combining.v0".to_string();
        let envelopes = vec![
            envelope(
                "request-1",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "first"}),
            ),
            invalid,
            envelope(
                "request-2",
                CompatibleMutation::CreateIssue,
                FUTURE_UNIX_MS,
                json!({"title": "second"}),
            ),
            envelope(
                "expired",
                CompatibleMutation::CreateIssue,
                NOW_UNIX_MS,
                json!({"title": "expired"}),
            ),
        ];
        let mut executed_keys = Vec::new();

        let result = execute_batch_with(
            &envelopes,
            BatchLimits::default(),
            NOW_UNIX_MS,
            |envelope| {
                executed_keys.push(envelope.idempotency_key.clone());
                let exit_code = if envelope.idempotency_key == "request-2" {
                    2
                } else {
                    0
                };
                MutationResult::new(
                    envelope.idempotency_key.clone(),
                    envelope.family,
                    exit_code,
                    format!("stdout:{}", envelope.idempotency_key),
                    format!("stderr:{}", envelope.idempotency_key),
                    CombinedFlushOutcome::Succeeded,
                )
            },
        );
        assert!(result.is_ok(), "unexpected execution error: {result:?}");
        let Ok(report) = result else {
            return;
        };

        assert_eq!(executed_keys, vec!["request-1", "request-2"]);
        assert_eq!(
            report.outcome_summary(),
            BatchOutcomeSummary {
                response_count: 4,
                applied_count: 2,
                skipped_count: 2,
                successful_mutations: 1,
                failed_mutations: 1,
                flush_failures: 0,
                first_failed_index: Some(2),
                first_flush_failure_index: None,
            }
        );
        assert_eq!(
            report
                .responses
                .iter()
                .map(|response| response.index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn create_burst_combined_harness_matches_direct_sequential_state() {
        let envelopes = vec![
            create_burst_envelope(1, "First combined create", "task", 1, &["swarm", "claim"]),
            create_burst_envelope(2, "Second combined create", "feature", 2, &["swarm"]),
            create_burst_envelope(
                3,
                "Third combined create",
                "bug",
                0,
                &["swarm", "regression"],
            ),
        ];

        let direct = run_direct_create_burst(&envelopes);
        let combined = run_combined_create_burst(&envelopes);

        assert_eq!(combined.results.len(), envelopes.len());
        assert_eq!(direct.results, combined.results);
        assert_eq!(direct.issues, combined.issues);
        assert_eq!(direct.event_order, combined.event_order);
        assert_eq!(direct.jsonl_bytes, combined.jsonl_bytes);
        assert_eq!(direct.dirty_ids, Vec::<String>::new());
        assert_eq!(combined.dirty_ids, Vec::<String>::new());
        assert_eq!(direct.needs_flush.as_deref(), Some("false"));
        assert_eq!(combined.needs_flush.as_deref(), Some("false"));
        assert_eq!(direct.export_hashes, combined.export_hashes);
    }

    #[test]
    fn command_validation_failure_does_not_create_issue_or_hide_neighbors() {
        let mut invalid = create_burst_envelope(2, "Missing title", "bug", 0, &["bad"]);
        invalid
            .arguments
            .as_object_mut()
            .expect("object payload")
            .remove("title");
        let envelopes = vec![
            create_burst_envelope(1, "First valid create", "task", 1, &["ok"]),
            invalid,
            create_burst_envelope(3, "Third valid create", "feature", 2, &["ok"]),
        ];
        let mut storage = SqliteStorage::open_memory().expect("open storage");
        let mut sequence = 0;

        let report = executed(execute_batch_with(
            &envelopes,
            BatchLimits::default(),
            NOW_UNIX_MS,
            |envelope| {
                if envelope
                    .arguments
                    .get("title")
                    .and_then(JsonValue::as_str)
                    .is_none()
                {
                    return failed_result(
                        envelope,
                        2,
                        "validation error: missing title",
                        CombinedFlushOutcome::NotRequested,
                    );
                }
                sequence += 1;
                apply_test_create(&mut storage, envelope, sequence)
            },
        ));

        assert_eq!(
            report.outcome_summary(),
            BatchOutcomeSummary {
                response_count: 3,
                applied_count: 3,
                skipped_count: 0,
                successful_mutations: 2,
                failed_mutations: 1,
                flush_failures: 0,
                first_failed_index: Some(1),
                first_flush_failure_index: None,
            }
        );
        assert_eq!(
            response_outcome(&report, 1),
            &BatchResponseOutcome::Applied(failed_result(
                &envelopes[1],
                2,
                "validation error: missing title",
                CombinedFlushOutcome::NotRequested,
            ))
        );
        assert_eq!(
            stored_issue_titles(&storage),
            vec![
                "First valid create".to_string(),
                "Third valid create".to_string()
            ]
        );
        assert_eq!(
            stored_unique_event_issue_ids(&storage),
            vec!["bd-burst-001".to_string(), "bd-burst-002".to_string()]
        );
    }

    #[test]
    fn pre_execution_rejections_do_not_touch_storage() {
        let first = create_burst_envelope(1, "First accepted", "task", 1, &["ok"]);
        let mut duplicate = create_burst_envelope(2, "Duplicate skipped", "bug", 0, &["skip"]);
        duplicate.idempotency_key = first.idempotency_key.clone();
        let second = create_burst_envelope(3, "Second accepted", "feature", 2, &["ok"]);
        let mut expired = create_burst_envelope(4, "Expired skipped", "task", 1, &["skip"]);
        expired.deadline_unix_ms = NOW_UNIX_MS;
        let third = create_burst_envelope(5, "Queue-full skipped", "task", 1, &["skip"]);
        let envelopes = vec![first, duplicate, second, expired, third];
        let mut storage = SqliteStorage::open_memory().expect("open storage");
        let mut executed_keys = Vec::new();
        let mut sequence = 0;

        let report = executed(execute_batch_with(
            &envelopes,
            BatchLimits::new(2, DEFAULT_MAX_COMBINED_ARGUMENT_BYTES),
            NOW_UNIX_MS,
            |envelope| {
                executed_keys.push(envelope.idempotency_key.clone());
                sequence += 1;
                apply_test_create(&mut storage, envelope, sequence)
            },
        ));

        assert_eq!(
            executed_keys,
            vec!["create-1".to_string(), "create-3".to_string()]
        );
        assert_eq!(
            response_outcome(&report, 1),
            &BatchResponseOutcome::Skipped(BatchSkipReason::DuplicateIdempotencyKey {
                idempotency_key: "create-1".to_string(),
            })
        );
        assert_eq!(
            response_outcome(&report, 3),
            &BatchResponseOutcome::Skipped(BatchSkipReason::Expired)
        );
        assert_eq!(
            response_outcome(&report, 4),
            &BatchResponseOutcome::Skipped(BatchSkipReason::QueueFull)
        );
        assert_eq!(
            stored_issue_titles(&storage),
            vec!["First accepted".to_string(), "Second accepted".to_string()]
        );
        assert_eq!(
            stored_unique_event_issue_ids(&storage),
            vec!["bd-burst-001".to_string(), "bd-burst-002".to_string()]
        );
    }

    #[test]
    fn storage_failure_rolls_back_current_envelope_and_shrinks_batch() {
        let envelopes = vec![
            create_burst_envelope(1, "First committed", "task", 1, &["ok"]),
            create_burst_envelope(2, "Duplicate storage id", "bug", 0, &["fail"]),
            create_burst_envelope(3, "Never executed", "feature", 2, &["blocked"]),
        ];
        let plan = planned(plan_batch(&envelopes, BatchLimits::default(), NOW_UNIX_MS));
        let mut storage = SqliteStorage::open_memory().expect("open storage");
        let first_envelope = envelopes.first().expect("first envelope");
        let mut results = vec![apply_test_create(&mut storage, first_envelope, 1)];
        let failing_envelope = envelopes.get(1).expect("failing envelope");
        let storage_err = try_apply_test_create(&mut storage, failing_envelope, 1)
            .expect_err("duplicate create should fail");
        results.push(failed_result(
            failing_envelope,
            1,
            format!("storage error: {storage_err}"),
            CombinedFlushOutcome::NotRequested,
        ));

        let shrunk_plan = move_accepted_after_failure_to_skipped(&plan, 1);
        let report = reported(assemble_batch_report(&envelopes, &shrunk_plan, &results));

        assert!(matches!(
            response_outcome(&report, 1),
            BatchResponseOutcome::Applied(result)
                if result.exit_code == 1 && result.stderr.starts_with("storage error:")
        ));
        assert_eq!(
            response_outcome(&report, 2),
            &BatchResponseOutcome::Skipped(BatchSkipReason::BlockedByBatchBoundary)
        );
        assert_eq!(
            report.outcome_summary(),
            BatchOutcomeSummary {
                response_count: 3,
                applied_count: 2,
                skipped_count: 1,
                successful_mutations: 1,
                failed_mutations: 1,
                flush_failures: 0,
                first_failed_index: Some(1),
                first_flush_failure_index: None,
            }
        );
        assert_eq!(
            stored_issue_titles(&storage),
            vec!["First committed".to_string()]
        );
        assert_eq!(
            stored_unique_event_issue_ids(&storage),
            vec!["bd-burst-001".to_string()]
        );
    }

    #[test]
    fn flush_failure_after_commit_preserves_dirty_db_state() {
        let envelopes = vec![
            create_burst_envelope(1, "First committed before flush", "task", 1, &["dirty"]),
            create_burst_envelope(2, "Second committed before flush", "bug", 0, &["dirty"]),
        ];
        let mut storage = SqliteStorage::open_memory().expect("open storage");
        let mut sequence = 0;
        let mut report = executed(execute_batch_with(
            &envelopes,
            BatchLimits::default(),
            NOW_UNIX_MS,
            |envelope| {
                sequence += 1;
                apply_test_create(&mut storage, envelope, sequence)
            },
        ));

        for response in &mut report.responses {
            if let BatchResponseOutcome::Applied(result) = &mut response.outcome {
                result.flush = CombinedFlushOutcome::FailedAfterCommit;
                result.stderr.push_str("auto-flush failed");
            }
        }
        let mut dirty_ids = storage.get_dirty_issue_ids().expect("read dirty ids");
        dirty_ids.sort();

        assert_eq!(
            report.outcome_summary(),
            BatchOutcomeSummary {
                response_count: 2,
                applied_count: 2,
                skipped_count: 0,
                successful_mutations: 2,
                failed_mutations: 0,
                flush_failures: 2,
                first_failed_index: None,
                first_flush_failure_index: Some(0),
            }
        );
        assert_eq!(
            stored_issue_titles(&storage),
            vec![
                "First committed before flush".to_string(),
                "Second committed before flush".to_string()
            ]
        );
        assert_eq!(
            dirty_ids,
            vec!["bd-burst-001".to_string(), "bd-burst-002".to_string()]
        );
        assert_eq!(stored_unique_event_issue_ids(&storage), dirty_ids);
    }

    #[test]
    fn missing_response_after_accept_is_not_reported_as_success() {
        let envelopes = vec![create_burst_envelope(
            1,
            "Accepted before missing response",
            "task",
            1,
            &["probe"],
        )];
        let plan = planned(plan_batch(&envelopes, BatchLimits::default(), NOW_UNIX_MS));
        let mut storage = SqliteStorage::open_memory().expect("open storage");
        let accepted_envelope = envelopes.first().expect("accepted envelope");
        let accepted_key = accepted_envelope.idempotency_key.clone();
        let committed = apply_test_create(&mut storage, accepted_envelope, 1);

        assert_eq!(committed.idempotency_key, accepted_key);
        assert_eq!(
            assemble_batch_report(&envelopes, &plan, &[]),
            Err(BatchReportError::MissingResult {
                index: 0,
                idempotency_key: accepted_key,
            })
        );
        assert_eq!(
            stored_issue_titles(&storage),
            vec!["Accepted before missing response".to_string()]
        );
        assert_eq!(
            stored_unique_event_issue_ids(&storage),
            vec!["bd-burst-001".to_string()]
        );
    }

    #[test]
    fn batch_execution_harness_rejects_bad_limits_and_bad_results() {
        let envelopes = vec![envelope(
            "request-1",
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({"title": "first"}),
        )];

        assert_eq!(
            execute_batch_with(&envelopes, BatchLimits::new(0, 1), NOW_UNIX_MS, |_| {
                mutation_result("request-1", CompatibleMutation::CreateIssue)
            }),
            Err(BatchExecutionError::BatchLimits(
                BatchLimitError::ZeroMaxEnvelopes
            ))
        );
        assert_eq!(
            execute_batch_with(&envelopes, BatchLimits::default(), NOW_UNIX_MS, |_| {
                mutation_result("wrong-key", CompatibleMutation::CreateIssue)
            }),
            Err(BatchExecutionError::Report(
                BatchReportError::MissingResult {
                    index: 0,
                    idempotency_key: "request-1".to_string(),
                }
            ))
        );
    }

    #[test]
    fn batch_report_rejects_missing_unexpected_and_duplicate_results() {
        let envelopes = vec![envelope(
            "request-1",
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({"title": "first"}),
        )];
        let plan = planned(plan_batch(&envelopes, BatchLimits::default(), NOW_UNIX_MS));

        assert_eq!(
            assemble_batch_report(&envelopes, &plan, &[]),
            Err(BatchReportError::MissingResult {
                index: 0,
                idempotency_key: "request-1".to_string(),
            })
        );
        assert_eq!(
            assemble_batch_report(
                &envelopes,
                &plan,
                &[
                    mutation_result("request-1", CompatibleMutation::CreateIssue),
                    mutation_result("extra", CompatibleMutation::CreateIssue),
                ],
            ),
            Err(BatchReportError::UnexpectedResult {
                idempotency_key: "extra".to_string(),
            })
        );
        assert_eq!(
            assemble_batch_report(
                &envelopes,
                &plan,
                &[
                    mutation_result("request-1", CompatibleMutation::CreateIssue),
                    mutation_result("request-1", CompatibleMutation::CreateIssue),
                ],
            ),
            Err(BatchReportError::DuplicateResult {
                idempotency_key: "request-1".to_string(),
            })
        );
    }

    #[test]
    fn batch_report_rejects_family_mismatches() {
        let envelopes = vec![envelope(
            "request-1",
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({"title": "first"}),
        )];
        let mut plan = planned(plan_batch(&envelopes, BatchLimits::default(), NOW_UNIX_MS));

        assert_eq!(
            assemble_batch_report(
                &envelopes,
                &plan,
                &[mutation_result("request-1", CompatibleMutation::AddComment)],
            ),
            Err(BatchReportError::ResultFamilyMismatch {
                idempotency_key: "request-1".to_string(),
                expected: CompatibleMutation::CreateIssue,
                found: CompatibleMutation::AddComment,
            })
        );

        if let Some(first_accepted) = plan.accepted.first_mut() {
            first_accepted.family = CompatibleMutation::AddComment;
        } else {
            assert_eq!(plan.accepted_count(), 1);
        }
        assert_eq!(
            assemble_batch_report(
                &envelopes,
                &plan,
                &[mutation_result(
                    "request-1",
                    CompatibleMutation::CreateIssue
                )],
            ),
            Err(BatchReportError::PlannedFamilyMismatch {
                index: 0,
                expected: CompatibleMutation::CreateIssue,
                found: CompatibleMutation::AddComment,
            })
        );
    }

    #[test]
    fn batch_report_rejects_inconsistent_plan_shape() {
        let envelopes = vec![envelope(
            "request-1",
            CompatibleMutation::CreateIssue,
            FUTURE_UNIX_MS,
            json!({"title": "first"}),
        )];

        assert_eq!(
            assemble_batch_report(&envelopes, &BatchPlan::default(), &[]),
            Err(BatchReportError::IncompletePlan {
                expected: 1,
                planned: 0,
            })
        );

        let out_of_bounds = BatchPlan {
            family: Some(CompatibleMutation::CreateIssue),
            accepted: vec![PlannedMutation {
                index: 1,
                family: CompatibleMutation::CreateIssue,
                argument_bytes: 1,
            }],
            skipped: Vec::new(),
            used_argument_bytes: 1,
        };
        assert_eq!(
            assemble_batch_report(
                &envelopes,
                &out_of_bounds,
                &[mutation_result(
                    "request-1",
                    CompatibleMutation::CreateIssue
                )],
            ),
            Err(BatchReportError::PlannedIndexOutOfBounds {
                index: 1,
                envelope_count: 1,
            })
        );

        let duplicate_index = BatchPlan {
            family: Some(CompatibleMutation::CreateIssue),
            accepted: vec![PlannedMutation {
                index: 0,
                family: CompatibleMutation::CreateIssue,
                argument_bytes: 1,
            }],
            skipped: vec![SkippedMutation {
                index: 0,
                reason: BatchSkipReason::Expired,
            }],
            used_argument_bytes: 1,
        };
        assert_eq!(
            assemble_batch_report(
                &envelopes,
                &duplicate_index,
                &[mutation_result(
                    "request-1",
                    CompatibleMutation::CreateIssue
                )],
            ),
            Err(BatchReportError::DuplicatePlannedIndex { index: 0 })
        );
    }
}
