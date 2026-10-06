#!/usr/bin/env bash
# Linux/root regression for GH#533. No accounts or existing trackers changed.
set -euo pipefail
br_bin=$(realpath "${1:?usage: trusted_group_sharing.sh /absolute/path/to/br}")
test "$(id -u)" = 0
command -v setpriv >/dev/null
command -v python3 >/dev/null
fixture=$(mktemp -d /tmp/br-trusted-group.XXXXXXXX)
echo "Disposable fixture: $fixture"
chmod 0755 "$fixture"
mkdir "$fixture/workspace"
chown 61001:61000 "$fixture/workspace"
chmod 2770 "$fixture/workspace"
as_user() {
    local uid=$1 gid=$2
    shift 2
    local user_home="$fixture/home-$uid"
    if [[ ! -d "$user_home" ]]; then
        mkdir "$user_home"
        chown "$uid:$gid" "$user_home"
        chmod 0700 "$user_home"
    fi
    setpriv --reuid "$uid" --regid "$gid" --clear-groups -- env \
        HOME="$user_home" XDG_CONFIG_HOME="$user_home/.config" \
        USER="br-sharing-$uid" LOGNAME="br-sharing-$uid" "$@"
}
cd "$fixture/workspace"
as_user 61001 61000 "$br_bin" init
issue=$(as_user 61001 61000 "$br_bin" create --title "shared tracker regression" --json |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
database=$(realpath .beads/beads.db)
# Administrative provisioning happens only with no live br process.
find .beads -type d -exec chgrp 61000 {} + -exec chmod 2770 {} +
find .beads -type f -exec chgrp 61000 {} + -exec chmod 0660 {} +
if as_user 61002 61000 "$br_bin" show "$issue" >"$fixture/default.log" 2>&1; then
    echo "FAIL: default cross-UID open succeeded" >&2
    exit 1
fi
shared_br() {
    local uid=$1
    shift
    as_user "$uid" 61000 env FSQLITE_TRUSTED_UNIX_DATABASE="$database" \
        FSQLITE_TRUSTED_UNIX_GID=61000 "$br_bin" "$@"
}
shared_br 61002 update "$issue" --status in_progress
shared_br 61001 update "$issue" --priority 1
shared_br 61002 show "$issue" --json >"$fixture/shared.json"
test "$(stat -c '%g:%a' .beads/issues.jsonl)" = '61000:660'
python3 - "$fixture/shared.json" <<'PY'
import json, sys
issue = json.load(open(sys.argv[1]))
if isinstance(issue, list):
    issue = issue[0]
assert issue['status'] == 'in_progress', issue
assert issue['priority'] == 1, issue
PY
for suffix in -fsqlite-ns-gate -fsqlite-ns-use; do
    test "$(stat -c '%u:%g:%a:%h' "$database$suffix")" = '61001:61000:660:1'
done
if as_user 61003 61003 env FSQLITE_TRUSTED_UNIX_DATABASE="$database" \
    FSQLITE_TRUSTED_UNIX_GID=61000 "$br_bin" show "$issue" >"$fixture/outsider.log" 2>&1; then
    echo "FAIL: untrusted UID admitted" >&2
    exit 1
fi
echo 'PASS: both trusted UIDs mutate and reopen; default and outsider refused'
# Leave the disposable fixture and logs for inspection.
