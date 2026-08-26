use super::Theme;
use crate::cli::{Cli, InheritedOutputMode, OutputFormat, command_requests_robot_json};
use crate::format::{IssueWithCounts, ListPage};
use crate::format::{sanitize_terminal_inline, sanitize_terminal_text};
use chrono::{DateTime, SecondsFormat, Utc};
use rich_rust::prelude::*;
use rich_rust::renderables::Renderable;
use serde::Serialize;
use std::borrow::Cow;
use std::io::{self, IsTerminal, Write};
use std::sync::{Mutex, OnceLock};
use toon_rust::options::KeyFoldingMode;
use toon_rust::{EncodeOptions, JsonValue, StringOrNumberOrBoolOrNull, encode_lines};

/// Central output coordinator that respects robot/json/quiet modes.
///
/// Uses lazy initialization for console and theme to ensure zero overhead
/// in JSON/Quiet modes where rich output is never used.
pub struct OutputContext {
    /// Output mode (always set eagerly - cheap)
    mode: OutputMode,
    /// Terminal width (cached, lazy)
    width: OnceLock<usize>,
    /// Rich console for human-readable output (lazy)
    console: OnceLock<Console>,
    /// Theme for consistent styling (lazy)
    theme: OnceLock<Theme>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Full rich formatting (tables, colors, panels)
    Rich,
    /// Plain text, no ANSI codes (for piping)
    Plain,
    /// JSON output only
    Json,
    /// TOON format (token-optimized object notation)
    Toon,
    /// Minimal output (quiet mode)
    Quiet,
}

const JSON_OUTPUT_BUFFER_CAPACITY: usize = 128 * 1024;

#[derive(Debug, Clone)]
struct OutputSerializationFailure {
    message: String,
    io_kind: Option<io::ErrorKind>,
}

static OUTPUT_SERIALIZATION_FAILURE: Mutex<Option<OutputSerializationFailure>> = Mutex::new(None);

fn record_output_serialization_failure(format: &str, err: &serde_json::Error) {
    if is_broken_pipe_serialization_error(err) {
        return;
    }

    let failure = OutputSerializationFailure {
        message: format!("failed to serialize {format} output: {err}"),
        io_kind: err.io_error_kind(),
    };
    if let Ok(mut recorded) = OUTPUT_SERIALIZATION_FAILURE.lock()
        && recorded.is_none()
    {
        *recorded = Some(failure);
    }
}

pub fn take_output_serialization_failure() -> Option<crate::BeadsError> {
    let Ok(mut recorded) = OUTPUT_SERIALIZATION_FAILURE.lock() else {
        return Some(crate::BeadsError::Io(io::Error::other(
            "output serialization failure tracker was poisoned",
        )));
    };
    let failure = recorded.take()?;
    Some(match failure.io_kind {
        Some(kind) => crate::BeadsError::Io(io::Error::new(kind, failure.message)),
        None => crate::BeadsError::Json(serde_json::Error::io(io::Error::other(failure.message))),
    })
}

/// Deferred process exit code for commands that must still emit their normal
/// (success-shaped) output and persist state, yet signal a detected problem to
/// scripted callers via a non-zero exit code.
///
/// Returning an `Err` from a command handler is not usable for this: it routes
/// through `handle_error`, which (a) prints a second, structured error payload
/// — corrupting the single-stream `--json` contract that already carried the
/// command's real output — and (b) short-circuits *before* the Phase-5
/// auto-flush, so a mutating command's changes would never reach the JSONL.
///
/// Instead, the command prints its output normally, records the intended exit
/// code here, and `main` applies it after auto-flush and storage teardown.
/// Used by `br dep cycles` (cycles present) and `br create -f` (declared
/// dependency edges dropped by cycle detection) — see #368.
static PENDING_EXIT_CODE: Mutex<Option<i32>> = Mutex::new(None);

/// Record a non-zero exit code to be applied by `main` once the command has
/// finished emitting output and any auto-flush has completed. The first
/// recorded code wins; later calls are ignored so the earliest-detected
/// condition is preserved.
pub fn record_pending_exit_code(code: i32) {
    if let Ok(mut recorded) = PENDING_EXIT_CODE.lock()
        && recorded.is_none()
    {
        *recorded = Some(code);
    }
}

/// Take the deferred exit code recorded by a command, if any.
#[must_use]
pub fn take_pending_exit_code() -> Option<i32> {
    PENDING_EXIT_CODE
        .lock()
        .ok()
        .and_then(|mut recorded| recorded.take())
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl CountingWriter {
    const fn len(&self) -> usize {
        self.bytes
    }
}

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_json_array_to_writer<I, T, W>(writer: &mut W, values: I) -> serde_json::Result<()>
where
    I: IntoIterator<Item = T>,
    T: Serialize,
    W: Write,
{
    writer.write_all(b"[").map_err(serde_json::Error::io)?;
    let mut first = true;
    for value in values {
        if first {
            first = false;
        } else {
            writer.write_all(b",").map_err(serde_json::Error::io)?;
        }
        serde_json::to_writer(&mut *writer, &value)?;
    }
    writer.write_all(b"]").map_err(serde_json::Error::io)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JsonArrayPageMeta {
    pub(crate) total: usize,
    pub(crate) limit: usize,
    pub(crate) offset: usize,
    pub(crate) has_more: bool,
}

fn write_json_array_page_to_writer<I, T, W>(
    writer: &mut W,
    array_field: &str,
    values: I,
    meta: JsonArrayPageMeta,
) -> serde_json::Result<()>
where
    I: IntoIterator<Item = T>,
    T: Serialize,
    W: Write,
{
    writer.write_all(b"{").map_err(serde_json::Error::io)?;
    serde_json::to_writer(&mut *writer, array_field)?;
    writer.write_all(b":").map_err(serde_json::Error::io)?;
    write_json_array_to_writer(writer, values)?;
    writer
        .write_all(b",\"total\":")
        .map_err(serde_json::Error::io)?;
    serde_json::to_writer(&mut *writer, &meta.total)?;
    writer
        .write_all(b",\"limit\":")
        .map_err(serde_json::Error::io)?;
    serde_json::to_writer(&mut *writer, &meta.limit)?;
    writer
        .write_all(b",\"offset\":")
        .map_err(serde_json::Error::io)?;
    serde_json::to_writer(&mut *writer, &meta.offset)?;
    writer
        .write_all(b",\"has_more\":")
        .map_err(serde_json::Error::io)?;
    serde_json::to_writer(&mut *writer, &meta.has_more)?;
    writer.write_all(b"}").map_err(serde_json::Error::io)
}

fn write_json_array_count_to_writer<I, T, W>(
    writer: &mut W,
    array_field: &str,
    values: I,
    count_field: &str,
    count: usize,
) -> serde_json::Result<()>
where
    I: IntoIterator<Item = T>,
    T: Serialize,
    W: Write,
{
    writer.write_all(b"{").map_err(serde_json::Error::io)?;
    serde_json::to_writer(&mut *writer, array_field)?;
    writer.write_all(b":").map_err(serde_json::Error::io)?;
    write_json_array_to_writer(writer, values)?;
    writer.write_all(b",").map_err(serde_json::Error::io)?;
    serde_json::to_writer(&mut *writer, count_field)?;
    writer.write_all(b":").map_err(serde_json::Error::io)?;
    serde_json::to_writer(&mut *writer, &count)?;
    writer.write_all(b"}").map_err(serde_json::Error::io)
}

fn write_json_trailer_to_writer<W: Write>(writer: &mut W) -> serde_json::Result<()> {
    writer.write_all(b"\n").map_err(serde_json::Error::io)?;
    writer.flush().map_err(serde_json::Error::io)
}

fn write_toon_lines_to_writer<W: Write>(
    writer: &mut W,
    lines: &[String],
) -> serde_json::Result<()> {
    let mut first = true;
    for line in lines {
        if first {
            first = false;
        } else {
            writer.write_all(b"\n").map_err(serde_json::Error::io)?;
        }
        writer
            .write_all(line.as_bytes())
            .map_err(serde_json::Error::io)?;
    }
    Ok(())
}

fn toon_lines_len(lines: &[String]) -> usize {
    lines.iter().map(String::len).sum::<usize>() + lines.len().saturating_sub(1)
}

fn write_toon_list_page_to_writer<W: Write>(
    writer: &mut W,
    page: &ListPage,
) -> serde_json::Result<()> {
    let mut line = String::new();
    line.push_str("issues[");
    line.push_str(&page.issues.len().to_string());
    line.push_str("]:");
    writer
        .write_all(line.as_bytes())
        .map_err(serde_json::Error::io)?;

    for row in &page.issues {
        write_toon_issue_counts_object_to_writer(writer, row)?;
    }

    write_toon_usize_line(writer, "total", page.total)?;
    write_toon_usize_line(writer, "limit", page.limit)?;
    write_toon_usize_line(writer, "offset", page.offset)?;
    write_toon_bool_line(writer, "has_more", page.has_more)
}

fn list_page_supports_streamed_toon(page: &ListPage) -> bool {
    page.issues
        .iter()
        .all(|row| row.issue.dependencies.is_empty() && row.issue.comments.is_empty())
}

fn write_toon_issue_counts_object_to_writer<W: Write>(
    writer: &mut W,
    row: &IssueWithCounts,
) -> serde_json::Result<()> {
    let issue = &row.issue;
    let mut line = String::new();
    line.push_str("  - id: ");
    push_toon_string_value(&mut line, &issue.id);
    write_toon_newline_and_line(writer, &line)?;

    write_toon_issue_text_fields(writer, &mut line, issue)?;
    write_toon_issue_workflow_fields(writer, &mut line, issue)?;
    write_toon_issue_time_fields(writer, &mut line, issue)?;
    write_toon_issue_source_fields(writer, &mut line, issue)?;
    write_toon_usize_field(writer, &mut line, "dependency_count", row.dependency_count)?;
    write_toon_usize_field(writer, &mut line, "dependent_count", row.dependent_count)
}

fn write_toon_issue_text_fields<W: Write>(
    writer: &mut W,
    line: &mut String,
    issue: &crate::model::Issue,
) -> serde_json::Result<()> {
    write_toon_string_field(writer, line, "title", &issue.title)?;
    write_optional_toon_string_field(writer, line, "description", issue.description.as_deref())?;
    write_optional_toon_string_field(writer, line, "design", issue.design.as_deref())?;
    write_optional_toon_string_field(
        writer,
        line,
        "acceptance_criteria",
        issue.acceptance_criteria.as_deref(),
    )?;
    write_optional_toon_string_field(writer, line, "notes", issue.notes.as_deref())
}

fn write_toon_issue_workflow_fields<W: Write>(
    writer: &mut W,
    line: &mut String,
    issue: &crate::model::Issue,
) -> serde_json::Result<()> {
    write_toon_string_field(writer, line, "status", issue.status.as_str())?;
    write_toon_i32_field(writer, line, "priority", issue.priority.0)?;
    write_toon_string_field(writer, line, "issue_type", issue.issue_type.as_str())?;
    write_optional_toon_string_field(writer, line, "assignee", issue.assignee.as_deref())?;
    write_optional_toon_string_field(writer, line, "owner", issue.owner.as_deref())?;
    write_optional_toon_i32_field(writer, line, "estimated_minutes", issue.estimated_minutes)
}

fn write_toon_issue_time_fields<W: Write>(
    writer: &mut W,
    line: &mut String,
    issue: &crate::model::Issue,
) -> serde_json::Result<()> {
    write_toon_datetime_field(writer, line, "created_at", &issue.created_at)?;
    write_optional_toon_string_field(writer, line, "created_by", issue.created_by.as_deref())?;
    write_toon_datetime_field(writer, line, "updated_at", &issue.updated_at)?;
    write_optional_toon_datetime_field(writer, line, "closed_at", issue.closed_at.as_ref())?;
    write_optional_toon_string_field(writer, line, "close_reason", issue.close_reason.as_deref())?;
    write_optional_toon_string_field(
        writer,
        line,
        "closed_by_session",
        issue.closed_by_session.as_deref(),
    )?;
    write_optional_toon_datetime_field(writer, line, "due_at", issue.due_at.as_ref())?;
    write_optional_toon_datetime_field(writer, line, "defer_until", issue.defer_until.as_ref())
}

fn write_toon_issue_source_fields<W: Write>(
    writer: &mut W,
    line: &mut String,
    issue: &crate::model::Issue,
) -> serde_json::Result<()> {
    write_optional_toon_string_field(writer, line, "external_ref", issue.external_ref.as_deref())?;
    write_optional_toon_string_field(
        writer,
        line,
        "source_system",
        issue.source_system.as_deref(),
    )?;
    write_optional_toon_string_field(writer, line, "source_repo", issue.source_repo.as_deref())?;
    write_optional_toon_string_field(
        writer,
        line,
        "source_repo_path",
        issue.source_repo_path.as_deref(),
    )?;
    write_optional_toon_datetime_field(writer, line, "deleted_at", issue.deleted_at.as_ref())?;
    write_optional_toon_string_field(writer, line, "deleted_by", issue.deleted_by.as_deref())?;
    write_optional_toon_string_field(
        writer,
        line,
        "delete_reason",
        issue.delete_reason.as_deref(),
    )?;
    write_optional_toon_string_field(
        writer,
        line,
        "original_type",
        issue.original_type.as_deref(),
    )?;
    write_toon_i32_field(
        writer,
        line,
        "compaction_level",
        issue.compaction_level.unwrap_or_default(),
    )?;
    write_optional_toon_datetime_field(writer, line, "compacted_at", issue.compacted_at.as_ref())?;
    write_optional_toon_string_field(
        writer,
        line,
        "compacted_at_commit",
        issue.compacted_at_commit.as_deref(),
    )?;
    write_optional_toon_i32_field(writer, line, "original_size", issue.original_size)?;
    write_optional_toon_string_field(writer, line, "sender", issue.sender.as_deref())?;
    write_toon_true_field_if_set(writer, line, "ephemeral", issue.ephemeral)?;
    write_toon_true_field_if_set(writer, line, "pinned", issue.pinned)?;
    write_toon_true_field_if_set(writer, line, "is_template", issue.is_template)?;
    write_toon_labels_field(writer, line, &issue.labels)
}

fn write_toon_newline_and_line<W: Write>(writer: &mut W, line: &str) -> serde_json::Result<()> {
    writer.write_all(b"\n").map_err(serde_json::Error::io)?;
    writer
        .write_all(line.as_bytes())
        .map_err(serde_json::Error::io)
}

fn write_toon_field_with<W, F>(
    writer: &mut W,
    line: &mut String,
    field: &str,
    push_value: F,
) -> serde_json::Result<()>
where
    W: Write,
    F: FnOnce(&mut String),
{
    line.clear();
    line.push_str("    ");
    line.push_str(field);
    line.push_str(": ");
    push_value(line);
    write_toon_newline_and_line(writer, line)
}

fn write_toon_string_field<W: Write>(
    writer: &mut W,
    line: &mut String,
    field: &str,
    value: &str,
) -> serde_json::Result<()> {
    write_toon_field_with(writer, line, field, |line| {
        push_toon_string_value(line, value);
    })
}

fn write_optional_toon_string_field<W: Write>(
    writer: &mut W,
    line: &mut String,
    field: &str,
    value: Option<&str>,
) -> serde_json::Result<()> {
    if let Some(value) = value {
        write_toon_string_field(writer, line, field, value)?;
    }
    Ok(())
}

fn write_toon_i32_field<W: Write>(
    writer: &mut W,
    line: &mut String,
    field: &str,
    value: i32,
) -> serde_json::Result<()> {
    write_toon_field_with(writer, line, field, |line| {
        line.push_str(&value.to_string());
    })
}

fn write_optional_toon_i32_field<W: Write>(
    writer: &mut W,
    line: &mut String,
    field: &str,
    value: Option<i32>,
) -> serde_json::Result<()> {
    if let Some(value) = value {
        write_toon_i32_field(writer, line, field, value)?;
    }
    Ok(())
}

fn write_toon_usize_field<W: Write>(
    writer: &mut W,
    line: &mut String,
    field: &str,
    value: usize,
) -> serde_json::Result<()> {
    write_toon_field_with(writer, line, field, |line| {
        line.push_str(&value.to_string());
    })
}

fn write_toon_datetime_field<W: Write>(
    writer: &mut W,
    line: &mut String,
    field: &str,
    value: &DateTime<Utc>,
) -> serde_json::Result<()> {
    write_toon_field_with(writer, line, field, |line| {
        push_toon_datetime_value(line, value);
    })
}

fn write_optional_toon_datetime_field<W: Write>(
    writer: &mut W,
    line: &mut String,
    field: &str,
    value: Option<&DateTime<Utc>>,
) -> serde_json::Result<()> {
    if let Some(value) = value {
        write_toon_datetime_field(writer, line, field, value)?;
    }
    Ok(())
}

fn write_toon_true_field_if_set<W: Write>(
    writer: &mut W,
    line: &mut String,
    field: &str,
    value: bool,
) -> serde_json::Result<()> {
    if value {
        write_toon_field_with(writer, line, field, |line| line.push_str("true"))?;
    }
    Ok(())
}

fn write_toon_labels_field<W: Write>(
    writer: &mut W,
    line: &mut String,
    labels: &[String],
) -> serde_json::Result<()> {
    if labels.is_empty() {
        return Ok(());
    }

    line.clear();
    line.push_str("    labels[");
    line.push_str(&labels.len().to_string());
    line.push_str("]: ");
    for (index, label) in labels.iter().enumerate() {
        if index > 0 {
            line.push(',');
        }
        push_toon_string_value(line, label);
    }
    write_toon_newline_and_line(writer, line)
}

fn write_toon_usize_line<W: Write>(
    writer: &mut W,
    field: &str,
    value: usize,
) -> serde_json::Result<()> {
    let mut line = String::new();
    line.push_str(field);
    line.push_str(": ");
    line.push_str(&value.to_string());
    write_toon_newline_and_line(writer, &line)
}

fn write_toon_bool_line<W: Write>(
    writer: &mut W,
    field: &str,
    value: bool,
) -> serde_json::Result<()> {
    let mut line = String::new();
    line.push_str(field);
    line.push_str(": ");
    line.push_str(if value { "true" } else { "false" });
    write_toon_newline_and_line(writer, &line)
}

fn push_toon_datetime_value(out: &mut String, value: &DateTime<Utc>) {
    push_toon_string_value(out, &value.to_rfc3339_opts(SecondsFormat::AutoSi, true));
}

fn push_toon_string_value(out: &mut String, value: &str) {
    let sanitized = sanitize_toon_string(value);
    if toon_string_is_safe_unquoted(&sanitized, ',') {
        out.push_str(&sanitized);
        return;
    }

    out.push('"');
    push_toon_escaped_string(out, &sanitized);
    out.push('"');
}

fn push_toon_escaped_string(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
}

fn toon_string_is_safe_unquoted(value: &str, delimiter: char) -> bool {
    !value.is_empty()
        && value.trim() == value
        && !matches!(value, "true" | "false" | "null")
        && !toon_string_is_numeric_like(value)
        && !value.contains(':')
        && !value.contains('"')
        && !value.contains('\\')
        && !value.contains('[')
        && !value.contains(']')
        && !value.contains('{')
        && !value.contains('}')
        && !value.contains('\n')
        && !value.contains('\r')
        && !value.contains('\t')
        && !value.contains(delimiter)
        && !value.starts_with('-')
}

fn toon_string_is_numeric_like(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }

    let mut bytes = trimmed.bytes().peekable();
    if bytes.peek().copied() == Some(b'-') {
        bytes.next();
    }
    if bytes.peek().is_none() {
        return false;
    }

    let mut digit_count = 0usize;
    let mut first_digit = None;
    while let Some(byte) = bytes.peek().copied()
        && byte.is_ascii_digit()
    {
        first_digit.get_or_insert(byte);
        digit_count += 1;
        bytes.next();
    }
    if digit_count == 0 {
        return false;
    }
    if digit_count > 1 && first_digit == Some(b'0') {
        return true;
    }

    if bytes.peek().copied() == Some(b'.') {
        bytes.next();
        let mut frac_digits = 0usize;
        while let Some(byte) = bytes.peek().copied()
            && byte.is_ascii_digit()
        {
            frac_digits += 1;
            bytes.next();
        }
        if frac_digits == 0 {
            return false;
        }
    }

    if matches!(bytes.peek().copied(), Some(b'e' | b'E')) {
        bytes.next();
        if matches!(bytes.peek().copied(), Some(b'+' | b'-')) {
            bytes.next();
        }
        let mut exp_digits = 0usize;
        while let Some(byte) = bytes.peek().copied()
            && byte.is_ascii_digit()
        {
            exp_digits += 1;
            bytes.next();
        }
        if exp_digits == 0 {
            return false;
        }
    }

    bytes.next().is_none()
}

fn is_broken_pipe_serialization_error(err: &serde_json::Error) -> bool {
    err.io_error_kind() == Some(io::ErrorKind::BrokenPipe)
}

#[must_use]
fn toon_encode_options() -> EncodeOptions {
    EncodeOptions {
        indent: Some(2),
        delimiter: None,
        key_folding: Some(KeyFoldingMode::Safe),
        flatten_depth: None,
        replacer: None,
    }
}

fn sanitize_toon_value(value: &mut JsonValue) {
    match value {
        JsonValue::Primitive(StringOrNumberOrBoolOrNull::String(value)) => {
            if let Cow::Owned(safe_value) = sanitize_toon_string(value) {
                *value = safe_value;
            }
        }
        JsonValue::Primitive(
            StringOrNumberOrBoolOrNull::Null
            | StringOrNumberOrBoolOrNull::Bool(_)
            | StringOrNumberOrBoolOrNull::Number(_),
        ) => {}
        JsonValue::Array(values) => {
            for value in values {
                sanitize_toon_value(value);
            }
        }
        JsonValue::Object(values) => {
            for (key, value) in values {
                if let Cow::Owned(safe_key) = sanitize_toon_string(key) {
                    *key = safe_key;
                }
                sanitize_toon_value(value);
            }
        }
    }
}

fn sanitize_toon_string(value: &str) -> Cow<'_, str> {
    if ascii_toon_string_is_clean(value).unwrap_or_else(|| {
        value
            .chars()
            .all(|ch| matches!(ch, '\n' | '\t') || !ch.is_control())
    }) {
        return Cow::Borrowed(value);
    }

    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\n' | '\t') || !ch.is_control() {
            escaped.push(ch);
            continue;
        }

        for escaped_char in ch.escape_default() {
            escaped.push(escaped_char);
        }
    }

    Cow::Owned(escaped)
}

fn ascii_toon_string_is_clean(value: &str) -> Option<bool> {
    let mut saw_non_ascii = false;
    for byte in value.bytes() {
        match byte {
            b'\n' | b'\t' => {}
            0x00..=0x1f | 0x7f => return Some(false),
            0x80..=0xff => saw_non_ascii = true,
            _ => {}
        }
    }
    (!saw_non_ascii).then_some(true)
}

impl OutputContext {
    /// Detect output mode from environment and terminal state without CLI args.
    #[must_use]
    pub fn detect() -> Self {
        if let Some(format) = OutputFormat::from_env() {
            return Self::from_output_format(format, false, false);
        }
        Self::from_flags(false, false, false)
    }

    /// Create a context with an explicit mode.
    #[must_use]
    pub fn with_mode(mode: OutputMode) -> Self {
        Self {
            mode,
            width: OnceLock::new(),
            console: OnceLock::new(),
            theme: OnceLock::new(),
        }
    }

    /// Create from CLI global args.
    ///
    /// Only mode is set eagerly; console/theme/width are lazy-initialized
    /// on first access to ensure zero overhead in JSON/Quiet modes.
    #[must_use]
    pub fn from_args(args: &Cli) -> Self {
        Self {
            mode: Self::detect_mode(args),
            width: OnceLock::new(),
            console: OnceLock::new(),
            theme: OnceLock::new(),
        }
    }

    /// Create from CLI-style flags.
    ///
    /// Only mode is set eagerly; console/theme/width are lazy-initialized
    /// on first access to ensure zero overhead in JSON/Quiet modes.
    #[must_use]
    pub fn from_flags(json: bool, quiet: bool, no_color: bool) -> Self {
        let mode = if json {
            OutputMode::Json
        } else if quiet {
            OutputMode::Quiet
        } else if no_color || std::env::var("NO_COLOR").is_ok() || !std::io::stdout().is_terminal()
        {
            OutputMode::Plain
        } else {
            OutputMode::Rich
        };

        Self {
            mode,
            width: OnceLock::new(),
            console: OnceLock::new(),
            theme: OnceLock::new(),
        }
    }

    /// Create from an explicit output format.
    #[must_use]
    pub fn from_output_format(format: OutputFormat, quiet: bool, no_color: bool) -> Self {
        let mode = match format {
            OutputFormat::Json => OutputMode::Json,
            OutputFormat::Toon => OutputMode::Toon,
            OutputFormat::Text | OutputFormat::Csv => {
                if quiet {
                    OutputMode::Quiet
                } else if no_color
                    || std::env::var("NO_COLOR").is_ok()
                    || !std::io::stdout().is_terminal()
                {
                    OutputMode::Plain
                } else {
                    OutputMode::Rich
                }
            }
        };

        Self {
            mode,
            width: OnceLock::new(),
            console: OnceLock::new(),
            theme: OnceLock::new(),
        }
    }

    fn detect_mode(args: &Cli) -> OutputMode {
        Self::detect_mode_with_env(args, OutputFormat::from_env())
    }

    fn detect_mode_with_env(args: &Cli, env_output_format: Option<OutputFormat>) -> OutputMode {
        if args.json || command_requests_robot_json(&args.command) {
            return OutputMode::Json;
        }
        if args.quiet {
            return OutputMode::Quiet;
        }
        if let Some(format) = env_output_format {
            match format {
                OutputFormat::Json => return OutputMode::Json,
                OutputFormat::Toon => return OutputMode::Toon,
                OutputFormat::Text | OutputFormat::Csv => {}
            }
        }
        if args.no_color || std::env::var("NO_COLOR").is_ok() {
            return OutputMode::Plain;
        }
        if !std::io::stdout().is_terminal() {
            return OutputMode::Plain;
        }
        OutputMode::Rich
    }

    /// Lazily create console based on mode.
    fn console(&self) -> &Console {
        self.console.get_or_init(|| match self.mode {
            OutputMode::Rich => Console::new(),
            OutputMode::Plain | OutputMode::Quiet | OutputMode::Json | OutputMode::Toon => {
                Console::builder().no_color().force_terminal(false).build()
            }
        })
    }

    // ─────────────────────────────────────────────────────────────
    // Mode Checks (no lazy initialization needed - mode is always set)
    // ─────────────────────────────────────────────────────────────

    pub fn mode(&self) -> OutputMode {
        self.mode
    }
    pub fn is_rich(&self) -> bool {
        self.mode == OutputMode::Rich
    }
    pub fn is_json(&self) -> bool {
        self.mode == OutputMode::Json
    }
    pub fn is_toon(&self) -> bool {
        self.mode == OutputMode::Toon
    }
    pub fn is_quiet(&self) -> bool {
        self.mode == OutputMode::Quiet
    }
    pub fn is_plain(&self) -> bool {
        self.mode == OutputMode::Plain
    }

    pub const fn inherited_output_mode(&self) -> InheritedOutputMode {
        match self.mode {
            OutputMode::Json => InheritedOutputMode::Json,
            OutputMode::Toon => InheritedOutputMode::Toon,
            OutputMode::Quiet => InheritedOutputMode::Quiet,
            OutputMode::Rich | OutputMode::Plain => InheritedOutputMode::None,
        }
    }

    /// Get terminal width (lazy-initialized).
    pub fn width(&self) -> usize {
        *self.width.get_or_init(|| self.console().width())
    }

    /// Get theme (lazy-initialized).
    ///
    /// In JSON/Quiet modes, this is never called, so theme is never created.
    pub fn theme(&self) -> &Theme {
        self.theme.get_or_init(Theme::default)
    }

    // ─────────────────────────────────────────────────────────────
    // Output Methods
    // ─────────────────────────────────────────────────────────────

    pub fn print(&self, content: &str) {
        let content = sanitize_terminal_text(content);
        match self.mode {
            OutputMode::Rich | OutputMode::Plain => {
                self.console()
                    .print_renderable(&Text::new(content.into_owned()));
            }
            OutputMode::Quiet | OutputMode::Json | OutputMode::Toon => {} // No console access - zero overhead
        }
    }

    pub fn print_line(&self, content: &str) {
        let content = sanitize_terminal_text(content);
        match self.mode {
            OutputMode::Rich => {
                let mut text = Text::new(content.into_owned());
                text.append("\n");
                self.console().print_renderable(&text);
            }
            OutputMode::Plain => println!("{content}"),
            OutputMode::Quiet | OutputMode::Json | OutputMode::Toon => {}
        }
    }

    pub fn render<R: Renderable>(&self, renderable: &R) {
        if self.is_rich() {
            self.console().print_renderable(renderable);
        }
    }

    fn report_serialization_error(&self, format: &str, err: &serde_json::Error) {
        record_output_serialization_failure(format, err);
        if !self.is_quiet() && !is_broken_pipe_serialization_error(err) {
            eprintln!("Error: failed to serialize {format} output: {err}");
        }
    }

    fn json_value<T: serde::Serialize>(
        &self,
        value: &T,
        format: &str,
    ) -> Option<serde_json::Value> {
        match serde_json::to_value(value) {
            Ok(json_value) => Some(json_value),
            Err(err) => {
                self.report_serialization_error(format, &err);
                None
            }
        }
    }

    pub fn json<T: serde::Serialize>(&self, value: &T) {
        if self.is_json() {
            // Stream to stdout to avoid allocating large JSON strings.
            let stdout = io::stdout();
            let mut out = io::BufWriter::with_capacity(JSON_OUTPUT_BUFFER_CAPACITY, stdout.lock());
            if let Err(err) = serde_json::to_writer(&mut out, value) {
                self.report_serialization_error("JSON", &err);
                return;
            }
            if let Err(err) = write_json_trailer_to_writer(&mut out) {
                self.report_serialization_error("JSON", &err);
            }
        }
    }

    pub fn json_array<I, T>(&self, values: I)
    where
        I: IntoIterator<Item = T>,
        T: serde::Serialize,
    {
        if self.is_json() {
            let stdout = io::stdout();
            let mut out = io::BufWriter::with_capacity(JSON_OUTPUT_BUFFER_CAPACITY, stdout.lock());
            if let Err(err) = write_json_array_to_writer(&mut out, values) {
                self.report_serialization_error("JSON", &err);
                return;
            }
            if let Err(err) = write_json_trailer_to_writer(&mut out) {
                self.report_serialization_error("JSON", &err);
            }
        }
    }

    pub(crate) fn json_array_page<I, T>(
        &self,
        array_field: &str,
        values: I,
        meta: JsonArrayPageMeta,
    ) where
        I: IntoIterator<Item = T>,
        T: serde::Serialize,
    {
        if self.is_json() {
            let stdout = io::stdout();
            let mut out = io::BufWriter::with_capacity(JSON_OUTPUT_BUFFER_CAPACITY, stdout.lock());
            if let Err(err) = write_json_array_page_to_writer(&mut out, array_field, values, meta) {
                self.report_serialization_error("JSON", &err);
                return;
            }
            if let Err(err) = write_json_trailer_to_writer(&mut out) {
                self.report_serialization_error("JSON", &err);
            }
        }
    }

    pub(crate) fn json_array_count<I, T>(
        &self,
        array_field: &str,
        values: I,
        count_field: &str,
        count: usize,
    ) where
        I: IntoIterator<Item = T>,
        T: serde::Serialize,
    {
        if self.is_json() {
            let stdout = io::stdout();
            let mut out = io::BufWriter::with_capacity(JSON_OUTPUT_BUFFER_CAPACITY, stdout.lock());
            if let Err(err) =
                write_json_array_count_to_writer(&mut out, array_field, values, count_field, count)
            {
                self.report_serialization_error("JSON", &err);
                return;
            }
            if let Err(err) = write_json_trailer_to_writer(&mut out) {
                self.report_serialization_error("JSON", &err);
            }
        }
    }

    pub(crate) fn toon_list_page_with_stats(&self, page: &ListPage, show_stats: bool) -> bool {
        if !self.is_toon() {
            return false;
        }
        if Self::should_emit_toon_stats(show_stats, std::env::var("TOON_STATS").is_ok()) {
            return false;
        }
        if !list_page_supports_streamed_toon(page) {
            return false;
        }

        let stdout = io::stdout();
        let mut out = io::BufWriter::with_capacity(JSON_OUTPUT_BUFFER_CAPACITY, stdout.lock());
        if let Err(err) = write_toon_list_page_to_writer(&mut out, page) {
            self.report_serialization_error("TOON", &err);
            return true;
        }
        if let Err(err) = write_json_trailer_to_writer(&mut out) {
            self.report_serialization_error("TOON", &err);
        }
        true
    }

    pub fn json_pretty<T: serde::Serialize>(&self, value: &T) {
        if self.is_rich() {
            let Some(json_value) = self.json_value(value, "JSON") else {
                return;
            };
            let json = rich_rust::renderables::Json::new(json_value);
            self.console().print_renderable(&json);
        } else if self.is_json() {
            self.json(value);
        }
    }

    /// Output value as TOON format (token-optimized object notation).
    pub fn toon<T: serde::Serialize>(&self, value: &T) {
        if self.is_toon() {
            let Some(json_value) = self.json_value(value, "TOON") else {
                return;
            };
            let mut toon_value: JsonValue = json_value.into();
            sanitize_toon_value(&mut toon_value);
            let options = Some(toon_encode_options());
            let toon_lines = encode_lines(toon_value, options);
            let stdout = io::stdout();
            let mut out = io::BufWriter::with_capacity(JSON_OUTPUT_BUFFER_CAPACITY, stdout.lock());
            if let Err(err) = write_toon_lines_to_writer(&mut out, &toon_lines) {
                self.report_serialization_error("TOON", &err);
                return;
            }
            if let Err(err) = write_json_trailer_to_writer(&mut out) {
                self.report_serialization_error("TOON", &err);
            }
        }
    }

    const fn should_emit_toon_stats(show_stats: bool, env_enabled: bool) -> bool {
        show_stats || env_enabled
    }

    fn pretty_json_len(value: &serde_json::Value) -> Option<usize> {
        let mut writer = CountingWriter::default();
        let mut serializer = serde_json::Serializer::pretty(&mut writer);
        value.serialize(&mut serializer).ok()?;
        Some(writer.len())
    }

    /// Output value as TOON format with optional stats on stderr.
    pub fn toon_with_stats<T: serde::Serialize>(&self, value: &T, show_stats: bool) {
        if self.is_toon() {
            let Some(json_value) = self.json_value(value, "TOON") else {
                return;
            };
            let mut toon_value: JsonValue = json_value.into();
            sanitize_toon_value(&mut toon_value);
            let emit_stats =
                Self::should_emit_toon_stats(show_stats, std::env::var("TOON_STATS").is_ok());
            let json_chars = if emit_stats {
                let sanitized_json_value: serde_json::Value = toon_value.clone().into();
                Self::pretty_json_len(&sanitized_json_value)
            } else {
                None
            };
            let options = Some(toon_encode_options());
            let toon_lines = encode_lines(toon_value, options);
            let toon_chars = toon_lines_len(&toon_lines);

            if let Some(json_chars) = json_chars {
                let savings = if json_chars > 0 {
                    let diff = json_chars.saturating_sub(toon_chars);
                    diff * 100 / json_chars
                } else {
                    0
                };
                eprintln!(
                    "[stats] JSON: {} chars, TOON: {} chars ({}% savings)",
                    json_chars, toon_chars, savings
                );
            }

            let stdout = io::stdout();
            let mut out = io::BufWriter::with_capacity(JSON_OUTPUT_BUFFER_CAPACITY, stdout.lock());
            if let Err(err) = write_toon_lines_to_writer(&mut out, &toon_lines) {
                self.report_serialization_error("TOON", &err);
                return;
            }
            if let Err(err) = write_json_trailer_to_writer(&mut out) {
                self.report_serialization_error("TOON", &err);
            }
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Semantic Output Methods
    // ─────────────────────────────────────────────────────────────

    pub fn success(&self, message: &str) {
        let message = sanitize_terminal_inline(message);
        match self.mode {
            OutputMode::Rich => {
                let mut text = Text::new("");
                text.append_styled("✓", self.theme().success.clone().bold());
                text.append(" ");
                text.append(message.as_ref());
                text.append("\n");
                self.console().print_renderable(&text);
            }
            OutputMode::Plain => println!("✓ {}", message),
            OutputMode::Quiet | OutputMode::Json | OutputMode::Toon => {} //
        }
    }

    pub fn error(&self, message: &str) {
        let message = sanitize_terminal_text(message);
        match self.mode {
            OutputMode::Rich => {
                let panel = Panel::from_text(message.as_ref())
                    .title(Text::new("Error"))
                    .border_style(self.theme().error.clone());
                self.console().print_renderable(&panel);
            }
            OutputMode::Plain | OutputMode::Quiet => eprintln!("Error: {}", message),
            OutputMode::Json | OutputMode::Toon => {} //
        }
    }

    pub fn warning(&self, message: &str) {
        let message = sanitize_terminal_inline(message);
        match self.mode {
            OutputMode::Rich => {
                let mut text = Text::new("");
                text.append_styled("⚠", self.theme().warning.clone().bold());
                text.append(" ");
                text.append_styled(message.as_ref(), self.theme().warning.clone());
                text.append("\n");
                self.console().print_renderable(&text);
            }
            OutputMode::Plain => eprintln!("Warning: {}", message),
            OutputMode::Quiet | OutputMode::Json | OutputMode::Toon => {} //
        }
    }

    pub fn info(&self, message: &str) {
        let message = sanitize_terminal_inline(message);
        match self.mode {
            OutputMode::Rich => {
                let mut text = Text::new("");
                text.append_styled("ℹ", self.theme().info.clone());
                text.append(" ");
                text.append(message.as_ref());
                text.append("\n");
                self.console().print_renderable(&text);
            }
            OutputMode::Plain => println!("{}", message),
            OutputMode::Quiet | OutputMode::Json | OutputMode::Toon => {} //
        }
    }

    pub fn section(&self, title: &str) {
        let title = sanitize_terminal_inline(title);
        if self.is_rich() {
            let rule =
                Rule::with_title(Text::new(title.into_owned())).style(self.theme().section.clone());
            self.console().print_renderable(&rule);
        } else if self.is_plain() {
            println!("\n─── {} ───\n", title);
        }
    }

    pub fn newline(&self) {
        if !self.is_quiet() && !self.is_json() && !self.is_toon() {
            println!();
        }
    }

    pub fn error_panel(&self, title: &str, description: &str, suggestions: &[&str]) {
        let title = sanitize_terminal_inline(title);
        let description = sanitize_terminal_text(description);
        match self.mode {
            OutputMode::Rich => {
                let mut text = Text::from(description.as_ref());
                text.append("\n\nSuggestions:\n");
                for suggestion in suggestions {
                    let suggestion = sanitize_terminal_inline(suggestion);
                    text.append("• ");
                    text.append(suggestion.as_ref());
                    text.append("\n");
                }

                let panel = Panel::from_rich_text(&text, self.width())
                    .title(Text::new(title.as_ref()))
                    .border_style(self.theme().error.clone());
                self.console().print_renderable(&panel);
            }
            OutputMode::Plain => {
                eprintln!("Error: {} - {}", title, description);
                for suggestion in suggestions {
                    eprintln!("  Suggestion: {}", sanitize_terminal_inline(suggestion));
                }
            }
            OutputMode::Quiet => eprintln!("Error: {}", description),
            OutputMode::Json | OutputMode::Toon => {} //
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::TimeZone;
    use clap::Parser;
    use serde::Serialize;
    use serde::ser::Error as _;
    use serde_json::json;

    struct FailingSerialize;

    impl Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(S::Error::custom("boom"))
        }
    }

    #[test]
    fn detect_mode_uses_env_json_default_when_no_explicit_format_requested() {
        let cli = Cli::parse_from(["br", "count"]);
        assert_eq!(
            OutputContext::detect_mode_with_env(&cli, Some(OutputFormat::Json)),
            OutputMode::Json
        );
    }

    #[test]
    fn detect_mode_uses_env_toon_default_when_no_explicit_format_requested() {
        let cli = Cli::parse_from(["br", "count"]);
        assert_eq!(
            OutputContext::detect_mode_with_env(&cli, Some(OutputFormat::Toon)),
            OutputMode::Toon
        );
    }

    #[test]
    fn detect_mode_quiet_overrides_env_machine_format() {
        let cli = Cli::parse_from(["br", "--quiet", "count"]);
        assert_eq!(
            OutputContext::detect_mode_with_env(&cli, Some(OutputFormat::Json)),
            OutputMode::Quiet
        );
    }

    #[test]
    fn detect_mode_explicit_json_overrides_env_toon_default() {
        let cli = Cli::parse_from(["br", "--json", "count"]);
        assert_eq!(
            OutputContext::detect_mode_with_env(&cli, Some(OutputFormat::Toon)),
            OutputMode::Json
        );
    }

    #[test]
    fn detect_mode_uses_robot_flag_for_sync() {
        let cli = Cli::parse_from(["br", "sync", "--robot"]);
        assert_eq!(
            OutputContext::detect_mode_with_env(&cli, Some(OutputFormat::Text)),
            OutputMode::Json
        );
    }

    #[test]
    fn detect_mode_global_flag_matrix_has_unambiguous_precedence() {
        for quiet in [false, true] {
            for json in [false, true] {
                for robot in [false, true] {
                    for no_color in [false, true] {
                        let mut argv = vec!["br"];
                        if quiet {
                            argv.push("--quiet");
                        }
                        if json {
                            argv.push("--json");
                        }
                        if no_color {
                            argv.push("--no-color");
                        }
                        argv.extend(["sync", "--status"]);
                        if robot {
                            argv.push("--robot");
                        }

                        let cli = Cli::parse_from(argv);
                        let mode = OutputContext::detect_mode_with_env(&cli, None);

                        if json || robot {
                            assert_eq!(
                                mode,
                                OutputMode::Json,
                                "json/robot must override quiet/no-color: quiet={quiet}, json={json}, robot={robot}, no_color={no_color}"
                            );
                        } else if quiet {
                            assert_eq!(
                                mode,
                                OutputMode::Quiet,
                                "quiet must override no-color: quiet={quiet}, json={json}, robot={robot}, no_color={no_color}"
                            );
                        } else if no_color {
                            assert_eq!(
                                mode,
                                OutputMode::Plain,
                                "no-color must force plain output: quiet={quiet}, json={json}, robot={robot}, no_color={no_color}"
                            );
                        } else {
                            assert!(
                                matches!(mode, OutputMode::Rich | OutputMode::Plain),
                                "no explicit output controls should be TTY-dependent, got {mode:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn detect_mode_short_quiet_alias_matches_long_quiet() {
        let short = Cli::parse_from(["br", "-q", "sync", "--status"]);
        let long = Cli::parse_from(["br", "--quiet", "sync", "--status"]);

        assert_eq!(
            OutputContext::detect_mode_with_env(&short, None),
            OutputContext::detect_mode_with_env(&long, None)
        );
    }

    #[test]
    fn should_emit_toon_stats_when_flag_is_set() {
        assert!(OutputContext::should_emit_toon_stats(true, false));
    }

    #[test]
    fn should_emit_toon_stats_when_env_is_set() {
        assert!(OutputContext::should_emit_toon_stats(false, true));
    }

    #[test]
    fn should_not_emit_toon_stats_when_flag_and_env_are_absent() {
        assert!(!OutputContext::should_emit_toon_stats(false, false));
    }

    #[test]
    fn pretty_json_len_matches_pretty_serializer_output() {
        let value = json!({
            "title": "CLI issue",
            "labels": ["cli", "perf"],
            "nested": { "priority": 2, "status": "open" }
        });

        assert_eq!(
            OutputContext::pretty_json_len(&value),
            Some(
                serde_json::to_string_pretty(&value)
                    .expect("JSON serialization failed")
                    .len()
            )
        );
    }

    #[test]
    fn write_json_array_to_writer_matches_materialized_vec_output() {
        #[derive(Serialize)]
        struct Row {
            id: &'static str,
            priority: u8,
        }

        let rows = vec![
            Row {
                id: "beads_rust-alpha",
                priority: 0,
            },
            Row {
                id: "beads_rust-beta",
                priority: 1,
            },
        ];
        let mut streamed = Vec::new();

        write_json_array_to_writer(&mut streamed, rows.iter())
            .expect("streaming JSON array serialization failed");

        assert_eq!(
            streamed,
            serde_json::to_vec(&rows).expect("materialized JSON serialization failed")
        );
    }

    #[test]
    fn write_json_array_to_writer_emits_empty_array() {
        let mut streamed = Vec::new();

        write_json_array_to_writer(&mut streamed, std::iter::empty::<serde_json::Value>())
            .expect("streaming empty JSON array serialization failed");

        assert_eq!(streamed, b"[]");
    }

    #[test]
    fn write_json_array_page_to_writer_matches_materialized_page_output() {
        #[derive(Serialize)]
        struct Row {
            id: &'static str,
            priority: u8,
        }

        #[derive(Serialize)]
        struct Page<'a> {
            issues: &'a [Row],
            total: usize,
            limit: usize,
            offset: usize,
            has_more: bool,
        }

        let rows = vec![
            Row {
                id: "beads_rust-alpha",
                priority: 0,
            },
            Row {
                id: "beads_rust-beta",
                priority: 1,
            },
        ];
        let meta = JsonArrayPageMeta {
            total: 5,
            limit: 2,
            offset: 1,
            has_more: true,
        };
        let mut streamed = Vec::new();

        write_json_array_page_to_writer(&mut streamed, "issues", rows.iter(), meta)
            .expect("streaming JSON page serialization failed");

        let materialized = Page {
            issues: &rows,
            total: meta.total,
            limit: meta.limit,
            offset: meta.offset,
            has_more: meta.has_more,
        };
        assert_eq!(
            streamed,
            serde_json::to_vec(&materialized).expect("materialized JSON page serialization failed")
        );
    }

    #[test]
    fn write_json_array_count_to_writer_matches_materialized_struct_output() {
        #[derive(Serialize)]
        struct QueryList<'a> {
            queries: &'a [serde_json::Value],
            count: usize,
        }

        let rows = vec![json!({"name": "triage"}), json!({"name": "release"})];
        let materialized = QueryList {
            queries: &rows,
            count: rows.len(),
        };
        let mut streamed = Vec::new();

        write_json_array_count_to_writer(
            &mut streamed,
            "queries",
            rows.iter(),
            "count",
            rows.len(),
        )
        .expect("streaming JSON query list serialization failed");

        assert_eq!(
            streamed,
            serde_json::to_vec(&materialized)
                .expect("materialized JSON query list serialization failed")
        );
    }

    #[test]
    fn write_toon_lines_to_writer_matches_materialized_encode_output() {
        let value = json!({
            "issues": [
                { "id": "beads_rust-alpha", "priority": 0 },
                { "id": "beads_rust-beta", "priority": 1 }
            ],
            "count": 2
        });
        let mut toon_value = JsonValue::from(value);
        sanitize_toon_value(&mut toon_value);
        let options = Some(toon_encode_options());
        let materialized = toon_rust::encode(toon_value.clone(), options.clone()).into_bytes();
        let lines = encode_lines(toon_value, options);
        let mut streamed = Vec::new();

        write_toon_lines_to_writer(&mut streamed, &lines)
            .expect("streaming TOON line output failed");

        assert_eq!(streamed, materialized);
        assert_eq!(toon_lines_len(&lines), streamed.len());
    }

    #[test]
    fn write_toon_list_page_to_writer_matches_materialized_encode_output() {
        let closed_at = Utc
            .with_ymd_and_hms(2026, 4, 2, 1, 40, 2)
            .single()
            .expect("valid timestamp");
        let mut first = toon_test_issue("bd-c-00004", "Labeled page row");
        first.assignee = Some("PinkTiger".to_string());
        first.owner = Some("swarm".to_string());
        first.estimated_minutes = Some(42);
        first.closed_at = Some(closed_at);
        first.close_reason = Some("fixed".to_string());
        first.due_at = Some(closed_at);
        first.external_ref = Some("JIRA-42".to_string());
        first.compacted_at_commit = Some("abc123".to_string());
        first.original_size = Some(0);
        first.sender = Some("agent,quoted".to_string());
        first.pinned = true;
        first.labels = vec![
            "perf".to_string(),
            "needs,quotes".to_string(),
            "swarm".to_string(),
        ];
        let page = ListPage {
            issues: vec![
                IssueWithCounts {
                    issue: first,
                    dependency_count: 3,
                    dependent_count: 5,
                },
                IssueWithCounts {
                    issue: toon_test_issue("bd-c-00005", "Sparse page row"),
                    dependency_count: 0,
                    dependent_count: 0,
                },
            ],
            total: 7,
            limit: 2,
            offset: 1,
            has_more: true,
        };
        let mut streamed = Vec::new();

        write_toon_list_page_to_writer(&mut streamed, &page)
            .expect("streaming list-page TOON output failed");

        let mut toon_value =
            JsonValue::from(serde_json::to_value(&page).expect("materialized page JSON failed"));
        sanitize_toon_value(&mut toon_value);
        let lines = encode_lines(toon_value, Some(toon_encode_options()));
        let mut materialized = Vec::new();
        write_toon_lines_to_writer(&mut materialized, &lines)
            .expect("materialized TOON line output failed");

        assert_eq!(streamed, materialized);
        assert!(
            String::from_utf8(streamed)
                .expect("TOON output should be utf8")
                .starts_with("issues[2]:\n  - id: bd-c-00004")
        );
    }

    fn toon_test_issue(id: &str, title: &str) -> Issue {
        let created_at = Utc
            .with_ymd_and_hms(2026, 4, 2, 1, 39, 55)
            .single()
            .expect("valid timestamp");
        let updated_at = Utc
            .with_ymd_and_hms(2026, 4, 2, 1, 39, 56)
            .single()
            .expect("valid timestamp");

        Issue {
            id: id.to_string(),
            title: title.to_string(),
            description: Some("Visible blocked description".to_string()),
            design: Some("0000000000000000000002".to_string()),
            acceptance_criteria: Some("done, quoted".to_string()),
            notes: Some("line\nwith tab\tand carriage\rcontrol".to_string()),
            status: Status::Open,
            priority: Priority(0),
            issue_type: IssueType::Task,
            created_at,
            created_by: Some("bench".to_string()),
            updated_at,
            source_repo: Some(".".to_string()),
            ..Issue::default()
        }
    }

    struct WriteZero;

    impl Write for WriteZero {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Ok(0)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct BrokenPipeOnFlush {
        bytes: Vec<u8>,
    }

    impl Write for BrokenPipeOnFlush {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    #[test]
    fn write_json_array_to_writer_propagates_partial_writer_failure() {
        let err = write_json_array_to_writer(&mut WriteZero, [json!({"id": "bd-a"})].iter())
            .expect_err("partial writer should fail");

        assert_eq!(err.io_error_kind(), Some(io::ErrorKind::WriteZero));
        assert!(!is_broken_pipe_serialization_error(&err));
    }

    #[test]
    fn write_toon_lines_to_writer_propagates_partial_writer_failure() {
        let lines = vec!["items[1]:".to_string(), "  - beads_rust-alpha".to_string()];
        let err =
            write_toon_lines_to_writer(&mut WriteZero, &lines).expect_err("partial writer failed");

        assert_eq!(err.io_error_kind(), Some(io::ErrorKind::WriteZero));
        assert!(!is_broken_pipe_serialization_error(&err));
    }

    #[test]
    fn write_json_trailer_flushes_and_classifies_broken_pipe() {
        let mut writer = BrokenPipeOnFlush { bytes: Vec::new() };
        let err =
            write_json_trailer_to_writer(&mut writer).expect_err("flush should report broken pipe");

        assert_eq!(writer.bytes, b"\n");
        assert_eq!(err.io_error_kind(), Some(io::ErrorKind::BrokenPipe));
        assert!(is_broken_pipe_serialization_error(&err));
    }

    #[test]
    fn sanitize_toon_string_keeps_newline_and_tab_but_escapes_carriage_return() {
        assert_eq!(sanitize_toon_string("line\n\t\rnext"), "line\n\t\\rnext");
    }

    #[test]
    fn sanitize_toon_value_escapes_controls_the_encoder_would_emit_raw() {
        let value = json!({
            "plain": "ok",
            "bad\u{1b}key": "title\u{1b}[2J\u{7}\u{9b}\u{8}\n\t\rend",
            "nested": [
                { "body": "bell\u{7}" }
            ]
        });

        let mut toon_value = JsonValue::from(value);
        sanitize_toon_value(&mut toon_value);
        let toon_output = toon_rust::encode(toon_value, Some(toon_encode_options()));

        for forbidden in ['\u{1b}', '\u{7}', '\u{8}', '\u{9b}', '\r'] {
            assert!(
                !toon_output.contains(forbidden),
                "TOON output contained raw control {forbidden:?}: {toon_output:?}"
            );
        }

        assert!(toon_output.contains("\\u{1b}[2J"));
        assert!(toon_output.contains("\\u{7}"));
        assert!(toon_output.contains("\\u{8}"));
        assert!(toon_output.contains("\\u{9b}"));
        assert!(toon_output.contains("\\n"));
        assert!(toon_output.contains("\\t"));
        assert!(toon_output.contains("\\r"));
    }

    #[test]
    fn sanitize_toon_value_keeps_entries_when_sanitized_keys_collide() {
        let mut toon_value = JsonValue::Object(vec![
            (
                "bad\u{1b}".to_string(),
                JsonValue::Primitive(StringOrNumberOrBoolOrNull::String("first".to_string())),
            ),
            (
                "bad\\u{1b}".to_string(),
                JsonValue::Primitive(StringOrNumberOrBoolOrNull::String("second".to_string())),
            ),
        ]);

        sanitize_toon_value(&mut toon_value);

        let entries = match toon_value {
            JsonValue::Object(entries) => entries,
            JsonValue::Primitive(_) | JsonValue::Array(_) => Vec::new(),
        };
        let keys = entries
            .into_iter()
            .map(|(key, _value)| key)
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec!["bad\\u{1b}".to_string(), "bad\\u{1b}".to_string()]
        );
    }

    #[test]
    fn json_value_returns_none_on_serialize_error() {
        let ctx = OutputContext::from_output_format(OutputFormat::Json, false, true);
        assert!(ctx.json_value(&FailingSerialize, "JSON").is_none());
    }

    fn rich_test_context() -> OutputContext {
        OutputContext {
            mode: OutputMode::Rich,
            width: std::sync::OnceLock::new(),
            console: std::sync::OnceLock::new(),
            theme: std::sync::OnceLock::new(),
        }
    }

    #[test]
    fn rich_status_helpers_emit_trailing_newlines() {
        let ctx = rich_test_context();
        ctx.console().begin_capture();

        ctx.success("created");
        ctx.info("details");
        ctx.warning("careful");

        let rendered: String = ctx
            .console()
            .end_capture()
            .into_iter()
            .map(|segment| segment.text.into_owned())
            .collect();

        assert!(rendered.contains("created\n"));
        assert!(rendered.contains("details\n"));
        assert!(rendered.contains("careful\n"));
    }
}
