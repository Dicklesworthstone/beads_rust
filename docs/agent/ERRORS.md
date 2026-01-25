# Errors

Most commands return non-zero exit codes on failure and may emit a structured error envelope.

Example (captured with stderr redirection):

```bash
br show bd-NOTEXIST --format json > /dev/null 2>err.json || true
cat err.json | jq .
```

Shape:

```json
{
  "error": {
    "code": "ISSUE_NOT_FOUND",
    "message": "Issue not found: bd-NOTEXIST",
    "hint": "Run 'br list' to see available issues.",
    "retryable": false,
    "context": { "searched_id": "bd-NOTEXIST" }
  }
}
```

Machine-readable schema:

```bash
br schema error --format json
```
