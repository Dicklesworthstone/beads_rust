#!/bin/sh

# Explicitly install the clone-local dispatcher that provisions br redirects for
# worktrees created with raw `git worktree add`.

set -eu

fail() {
    printf '%s\n' "ERROR: $*" >&2
    exit 1
}

command -v git >/dev/null 2>&1 || fail "git is required"
command -v cmp >/dev/null 2>&1 || fail "cmp is required"

common_dir=$(git rev-parse --git-common-dir 2>/dev/null) ||
    fail "run this activation utility inside the Git repository to configure"

case "$common_dir" in
    /*) ;;
    *)
        common_dir=$(
            CDPATH=
            cd "$common_dir" 2>/dev/null && pwd -P
        ) ||
            fail "cannot resolve the repository's common Git directory"
        ;;
esac

configured_hooks_path=$(git config --get core.hooksPath 2>/dev/null || true)
if [ -n "$configured_hooks_path" ]; then
    fail "core.hooksPath is already configured as '$configured_hooks_path'; preserve that infrastructure and chain this repository's post-checkout behavior manually"
fi

hooks_dir="$common_dir/hooks"
hook_path="$hooks_dir/post-checkout"
if [ -L "$hooks_dir" ]; then
    fail "common hooks directory is a symlink; refusing to install outside $common_dir: $hooks_dir"
fi
if [ -e "$hooks_dir" ] && [ ! -d "$hooks_dir" ]; then
    fail "common hooks path is not a directory: $hooks_dir"
fi
mkdir -p "$hooks_dir" || fail "cannot create the common hooks directory: $hooks_dir"

hook_body=$(cat <<'BR_POST_CHECKOUT'
#!/bin/sh

# `git worktree add` reports a null previous HEAD and a branch-style checkout.
# Ordinary branch switches also report 1 as the third argument, so both parts
# are required to avoid migrating an existing worktree implicitly.
null_sha1=0000000000000000000000000000000000000000
null_sha256=0000000000000000000000000000000000000000000000000000000000000000
if [ "${3:-0}" = "1" ] && { [ "${1:-}" = "$null_sha1" ] || [ "${1:-}" = "$null_sha256" ]; }; then
    if ! br init --redirect >/dev/null 2>&1; then
        printf '%s\n' 'WARNING: br init --redirect failed after checkout; recover manually in this worktree with br init --redirect /exact/path/to/.beads (exact .beads path).' >&2
    fi
fi

# A post-checkout failure cannot undo a worktree Git already registered.
exit 0
BR_POST_CHECKOUT
)

is_installed_dispatcher() {
    [ -f "$hook_path" ] &&
        [ -x "$hook_path" ] &&
        printf '%s\n' "$hook_body" | cmp -s - "$hook_path"
}

if [ -e "$hook_path" ] || [ -L "$hook_path" ]; then
    if is_installed_dispatcher; then
        printf '%s\n' "Worktree redirect hook already active: $hook_path"
        exit 0
    fi
    fail "preserving existing post-checkout hook at $hook_path; chain the reviewed br init --redirect dispatcher manually"
fi

if ! (umask 022; set -C; printf '%s\n' "$hook_body" > "$hook_path") 2>/dev/null; then
    if is_installed_dispatcher; then
        printf '%s\n' "Worktree redirect hook already active: $hook_path"
        exit 0
    fi
    fail "post-checkout appeared concurrently at $hook_path; it was preserved"
fi

chmod 0755 "$hook_path" ||
    fail "dispatcher was written but could not be made executable: $hook_path"

printf '%s\n' "Activated worktree redirect hook: $hook_path"
