# MCP Agent Mail Error Patterns Research

Research document for second-9fh: Study mcp_agent_mail codebase for agent-friendly error patterns.

**Research Date:** 2026-01-16
**Researcher:** ScarletAnchor
**Source:** /data/projects/mcp_agent_mail

## Summary

mcp_agent_mail demonstrates exceptional patterns for agent-friendly error handling. This document catalogs 10+ patterns with code citations and recommendations for br.

---

## Pattern Catalog

### 1. Structured Error Class

**File:** `src/mcp_agent_mail/app.py:286-301`

```python
class ToolExecutionError(Exception):
    def __init__(self, error_type: str, message: str, *,
                 recoverable: bool = True, data: Optional[dict] = None):
        self.error_type = error_type      # Machine-readable code
        self.recoverable = recoverable    # Can agent retry?
        self.data = data or {}            # Structured context

    def to_payload(self) -> dict[str, Any]:
        return {
            "error": {
                "type": self.error_type,
                "message": str(self),
                "recoverable": self.recoverable,
                "data": self.data,
            }
        }
```

**Key Elements:**
- `error_type`: Machine-readable category (ISSUE_NOT_FOUND, CYCLE_DETECTED)
- `recoverable`: Boolean flag for retry logic
- `data`: Structured context (what was provided, what was expected)

**br Application:** Create `StructuredError` struct with these fields.

---

### 2. Timestamp Validation with Examples

**File:** `src/mcp_agent_mail/app.py:960-1004`

```python
raise ToolExecutionError(
    error_type="INVALID_TIMESTAMP",
    message=(
        f"Invalid {param_name} format: '{raw_value}'. "
        f"Expected ISO-8601 format like '2025-01-15T10:30:00+00:00'. "
        f"Common mistakes: missing timezone (add +00:00 or Z), "
        f"using slashes instead of dashes, or using 12-hour format."
    ),
    recoverable=True,
    data={"provided": raw_value, "expected_format": "YYYY-MM-DDTHH:MM:SS+HH:MM"},
)
```

**Pattern Elements:**
- Shows **what was provided** vs **what was expected**
- Lists **common mistakes** to help agent self-correct
- Provides **format example** in the message

**br Application:** Priority validation should say "Priority must be 0-4 (or P0-P4). You provided: 'high'. Use numeric values: 0=critical, 1=high, 2=medium, 3=low, 4=backlog."

---

### 3. Intent Detection - 6 Categories of Agent Mistakes

**File:** `src/mcp_agent_mail/app.py:1859-1906`

The system detects common agent mistakes before they become errors:

| Mistake Type | Detection | Example Input | Guidance |
|--------------|-----------|---------------|----------|
| PROGRAM_NAME_AS_AGENT | Known list | "claude-code" | "Use 'program' parameter" |
| MODEL_NAME_AS_AGENT | Pattern match | "gpt-4" | "Use 'model' parameter" |
| EMAIL_AS_AGENT | Contains @ | "alice@example.com" | "Agent names are simple identifiers" |
| BROADCAST_ATTEMPT | Special values | "all", "*" | "List specific recipients" |
| DESCRIPTIVE_NAME | Suffix check | "BackendHarmonizer" | "Use adjective+noun like 'BlueLake'" |
| UNIX_USERNAME_AS_AGENT | Lowercase check | "ubuntu" | "Check register_agent response" |

**br Application:** Detect when user provides wrong format for:
- IDs: "Implement feature X" (title, not ID)
- Priority: "high" (string, not 0-4)
- Status: "done" (instead of "closed")
- Type: "story" (instead of "task", "bug", "feature")

---

### 4. O(1) Validation with Precomputed Sets

**File:** `src/mcp_agent_mail/utils.py:189-233`

```python
# Precomputed frozenset for O(1) lookup
_VALID_AGENT_NAMES: frozenset[str] = frozenset(
    f"{adj}{noun}".lower() for adj in ADJECTIVES for noun in NOUNS
)

def validate_agent_name_format(name: str) -> bool:
    return name.lower() in _VALID_AGENT_NAMES
```

**Pattern:** Pre-compute valid values at module load time for fast validation.

**br Application:** Precompute valid status, type, and priority values:
```rust
static VALID_STATUSES: LazyLock<HashSet<&str>> = LazyLock::new(|| {
    ["open", "in_progress", "closed", "tombstone"].into_iter().collect()
});
```

---

### 5. Query Sanitization with Auto-Fix

**File:** `src/mcp_agent_mail/app.py:1058-1121`

```python
def _sanitize_fts_query(query: str) -> str | None:
    """Fix common FTS5 mistakes rather than failing."""

    # Bare wildcards can't search
    if trimmed in {"*", "**", "."}: return None

    # Strip leading wildcards (*foo -> foo)
    if trimmed.startswith("*"):
        return _sanitize_fts_query(trimmed[1:].lstrip())

    # Multiple spaces -> single
    trimmed = re.sub(r" {2,}", " ", trimmed)

    return trimmed
```

**Pattern:** Auto-correct what you can, return empty results only when necessary.

**br Application:** When searching:
- Auto-trim whitespace
- Handle case variations ("Closed" -> "closed")
- Strip accidental prefixes ("bd-" when not needed)

---

### 6. Self-Send Detection Warning

**File:** `src/mcp_agent_mail/app.py:4414-4431`

```python
if sender_name in all_recipients:
    await ctx.info(
        f"[note] You ({sender_name}) are sending a message to yourself. "
        f"This is allowed but usually not intended."
    )
```

**Pattern:** Warn on unusual but valid operations.

**br Application:** Warn when:
- Creating dependency from issue to itself
- Setting assignee to empty string (use --unassign)
- Closing issue that blocks others (list dependents)

---

### 7. Subject Length Warning with Truncation

**File:** `src/mcp_agent_mail/app.py:4425-4431`

```python
if len(subject) > 200:
    await ctx.info(
        f"[warn] Subject is {len(subject)} chars (max: 200). "
        f"Long subjects may be truncated. Consider moving details to body."
    )
    subject = subject[:200]
```

**Pattern:** Warn AND auto-truncate, don't fail.

**br Application:** Title validation:
- Warn if title > 80 chars
- Auto-truncate at 200 with warning
- Don't reject valid but unusual input

---

### 8. Suspicious Pattern Detection

**File:** `src/mcp_agent_mail/app.py:1909-1937`

```python
def _detect_suspicious_file_reservation(pattern: str) -> str | None:
    if p in ("*", "**", "**/*"):
        return "Pattern too broad - would reserve entire project"
    if p.startswith("/"):
        return "Looks like absolute path - use project-relative"
    return None
```

**Pattern:** Proactively detect and warn about potentially problematic inputs.

**br Application:** For search queries:
- Warn if query matches all issues
- Warn if filter combination returns empty
- Suggest refinements

---

### 9. Configuration with Graceful Defaults

**File:** `src/mcp_agent_mail/config.py:238-251`

```python
def _bool(value: str, *, default: bool) -> bool:
    if value.lower() in {"1", "true", "yes", "y"}: return True
    if value.lower() in {"0", "false", "no", "n"}: return False
    return default  # Unknown values fall back to default
```

**Pattern:** Never fail on config parsing - use sensible defaults.

**br Application:** Config values should accept multiple forms:
- Priority: 0, P0, "critical", "crit"
- Status: "open", "o", "Open"
- Boolean: --verbose, -v, --verbose=true

---

### 10. Linked Resource Guidance

**File:** Multiple locations

```python
raise ToolExecutionError(
    "AGENT_NOT_FOUND",
    f"Agent '{name}' not found. To discover agents, use resource://agents/{project_key}."
)
```

**Pattern:** Error messages include links to resources/commands that help.

**br Application:**
- "Issue not found. Run 'br list' to see available issues."
- "Not initialized. Run 'br init' first."
- "Invalid status. Valid values: open, in_progress, closed."

---

## Intent Correction Examples for br

| User Input | Detected Intent | Correction | Result |
|------------|-----------------|------------|--------|
| `--priority high` | Set high priority | "Did you mean --priority 1?" | Suggest or auto-correct |
| `--status done` | Mark as closed | "Did you mean --status closed?" | Suggest |
| `bd-123abc` (ambiguous) | Show issue | "Multiple matches: bd-123abc, bd-123abd. Be more specific." | List options |
| `--type story` | Create feature | "Did you mean --type feature or --type task?" | Suggest alternatives |
| `close` (no ID) | Close current | "Which issue? Run 'br list --status=in_progress'" | Guide to list |

---

## Recommendations for br

### High Priority (P0)

1. **Add StructuredError type** with code, message, recoverable, context
2. **Implement Levenshtein-based ID suggestion** for IssueNotFound
3. **Add "did you mean?" for status/type/priority** using known value lists
4. **Include actionable hints** in all error messages

### Medium Priority (P1)

5. **Add proactive warnings** for unusual operations (self-dependency, empty filter)
6. **Auto-fix common input mistakes** (case, whitespace, synonyms)
7. **Detect common command mistakes** (wrong argument order, missing ID)
8. **Link to help commands** in error messages

### Nice to Have (P2)

9. **Warn before truncation** (long titles, descriptions)
10. **Detect suspicious patterns** in search queries
11. **Track and surface common error patterns** for improvement
12. **Add --explain flag** for verbose error context

---

## Code Citations

All patterns were extracted from `/data/projects/mcp_agent_mail`:

| Pattern | File | Lines |
|---------|------|-------|
| ToolExecutionError | app.py | 286-301 |
| Timestamp validation | app.py | 960-1004 |
| Intent detection | app.py | 1859-1906 |
| O(1) validation | utils.py | 189-233 |
| Query sanitization | app.py | 1058-1121 |
| Self-send warning | app.py | 4414-4431 |
| Subject truncation | app.py | 4425-4431 |
| Suspicious patterns | app.py | 1909-1937 |
| Config defaults | config.py | 238-251 |
