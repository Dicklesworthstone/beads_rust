#!/usr/bin/env bash
# Fixture: multiple_br_in_path
# FM: fm-external_artifacts-multiple-br-in-path (P1) — more than one
# executable named `br` on PATH.
#
# The real test binary is still invoked through TOOL_BIN. The fixture only
# prepends synthetic PATH entries so the doctor detector can see duplicate
# operator installs without accidentally running the stubs.

set -euo pipefail
target_dir="${1:?usage: corrupt.sh <target_dir>}"
tool_bin="${TOOL_BIN:-br}"

mkdir -p "$target_dir/bin_a" "$target_dir/bin_b"
cd "$target_dir"
"$tool_bin" init >/dev/null 2>&1

cat > bin_a/br <<'EOF'
#!/usr/bin/env bash
case "${1:-}" in
  --version) echo "br 0.1.45" ;;
  *) echo "fixture stub A: $*" ;;
esac
EOF

cat > bin_b/br <<'EOF'
#!/usr/bin/env bash
case "${1:-}" in
  --version) echo "br 0.2.6" ;;
  *) echo "fixture stub B: $*" ;;
esac
EOF

chmod +x bin_a/br bin_b/br

sha256sum bin_a/br | awk '{print $1}' > .fixture_bin_a_sha256
sha256sum bin_b/br | awk '{print $1}' > .fixture_bin_b_sha256
printf 'BR_DOCTOR_FIXTURE_PATH_PREPEND=%s/bin_a:%s/bin_b\n' \
  "$target_dir" "$target_dir" > .fixture_env

if [ -e .fixture_baseline ]; then
  echo "fixture baseline already exists; expected a fresh workspace" >&2
  exit 1
fi
mkdir -p .fixture_baseline
tar --exclude=.fixture_baseline -cf .fixture_baseline/state.tar .

