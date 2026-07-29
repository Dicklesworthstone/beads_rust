#!/usr/bin/env bash
# Fixture: root_gitignore_not_writable
# FM: fm-permissions-gitignore-not-writable-blocks-repair (P2)
#
# Initialises a workspace, plants a repo-root `.gitignore` containing
# the canonical `.beads/` shadow rule (so the gitignore_repair fixer
# has nothing to add), then strips owner-write via chmod 0o444.
# Doctor's `permissions.root_gitignore` check should fire warn;
# `--repair` must NOT silently chmod the file (operator intent is
# sacred).

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

# Plant a repo-root .gitignore that:
#   1. Has NO offending `.beads/`-shadowing patterns (see
#      `ROOT_GITIGNORE_OFFENDING_PATTERNS` in doctor.rs:299) so the
#      `doctor.gitignore_repair` fixer is a no-op AND it correctly
#      consults `permissions.root_gitignore` warn before any write.
#   2. Already contains `.doctor/` so `ensure_doctor_in_gitignore`
#      (the documented sole pre-chokepoint carveout in
#      `run_dir.rs:295`) is also a no-op. That carveout does NOT
#      consult the permissions lock — it predates the
#      `permissions.root_gitignore` detector — so we have to pre-
#      satisfy it from corrupt-time to keep the fixture's SACRED
#      INVARIANT assertion honest.
cat > .gitignore <<'GITIGNORE'
# Test fixture root .gitignore (no .beads/ shadowing patterns)
*.log
node_modules/
target/
# br doctor per-run artifacts
.doctor/
GITIGNORE

chmod 0444 .gitignore

# Record the pre-corruption mode so post_undo can verify byte-deterministic
# restoration by the chokepoint snapshot.
stat -c '%a' .gitignore > .fixture_baseline_mode

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .
