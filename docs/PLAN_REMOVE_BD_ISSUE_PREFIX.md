# Plan: Retire `BD_ISSUE_PREFIX` from Issue Creation; Explicit `--prefix` Everywhere

## Status: REVISED — incorporates operator rulings

---

## 1. Inventory of Creation Paths

Every flow that mints a new issue ID, how it gets its prefix today, and what
the required-`--prefix` UX becomes.

### 1a. `bd create` — `src/cli/commands/create.rs`

**Today:** `execute()` (line 57) calls `id_config_from_layer(&layer)` which
merges the config cascade: defaults → DB → legacy-user → user → project →
env (`BD_ISSUE_PREFIX`) → CLI (`--prefix`). If `--prefix` is given, it is
injected into the layer (line 67). The `IdConfig.prefix` is then used to
construct an `IdGenerator` (line 195) which stamps `<prefix>-<hash>` on
every new issue.

The `execute_import()` path (line 416, `--file` bulk create) follows the
exact same pattern: layer → `id_config_from_layer` → `IdGenerator`.

**After the change:** `--prefix` is **mandatory**. No fallback to DB, YAML,
env, or the hardcoded `"bd"` default. If `--prefix` is absent, error:

> `Error: --prefix is required for issue creation.`

No change to `--prefix` UX shape — it remains `--prefix <value>` (long flag,
`Option<String>` in `CreateArgs`). The implementation stops calling
`id_config_from_layer()` for the prefix; instead it reads `args.prefix`
directly and errors when `None`.

### 1b. `bd q` (quick-create) — `src/cli/commands/q.rs`

**Today:** `execute()` (line 39) calls `id_config_from_layer(&layer)` and
uses the result directly in `IdGenerator::new(id_config)` (line 56). There
is **no `--prefix` flag on `QuickArgs`** at all — it has only `title`,
`priority`, `type_`, and `labels`.

**After the change:** Add `--prefix` to `QuickArgs` (mirrors `CreateArgs`).
Mandatory — same error when absent.

### 1c. `bd init` — `src/cli/commands/init.rs`

**Today:** `execute()` accepts `prefix: Option<String>` (line 15). When
`Some`, it normalises and writes `issue_prefix` into the DB config table
(line 54). The config template written to `.beads/config.yaml` has the key
commented out (line 74: `# issue_prefix: bd`).

**After the change:**

- Remove the `prefix` parameter from `init`. Init no longer stores any
  prefix — there is no `issue_prefix` config concept.
- Remove the `storage.set_config("issue_prefix", &normalized)` call
  (line 54).
- Remove the `# issue_prefix: bd` comment from the config template
  (line 74). Replace with a comment like `# See bd create --prefix`.
- The `assert_writable_prefix` call (line 53) goes away with the
  parameter.

### 1d. `sync import` — `src/cli/commands/sync.rs` + `src/sync/mod.rs`

**Today:** `execute_import_jsonl()` (sync.rs line 940) reads
`issue_prefix` from the DB config table. If absent, it auto-detects from
the JSONL file via `detect_prefix_from_jsonl()` or falls back to `"bd"`
(line 951). The `import_from_jsonl` function (sync/mod.rs line 2219) uses
`IdConfig::with_prefix(prefix)` for rename-on-import.

**After the change:**

- Remove the `storage.get_config("issue_prefix")?` read (line 940).
- Remove the fallback to `"bd"` (line 951).
- Keep the JSONL auto-detect path (`detect_prefix_from_jsonl`). This
  inspects actual data, not config — it returns the prefix of the first
  non-tombstone issue in the file.
- Remove the `storage.set_config("issue_prefix", &detected)?` side-effect
  (line 948) — there is no longer a config row to populate.
- **Mixed-prefix JSONL:** `detect_prefix_from_jsonl()` takes the first
  non-tombstone ID's prefix. The inner `import_from_jsonl` (sync/mod.rs
  line 2173) already validates all IDs against the expected prefix and
  errors on mismatches (unless `skip_prefix_validation` or
  `rename_on_import` is set). So mixed-prefix JSONL is already handled:
  it either errors (default), renames (with `--rename-prefix`), or is
  allowed through (with `--force`). No new work needed for this case.

### 1e. `resolve_no_db_prefix` — `src/config/mod.rs` line 444

**Today:** Called when `--no-db` mode opens an in-memory store. Cascade:
project config YAML `issue_prefix` key → `common_prefix_from_jsonl()` →
parent directory name → error.

**After the change:**

- Remove the project-config YAML lookup (line 446) — there is no
  `issue_prefix` key to look up.
- Remove the parent-directory-name fallback (line 457) — a dirname is not
  an explicit prefix.
- Keep `common_prefix_from_jsonl()` — it infers from data, which is valid
  for the no-db read-only mode where we need a prefix to scope the
  in-memory store.
- If `common_prefix_from_jsonl()` returns `None` (empty JSONL or no
  file), the no-db path proceeds without setting `issue_prefix` in the
  in-memory store. The caller (`open_storage_with_cli`, line 408) no
  longer needs to set `issue_prefix` — it was only used by creation
  paths, which now require `--prefix`.
- `common_prefix_from_jsonl()` already errors on mixed prefixes (line
  502: `"Mixed issue prefixes detected in JSONL"`). This is correct
  behaviour.

### 1f. Child-ID creation (create.rs line 161)

Child IDs (`parent.1`, `parent.2`) inherit the parent's prefix by
construction (`child_id(parent_id, next_num)`). No change needed — the
prefix is embedded in the parent ID.

### 1g. `main.rs` auto-import — `src/main.rs` line 314

**Today:** `auto_import_if_stale()` reads `storage.get_config("issue_prefix")?`
to pass as `expected_prefix` to the import function for prefix validation.

**After the change:** Pass `None` for expected_prefix. Auto-import uses
JSONL data to infer the prefix (same as `detect_prefix_from_jsonl`).
Prefix validation during auto-import becomes data-driven rather than
config-driven.

---

## 2. Removal of the `issue_prefix` Config Key

### 2a. Current config cascade (for reference)

```
CLI --prefix  >  env BD_ISSUE_PREFIX  >  project .beads/config.yaml
  >  user ~/.config/beads/config.yaml  >  legacy user  >  DB issue_prefix
  >  hardcoded default "bd"
```

Implemented in `config::load_config()` (config/mod.rs line 813) via
`ConfigLayer::merge_layers`. The env layer is built by `ConfigLayer::from_env()`
(line 635), which maps every `BD_*` env var through `env_key_variants()` —
meaning `BD_ISSUE_PREFIX` silently becomes `issue_prefix` / `issue-prefix` /
`issue.prefix` in the runtime config.

### 2b. What to remove

The `issue_prefix` config key is removed **everywhere**:

1. **`default_config_layer()`** (config/mod.rs line 800): Remove the
   `issue_prefix: "bd"` insertion. This is the hardcoded default.

2. **`ConfigLayer::from_env()` `BD_*` loop** (line 638): The generic loop
   maps `BD_ISSUE_PREFIX` → `issue_prefix`. After removing the key from
   all consumers, this mapping becomes inert. Blocklist it explicitly
   (remove `issue_prefix` / `issue-prefix` / `issue.prefix` from
   `layer.runtime` after the loop) to prevent it from leaking into config
   output.

3. **`id_config_from_layer()`** (line 842): This function currently reads
   `issue_prefix` from the merged layer. With `--prefix` mandatory and no
   config key, this function's prefix lookup becomes dead code. The
   function still serves a purpose for `min_hash_length`,
   `max_hash_length`, and `max_collision_prob`. Change it to return
   `IdConfig` **without a prefix** (prefix field becomes `Option<String>`
   or is removed from `IdConfig`; callers that need a prefix get it from
   `--prefix`).

4. **DB config row:** `init` no longer writes it (§1c). `sync import` no
   longer reads or writes it (§1d). `bd config set issue_prefix=X` will
   still work (generic key-value store) but has no effect — document this
   in the migration note or reject the key in `config set`.

5. **Project YAML key:** Remove `# issue_prefix: bd` from the init
   template. The YAML parser still tolerates unknown keys, so old repos
   with `issue_prefix:` in their YAML won't break — the key is simply
   ignored.

6. **`config show/list`** (`config.rs` line 771, 810, 854): Currently
   displays `_computed.prefix` derived from `id_config_from_layer()`.
   Remove this computed field from the output. If a DB row still exists
   (leftover from a pre-migration init), it shows as any other generic
   config row — that's acceptable.

### 2c. Functions that become dead code

| Function | File | Disposition |
|---|---|---|
| `resolve_no_db_prefix()` | `config/mod.rs:444` | Gut (keep JSONL-detect only, or inline the `common_prefix_from_jsonl` call at the one call site) |
| `id_config_from_layer()` prefix lookup | `config/mod.rs:842` | Strip prefix resolution; keep hash-length config |

### 2d. Optional future: per-checkout default prefix file

The operator could be convinced that a git-ignored per-checkout file
(e.g. `.beads/local-prefix` listed in `.git/info/exclude`, or a key in
`.git/info/beads`) supplying a default prefix for creation commands would
be a nice convenience. **This is explicitly out of scope for this sweep**
but is sketched here for future reference:

- Shape: A plain-text file `.beads/local-prefix` containing one line
  (the prefix string). Added to `.git/info/exclude` or `.gitignore` so it
  is never committed.
- Creation commands check it as a fallback when `--prefix` is absent:
  `--prefix` flag → `.beads/local-prefix` file → error.
- This avoids polluting shared config (DB, YAML, env) while giving
  developers a per-checkout convenience.
- Not designed further in this sweep.

---

## 3. Partial-ID Resolution Without a Default Prefix

### 3a. How resolution works today

`IdResolver` (src/util/id.rs line 555) has a `ResolverConfig.default_prefix`
(defaulting to `"bd"`). Resolution steps:

1. **Exact match** — `exists_fn(normalized_input)`.
2. **Prefix-prepend** — if input has no dash, try
   `default_prefix + "-" + input`; if that exists, return it.
3. **Substring match** — `find_matching_ids()` scans all IDs for a hash
   substring match across all prefixes; returns AmbiguousId on >1 hit.

Step 2 is the only one that uses `default_prefix`. Steps 1 and 3 are
prefix-agnostic.

### 3b. What happens without a default prefix

Step 2 attempts `"bd-" + input` when the user types a bare hash like
`fvzl`. If the DB has `wf2-fvzl`, step 2 fails (no `bd-fvzl`), and step 3
succeeds (substring match finds `wf2-fvzl`). **Partial-ID resolution
already works without the prefix** — step 3 is the workhorse.

Step 2 is an optimisation: it catches `abc123` → `bd-abc123` without
scanning all IDs. With no default prefix anywhere, this optimisation is
unreachable.

### 3c. What changes

**Remove `ResolverConfig.default_prefix` and step 2 entirely.**

- `ResolverConfig` drops the `default_prefix` field. The struct keeps
  `allowed_prefixes` (currently unused but harmless) and
  `allow_substring_match`.
- `IdResolver::resolve()` drops the step-2 prefix-prepend branch
  (lines 630-639). Resolution goes: exact match → substring match →
  error. This is functionally identical for any user who types a bare
  hash, because step 3 already catches it.
- `ResolverConfig::with_prefix()` constructor is removed.
- `ResolverConfig::default()` no longer sets `default_prefix`.
- `IdResolver::with_prefix()`, `IdResolver::default_prefix()` are removed.

**Commands that construct resolvers** — each currently does
`IdResolver::new(ResolverConfig::with_prefix(id_config.prefix))`. They all
change to `IdResolver::new(ResolverConfig::default())` or just
`IdResolver::with_defaults()`:

| Command file | Function |
|---|---|
| `show.rs` | `execute()` line 40 |
| `update.rs` | `build_resolver()` line 206 |
| `close.rs` | `execute()` line 138 |
| `dep.rs` | `execute()` line 37 |
| `defer.rs` | `execute_defer()` line 58, `execute_undefer()` line 202 |
| `reopen.rs` | `execute()` line 60 |
| `graph.rs` | `execute()` line 118 |
| `lint.rs` | `resolve_issues()` line 270 |

These all simplify: stop calling `id_config_from_layer()` for the prefix,
just construct a default resolver. The `id_config_from_layer()` call may
still be needed for other config (hash lengths for creation), but many of
these commands only used it for the prefix.

### 3d. `orphans.rs` git-grep usage

`orphans.rs` (line 63) calls `config::id_config_from_layer(&config_layer).prefix`
to get the prefix for `get_git_commit_refs()`, which builds a regex
`\b(<prefix>-[a-z0-9]+)\b` to scan git log. This is **not** ID resolution —
it's pattern matching in commit messages. Without a default prefix, this
command cannot know which prefix to grep for.

**Resolution:** `orphans` should scan for **all** prefixes present in the DB.
Query `storage.get_all_ids()`, extract the unique prefixes via
`split_prefix_remainder()`, and build a regex alternation
`\b(prefix1|prefix2)-[a-z0-9]+\b`. This is a minor logic change.

### 3e. Performance of step-3 scan

Step 3 (`find_matching_ids`) scans all IDs in the DB via
`storage.get_all_ids()` or `storage.find_ids_by_hash()`. At beads scale
(hundreds to low thousands of issues), this is negligible. The scan is
already the production code path for any input that contains a dash (e.g.
`bd show wf2-fvzl` skips step 2 because the input has a dash). No
performance concern.

### 3f. `find_matching_ids` / `split_prefix_remainder`

These are prefix-agnostic utility functions. No changes needed.

---

## 4. Identity for msg / watch / presence

### 4a. Current direct `env::var("BD_ISSUE_PREFIX")` reads

| File | Function | Purpose |
|---|---|---|
| `messaging.rs` line 105 | `resolve_msg_sender()` | Determines "who am I" for `bd msg` / `bd inbox` / `bd outbox` |
| `watch.rs` line 865 | `resolve_prefix()` | Determines which inbox to monitor |
| `presence.rs` line 34 | `set_state()` | Records agent working/idle state |
| `list.rs` line 27 | `resolve_list_prefix()` | Scopes `bd list` to the agent's own prefix |
| `ready.rs` line 30 | `resolve_ready_prefix()` | Scopes `bd ready` to the agent's own prefix |

### 4b. The existing `BEADS_IDENTITY` / `identity` config key

`ConfigLayer::from_env()` (config/mod.rs line 650) maps `BEADS_IDENTITY`
→ `identity`. `CliOverrides` has an `identity` field (line 688) which is
mapped to the config layer (line 710-711). However:

- **No CLI flag wires to it.** `build_cli_overrides()` in `main.rs`
  (line 406) hardcodes `identity: None`. There is no `--identity` flag
  on the `Cli` struct.
- **No command reads it.** No code calls `get_value(layer, &["identity"])`.
- The `identity` key is classified as a startup key (`is_startup_key`,
  config/mod.rs line 1050), meaning it goes into `layer.startup`, not
  `layer.runtime`.

**The entire `identity` config plumbing is dead code.**

### 4c. `BD_AGENT_ID` as the sole identity source — clean sweep

**`BD_AGENT_ID`** is the sole identity source. No compat fallback.

**Resolution order for identity:**

1. `BD_AGENT_ID` env var.
2. Hard error: `"BD_AGENT_ID is not set. Set it to your agent prefix
   (e.g. BD_AGENT_ID=myagent)."`

**What is removed:**

- All 5 direct `std::env::var("BD_ISSUE_PREFIX")` reads in messaging.rs,
  watch.rs, presence.rs, list.rs, ready.rs.
- The `BEADS_IDENTITY` env-var mapping in `ConfigLayer::from_env()`
  (line 650-651).
- The `identity` field from `CliOverrides` (line 688) and its layer
  insertion (line 710-711).
- The `"identity"` entry from `is_startup_key()` (line 1050).
- No `--identity` CLI flag exists, so nothing to remove there.

**Identity function:** A single `pub fn resolve_agent_identity() -> Result<String>`
in `src/config/mod.rs`:

```rust
pub fn resolve_agent_identity() -> Result<String> {
    match std::env::var("BD_AGENT_ID") {
        Ok(val) if !val.trim().is_empty() => {
            let trimmed = val.trim().to_string();
            if trimmed.eq_ignore_ascii_case(OPERATOR_PREFIX) {
                return Err(BeadsError::validation(
                    "identity",
                    "BD_AGENT_ID=operator is reserved for the human operator.",
                ));
            }
            Ok(trimmed)
        }
        _ => Err(BeadsError::validation(
            "identity",
            "BD_AGENT_ID is not set. Set it to your agent prefix \
             (e.g. BD_AGENT_ID=myagent). If you're the human operator, \
             use `bd admin msg` / `bd admin inbox` instead.",
        )),
    }
}
```

All five call sites invoke this instead of raw `env::var("BD_ISSUE_PREFIX")`.

**External harness migration:** Agent harnesses and skills that currently
export `BD_ISSUE_PREFIX` must be updated to export `BD_AGENT_ID` at
rollout. The operator owns that coordination.

### 4d. `list.rs` and `ready.rs` scoping — OPERATOR RULING: remove scoping

These two commands use `BD_ISSUE_PREFIX` to scope output to the agent's
own prefix. **The operator ruled that identity-based default scoping is
removed entirely** — agent IDs will rarely correspond to issue prefixes
going forward, so scoping list output by identity is confusing, not
helpful.

Concretely for `list.rs`: delete `resolve_list_prefix()`'s env read and
the implicit self-scoping behaviour. Default output shows ALL prefixes;
`--prefix <p>` remains as an explicit filter. `--all-prefixes` becomes
redundant (it described an escape from the now-removed default) — remove
the flag or keep it as a hidden no-op alias; prefer removal. Do NOT wire
list to `resolve_agent_identity()`.

The same principle applies generally: no command's default output should
silently scope by agent identity. Identity is for messaging/watch/
presence sender resolution only.

`ready.rs` is noted as being deleted in a parallel task. The plan accounts
for it but implementers should skip it if it's gone by the time they land.

---

## 5. Test / Docs Blast Radius

### 5a. Test files affected

| File | Nature of references | Impact |
|---|---|---|
| `tests/e2e_config_precedence.rs` | Lines 20-21: `set_config("issue_prefix", "DB")`; line 38: `issue_prefix: PROJECT` in YAML; line 72: `BD_ISSUE_PREFIX` env var; lines 155-201: full precedence tests with all layers. | **MAJOR.** Tests must be rewritten or deleted. The `issue_prefix` key no longer exists; `BD_ISSUE_PREFIX` is removed. Tests should be repurposed to test other config keys, or removed if they only tested `issue_prefix` precedence. |
| `tests/conformance.rs` | Lines 9967-9971: asserts `config list` output contains `issue_prefix`. Lines 9987-10076: `config get/set issue_prefix` tests. Lines 10149-10163: asserts `issue_prefix` in config defaults. | **MAJOR.** All `issue_prefix`-specific assertions must be removed or changed to a different config key. The `config set/get` commands are generic (any key works), so tests can use a different key. |
| `tests/e2e_relations.rs` | Lines 222, 337: `issue_prefix: bd` in YAML config; lines 226, 341: `issue_prefix: bd` in YAML config for external projects. | **AFFECTED.** Remove `issue_prefix` from YAML configs. These tests set up external project configs — they may need `--prefix` flags added to create commands, or the tests restructured. |
| `tests/e2e_routing.rs` | Lines 170, 566: `issue_prefix: ext` in external project config YAML. | **AFFECTED.** Same treatment as e2e_relations — remove YAML key, adjust create commands to use `--prefix`. |
| `tests/e2e_ready.rs` | Line 174: `issue_prefix: bd` in YAML; line 177: `issue_prefix: bd` in external config. | **AFFECTED.** Remove YAML key, adjust create commands. |
| `tests/e2e_workspace_commands.rs` | Line 156: comment mentioning `issue_prefix`. | **Minor.** Update or remove the comment. |
| `tests/e2e_workspace_scenarios.rs` | Line 156: `config set issue_prefix=test_prefix`. | **AFFECTED.** Change the test to use a different config key (the test is testing generic config set/get, not issue_prefix specifically). |
| `tests/e2e_queries.rs` | Line 420: checks `config list` output contains `issue_prefix`. | **AFFECTED.** Remove or change the assertion. |
| `tests/proptest_id.rs` | Uses `IdConfig::with_prefix()` (line 206), `IdGenerator::with_defaults()` (line 225), `generate_id()` (line 44). | **AFFECTED.** `IdConfig.prefix` becomes mandatory-at-construction rather than defaulting to `"bd"`. `generate_id()` (the convenience function that uses `IdGenerator::with_defaults()`) needs a prefix parameter, or is removed. Proptests that use `IdConfig::with_prefix()` are fine. The `generate_id()` users (lines 44, 71, 90) need updating. |
| `tests/repro_id_collision.rs` | Uses `IdConfig`. | **Minor.** May need `IdConfig` constructor update if `prefix` becomes non-optional. |
| `tests/storage_id_hash_parity.rs` | Uses `IdConfig`. | **Minor.** Same. |

### 5b. Snapshot files affected

| File | Content | Impact |
|---|---|---|
| `tests/snapshots/.../info_with_schema.snap` | Contains `issue_prefix: bd`. | **AFFECTED.** Must be updated — config output no longer includes `issue_prefix` as a computed field. |
| `tests/snapshots/.../info_json_output.snap` | May contain `issue_prefix`. | **AFFECTED.** Same. |
| `tests/snapshots/.../info_schema_json_output.snap` | May contain `issue_prefix`. | **AFFECTED.** Same. |
| `tests/snapshots/.../info_output.snap` | May contain `issue_prefix`. | **AFFECTED.** Same. |

### 5c. Source-level test blocks affected

| File | Test function | Change needed |
|---|---|---|
| `src/cli/commands/messaging.rs` | `resolve_msg_sender_rejects_missing_env` (line 562) | Error message text changes from `BD_ISSUE_PREFIX` to `BD_AGENT_ID`. Underlying function changes to call `resolve_agent_identity()`. |
| `src/cli/commands/messaging.rs` | `resolve_msg_sender_rejects_empty_env` (line 570) | Same. |
| `src/cli/commands/messaging.rs` | `resolve_msg_sender_rejects_operator` (line 576) | Same. |
| `src/cli/commands/messaging.rs` | `resolve_msg_sender_trims_and_returns` (line 585) | Same. |
| `src/config/mod.rs` | `issue_prefix` precedence tests (lines 1232-1481) | **Delete or rewrite.** These test `issue_prefix` cascading through layers — the key no longer exists. Keep the tests that verify generic layer merging works (by using a different key). |
| `src/config/mod.rs` | `id_config_uses_defaults_when_keys_missing` (line 1943) | **AFFECTED.** `id_config_from_layer` no longer returns a default prefix. |
| `src/config/mod.rs` | `id_config_handles_hyphenated_keys` (line 1954) | **AFFECTED.** Tests prefix lookup by hyphenated key name — no longer relevant. |
| `src/config/mod.rs` | `id_config_accepts_legacy_prefix_key` (line 1969) | **AFFECTED.** Tests the `prefix` key alias — no longer relevant. |
| `src/config/mod.rs` | `id_config_parses_numeric_overrides` (line 1280) | **Minor.** Still tests hash-length overrides, but sets `issue_prefix` in setup. Update setup. |
| `src/cli/commands/create.rs` | `default_config()` test helper (line 698) | **AFFECTED.** Sets `prefix: "bd"`. Callers must provide an explicit prefix to `IdConfig`. |

### 5d. Documentation affected

| File | Line | Change |
|---|---|---|
| `docs/ARCHITECTURE.md` | 385 | Remove `issue_prefix` row from config table; add `BD_AGENT_ID` documentation. |
| `src/cli/mod.rs` | 1201 | WatchArgs: update help text from `BD_ISSUE_PREFIX` to `BD_AGENT_ID`. |
| `src/cli/mod.rs` | 1312 | CreateArgs `--prefix` help: remove "overrides BD_ISSUE_PREFIX env var"; say "required for issue creation". |
| `src/cli/mod.rs` | 1438 | UpdateArgs `--prefix` help: remove "overrides BD_ISSUE_PREFIX env var". |
| `src/cli/mod.rs` | 1686, 1692, 1697 | ListArgs: update `BD_ISSUE_PREFIX` references to `BD_AGENT_ID`. |
| `src/cli/mod.rs` | 2121, 2126 | ReadyArgs: update `BD_ISSUE_PREFIX` references to `BD_AGENT_ID`. |
| `src/cli/mod.rs` | 1001 | Admin msg doc comment: remove `BD_ISSUE_PREFIX` reference. |
| `CLI_SCHEMA.json` | (no current references) | **Unaffected.** |

---

## 6. Sequenced Implementation Steps

Each step is independently landable and includes a verification story.

### Step 1: Add `resolve_agent_identity()` — no compat fallback

**Scope:** `src/config/mod.rs` only.

- Add `pub fn resolve_agent_identity() -> Result<String>` with the
  two-step cascade: `BD_AGENT_ID` → hard error.
- Include the `OPERATOR_PREFIX` rejection (same as current
  `resolve_msg_sender` logic).
- Add unit tests for: valid value, trimming, empty value, missing env,
  operator rejection.
- **No callers yet.** This is a leaf addition.

**Verify:** `cargo test` passes. New unit tests cover all branches.

### Step 2: Wire identity commands to `resolve_agent_identity()`

**Scope:** `src/cli/commands/messaging.rs`, `watch.rs`, `presence.rs`,
`list.rs`, `ready.rs`.

- Replace all 5 `std::env::var("BD_ISSUE_PREFIX")` reads with
  `config::resolve_agent_identity()` (or `.ok()` for list/ready scoping).
- Delete `resolve_msg_sender()` and `resolve_msg_sender_from()` in
  messaging.rs — their logic moves into `resolve_agent_identity()`.
  `execute_msg`, `execute_inbox`, `execute_outbox` call
  `resolve_agent_identity()` directly.
- Update watch.rs `resolve_prefix()`: check `args.prefix` first (CLI
  flag), then fall back to `resolve_agent_identity()`. Remove
  `BD_ISSUE_PREFIX` references from error messages.
- Update presence.rs `set_state()`: replace env-var read with
  `resolve_agent_identity().ok()` (silent no-op when unset, matching
  current behaviour).
- Update list.rs / ready.rs: replace env-var read with
  `resolve_agent_identity().ok()`.
- Update all doc comments referencing `BD_ISSUE_PREFIX` in these files.
- Update messaging.rs unit tests (4 tests).
- If `ready.rs` has been deleted by a parallel task, skip it.

**Verify:** `cargo test`. Manual: `BD_AGENT_ID=test1 bd msg operator "hello"`
works; unset `BD_AGENT_ID` causes `bd msg` to error naming `BD_AGENT_ID`.

### Step 3: Remove `BEADS_IDENTITY` and `identity` config plumbing

**Scope:** `src/config/mod.rs`.

- Remove `BEADS_IDENTITY` env-var mapping from `ConfigLayer::from_env()`
  (line 650-651).
- Remove `identity` field from `CliOverrides` (line 688) and its layer
  insertion (lines 710-711).
- Remove `"identity"` from `is_startup_key()` (line 1050).
- Update `build_cli_overrides()` in `main.rs` (line 410) — remove
  `identity: None`.
- Update unit test `is_startup_key` assertion (line 1661).

**Verify:** `cargo test` passes. `BEADS_IDENTITY=x` has no effect on any
command.

### Step 4: Remove `issue_prefix` config key and `BD_ISSUE_PREFIX` env mapping

**Scope:** `src/config/mod.rs`, `src/cli/commands/config.rs`,
`src/cli/commands/init.rs`, `src/cli/commands/sync.rs`,
`src/cli/commands/info.rs`, `src/cli/commands/where.rs`,
`src/main.rs`.

- **`config/mod.rs`:**
  - `default_config_layer()` (line 800): remove `issue_prefix: "bd"`
    insertion.
  - `ConfigLayer::from_env()` (line 638): blocklist `issue_prefix` /
    `issue-prefix` / `issue.prefix` from the `BD_*` loop output.
  - `id_config_from_layer()` (line 842): remove the prefix lookup.
    Return an `IdConfig` without a prefix (see step 5 for `IdConfig`
    changes). Keep hash-length config lookup.
  - `resolve_no_db_prefix()` (line 444): gut the function — remove
    project-config lookup and dirname fallback. Inline
    `common_prefix_from_jsonl()` call at the one call site
    (`open_storage_with_cli` line 407), or keep as a thin wrapper.
    The in-memory store no longer needs `set_config("issue_prefix", ...)`.
  - `assert_writable_prefix()` (line 45): keep — it's still used by
    creation commands to reject `"operator"` when `--prefix=operator`
    is passed.
- **`init.rs`:** Remove the `prefix` parameter, the
  `set_config("issue_prefix")` call, the `assert_writable_prefix` call.
  Update the config template comment.
- **`sync.rs`:** Remove `get_config("issue_prefix")` read (line 940),
  remove `set_config("issue_prefix")` write (line 948), remove `"bd"`
  fallback (line 951). Keep `detect_prefix_from_jsonl()`.
- **`info.rs`:** Remove `issue_prefix` display logic (lines 147, 184).
- **`where.rs`:** Remove `get_config("issue_prefix")` (line 74).
  The `detect_prefix` function can still infer from IDs in the DB.
- **`main.rs`:** `auto_import_if_stale` (line 314): pass `None` for
  `expected_prefix` instead of reading from config.
- **`config.rs`:** Remove `_computed.prefix` from config list/show
  output (lines 771, 810, 854).
- **Update tests:**
  - `tests/e2e_config_precedence.rs`: rewrite or delete tests that
    are solely about `issue_prefix` cascading. Repurpose for other keys
    or delete.
  - `tests/conformance.rs`: remove `issue_prefix` assertions from
    `conformance_config_list`, `conformance_config_defaults`. Change
    `conformance_config_get`, `conformance_config_set`,
    `conformance_config_get_after_set` to use a different key (e.g.
    `default_priority`).
  - `tests/e2e_relations.rs`: remove `issue_prefix: bd` from YAML
    configs. Add `--prefix bd` to create commands in these tests.
  - `tests/e2e_routing.rs`: remove `issue_prefix: ext` from YAML.
    Add `--prefix ext` to create commands.
  - `tests/e2e_ready.rs`: remove `issue_prefix: bd` from YAML. Add
    `--prefix bd`.
  - `tests/e2e_workspace_scenarios.rs`: change `config set
    issue_prefix=test_prefix` to a different key.
  - `tests/e2e_queries.rs`: remove `issue_prefix` assertion from
    config list output check.
  - `tests/e2e_workspace_commands.rs`: update comment.
  - Config precedence unit tests in `config/mod.rs` (lines 1232-1481):
    remove all `issue_prefix`-specific tests. Keep the generic layer-
    merge tests, rewritten to use a different key.
  - `id_config_*` unit tests (lines 1943, 1954, 1969): rewrite to test
    hash-length config only.
  - Update all 4 info snapshots that contain `issue_prefix: bd`.

**Verify:** `cargo test`. `bd config list` no longer shows `issue_prefix`.
`bd init` no longer accepts a prefix argument. Existing repos with
`issue_prefix` in their config YAML are unaffected (key is silently
ignored).

### Step 5: Make `--prefix` mandatory for creation; update `IdConfig`/`IdResolver`

**Scope:** `src/util/id.rs`, `src/cli/mod.rs`,
`src/cli/commands/create.rs`, `src/cli/commands/q.rs`,
all resolver-using commands (show, update, close, dep, defer, reopen,
graph, lint), `src/cli/commands/orphans.rs`, `src/sync/mod.rs`.

- **`IdConfig`** (id.rs line 11): `prefix` field stays (generators need
  it), but `IdConfig::default()` no longer sets a default prefix. The
  `Default` impl sets `prefix: String::new()` or `prefix` becomes
  `Option<String>`. `IdConfig::with_prefix()` stays — it's how callers
  supply the prefix.
- **`IdGenerator::with_defaults()`** (line 59): remove or mark as
  `#[cfg(test)]` — production code should not generate IDs without an
  explicit prefix. The convenience `generate_id()` function (line 314)
  is removed or made test-only.
- **`ResolverConfig`** (line 495): remove `default_prefix` field.
  `ResolverConfig::with_prefix()` is removed. `ResolverConfig::default()`
  no longer sets a prefix.
- **`IdResolver`** (line 555): remove step 2 (prefix-prepend, lines
  630-639). Remove `with_prefix()` and `default_prefix()` methods.
- **`CreateArgs`** (cli/mod.rs line 1312): make `--prefix` required.
  Change from `Option<String>` to a required `String`, or keep as
  `Option` and error in the command if `None`.
- **`QuickArgs`** (cli/mod.rs line 1330): add `--prefix` flag (required).
- **`create.rs`:** In `execute()`, read prefix directly from
  `args.prefix`, error if absent. Build `IdConfig::with_prefix(prefix)`.
  Remove the `id_config_from_layer()` call for prefix. Same for
  `execute_import()`.
- **`q.rs`:** Same pattern — read `args.prefix`, error if absent.
- **Resolver-using commands** (show, update, close, dep, defer, reopen,
  graph, lint): change `IdResolver::new(ResolverConfig::with_prefix(...))`
  to `IdResolver::new(ResolverConfig::default())` or
  `IdResolver::with_defaults()`.
- **`orphans.rs`:** Replace single-prefix git-grep with multi-prefix
  scan. Extract unique prefixes from `storage.get_all_ids()`, build
  alternation regex.
- **`sync/mod.rs`** rename-on-import (line 2219): already receives
  `prefix` as a parameter, uses `IdConfig::with_prefix(prefix)`. No
  change needed.
- **Update tests:**
  - `proptest_id.rs`: update `generate_id()` calls (lines 44, 71, 90)
    to use `IdGenerator::new(IdConfig::with_prefix("bd"))` explicitly.
    The `prefix_preserved` proptest (line 199) is already fine.
  - `repro_id_collision.rs`, `storage_id_hash_parity.rs`: update
    `IdConfig` construction if needed.
  - `create.rs` unit tests: update `default_config()` helper. Add test
    for missing `--prefix` error.
  - `id.rs` unit tests: update resolver tests to not use
    `with_prefix()`. Add test verifying resolution works without a
    default prefix (substring match catches bare hashes).

**Verify:** `cargo test`. Manual:
- `bd create "title"` → error: `--prefix is required`.
- `bd create --prefix=myproj "title"` → succeeds.
- `bd q fix the thing` → error: `--prefix is required`.
- `bd q --prefix=myproj fix the thing` → succeeds.
- `bd show fvzl` → finds `wf2-fvzl` via substring match (no step-2
  prefix-prepend needed).

### Step 6: Update help text and documentation

**Scope:** `src/cli/mod.rs`, `docs/ARCHITECTURE.md`, doc comments.

- Update all `--prefix` help strings: remove "overrides BD_ISSUE_PREFIX",
  say "required for issue creation" on create/q.
- Update identity-related help strings (WatchArgs, ListArgs, ReadyArgs)
  to reference `BD_AGENT_ID`.
- Update `docs/ARCHITECTURE.md` config table: remove `issue_prefix` row,
  add `BD_AGENT_ID` documentation.
- Add a "Migration from `BD_ISSUE_PREFIX`" section to
  `docs/AGENT_INTEGRATION.md` (or create the file if absent) noting:
  harnesses must switch to `BD_AGENT_ID`; `--prefix` is mandatory for
  creation; `issue_prefix` config is removed.

**Verify:** `bd create --help`, `bd watch --help` show updated text.
Doc review.
