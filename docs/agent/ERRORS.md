# Errors

Most commands return non-zero exit codes on failure and may emit a structured error envelope.

**Stream contract (JSON mode):** with `--json`/`--robot`, the machine-readable *result* goes to stdout — successful data on exit `0`, the structured error envelope on non-zero exits. Robot callers read ONE clean, parseable stream for the result; the envelope is never on stderr. stderr carries human diagnostics, `RUST_LOG` tracing output, and non-fatal structured *warnings* (e.g. `{"warning":{"code":"AUTO_FLUSH_FAILED",...}}` after a mutation succeeded but its JSONL export failed — see `docs/TROUBLESHOOTING.md`). (In human mode the reverse holds for errors: they are printed to stderr so stdout stays pipeline-clean.)

Example:

```bash
br show br-NOTEXIST --json > out.json 2>/dev/null || true
jq .error out.json
```

Minimal regression check:

```bash
set +e
br show br-NOTEXIST --json >out.json 2>err.json
status=$?
set -e
test "$status" -eq 3
test ! -s err.json
jq -e '.error.code == "ISSUE_NOT_FOUND"' out.json >/dev/null
```

Shape:

```json
{
  "error": {
    "code": "ISSUE_NOT_FOUND",
    "message": "Issue not found: br-NOTEXIST",
    "hint": "Run 'br list' to see available issues.",
    "retryable": false,
    "context": { "searched_id": "br-NOTEXIST" }
  }
}
```

## Partial-batch failures: two documents on stdout

Since [#336], a command that partially applies a batch (e.g. `br close <blocked> <closeable> --json`) exits non-zero and writes **two** JSON documents to stdout: first the payload document describing what *did* happen, then the error envelope describing what failed:

```console
$ br close t-s4u t-k7x --json; echo "exit=$?"
{"closed":[{"id":"t-k7x",...}],"skipped":[{"id":"t-s4u","reason":"blocked by: t-41y — ..."}]}
{
  "error": {
    "code": "CLOSE_INCOMPLETE",
    "message": "Partially applied: 1 closed, 1 skipped — ...",
    "hint": "Skipped issue(s) have open blocking dependencies. ...",
    "retryable": false,
    "context": { "closed": 1, "skipped": 1, "reason": "..." }
  }
}
exit=3
```

Parse with a streaming JSON deserializer (each document is self-delimiting), or let `jq` handle the concatenated stream natively: `jq -s '.'` collects both documents into an array, so `jq -s '.[0]'` is the payload and `jq -s '.[-1].error'` is the envelope.

**Robust recipe:** on a non-zero exit, parse stdout as a stream of JSON documents. If the last document has an `error` key, that is the envelope; any preceding document is the partial-success payload.

Machine-readable schema:

```bash
br schema error --format json
```

[#336]: https://github.com/Dicklesworthstone/beads_rust/issues/336
