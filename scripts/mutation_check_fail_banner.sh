#!/usr/bin/env bash
# Mutation harness for the nonzero-exit failure banner.
#
# Breaks the banner one way at a time and checks the tests notice. Run from the
# repo root.
#
# TWO RULES THIS SCRIPT ENFORCES, both from something that bit someone:
#
# 1. A MUTATION MUST BE PROVEN PRESENT IN THE BYTES THE TEST RAN AGAINST.
#    RED is self-verifying — unmodified code cannot fail its own passing suite —
#    but GREEN has two indistinguishable explanations: a real gap in the suite,
#    or a mutation that never landed. A false "not caught" becomes a published
#    claim that someone else's work is defective, and the accused has to prove a
#    negative about a run they did not perform. So every mutation asserts its own
#    presence (`grep -q`) and aborts loudly if absent.
#
# 2. THE TREE MUST BE PROVEN RESTORED, NOT ASSUMED.
#    `restore` on the happy path is not enough: if this script is killed
#    mid-run (timeout, OOM, lost session — all of which have happened here) the
#    mutation stays in the tree, and the next "commit early and often"
#    checkpoint commits the corruption. Hence the EXIT trap, and a
#    `git status --porcelain` check after each restore.
#
# Never checkpoint between applying a mutation and proving the restore.
set -u
export PATH="/home/toad/bit/beads_rust/.devenv/profile/bin:$PATH"

trap 'git checkout -- src/ 2>/dev/null' EXIT

if [ -n "$(git status --porcelain src/)" ]; then
    echo "REFUSING TO START: src/ is already dirty. Commit or restore first —" >&2
    echo "a mutation run cannot tell its own damage from yours." >&2
    exit 1
fi

restore() {
    git checkout -- src/
    local dirty
    dirty=$(git status --porcelain src/)
    if [ -n "$dirty" ]; then
        echo "RESTORE FAILED — src/ still dirty, DO NOT COMMIT:" >&2
        echo "$dirty" >&2
        exit 1
    fi
}

run_tests() {
    cargo test --test e2e_fail_banner --test e2e_broken_pipe -j 2 -- \
        --test-threads=2 --skip common:: 2>&1 |
        grep -E "^test [a-z_]+ \.\.\.|^test result:|^error"
}

# mutate <name> <python-source> <grep-assert-pattern> <expectation: RED|GREEN>
mutate_and_test() {
    local name="$1" script="$2" proof="$3" expect="$4"
    echo "=================== $name  (expect $expect) ==================="
    if ! python3 -c "$script"; then
        echo "MUTATION SCRIPT FAILED — no finding may be reported from this run" >&2
        restore
        return 1
    fi
    if ! grep -rq -- "$proof" src/; then
        echo "MUTATION NOT PRESENT IN BYTES (grep -q '$proof' failed):" >&2
        echo "any result from this run is INADMISSIBLE" >&2
        restore
        return 1
    fi
    echo "mutation proven present in src/ (grep: $proof)"
    run_tests
    restore
}

# --- M1: banner emitted FIRST instead of last (the placebo implementation) ---
mutate_and_test "M1 banner emitted FIRST (before the error envelope)" '
p="src/main.rs"; s=open(p).read()
old="""    let structured = StructuredError::from_error(err);
    let exit_code = structured.code.exit_code();
"""
new="""    let structured = StructuredError::from_error(err);
    let exit_code = structured.code.exit_code();
    beads_rust::exit::emit_exit_banner(ExitKind::Failure, structured.code.as_str(), exit_code);
"""
assert old in s, "PATTERN ABSENT"
s=s.replace(old,new)
s=s.replace("    exit_with_status(exit_code, ExitKind::Failure, structured.code.as_str());","    std::process::exit(exit_code);")
open(p,"w").write(s)
' 'emit_exit_banner(ExitKind::Failure, structured.code.as_str(), exit_code)' RED

# --- M2: no stdout flush before the banner ---
mutate_and_test "M2 stdout NOT flushed before the banner" '
p="src/exit.rs"; s=open(p).read()
old="    let _ = std::io::stdout().flush();\n"
assert old in s, "PATTERN ABSENT"
s=s.replace(old,"    // MUTANT: flush removed\n",1)
open(p,"w").write(s)
' 'MUTANT: flush removed' RED

# --- M3: one exit path skips the banner (doctor) ---
mutate_and_test "M3 doctor exit path bypasses the funnel" '
p="src/cli/commands/doctor.rs"; s=open(p).read()
old="        exit_with_status(1, ExitKind::Failure, DOCTOR_FAILED);"
assert old in s, "PATTERN ABSENT"
open(p,"w").write(s.replace(old,"        std::process::exit(1); // MUTANT: bypass"))
' 'MUTANT: bypass' RED

# --- M4: clap usage errors bypass the funnel (Cli::parse) ---
mutate_and_test "M4 clap usage error bypasses the funnel" '
p="src/main.rs"; s=open(p).read()
old="let cli = parse_cli();"
assert old in s, "PATTERN ABSENT"
open(p,"w").write(s.replace(old,"let cli = Cli::parse(); // MUTANT: clap exits on its own"))
' 'MUTANT: clap exits on its own' RED

# --- M5: banner is unconditional, including on exit 0 ---
mutate_and_test "M5 banner emitted even on exit 0" '
p="src/exit.rs"; s=open(p).read()
old="""    if status != 0 {
        emit_exit_banner(kind, label, status);
    }
    std::process::exit(status)"""
new="""    emit_exit_banner(kind, label, status); // MUTANT: unconditional
    std::process::exit(status)"""
assert old in s, "PATTERN ABSENT"
open(p,"w").write(s.replace(old,new))
' 'MUTANT: unconditional' RED

# --- M6: banner on stdout instead of stderr ---
mutate_and_test "M6 banner written to stdout" '
p="src/exit.rs"; s=open(p).read()
old="    let mut stderr = std::io::stderr().lock();"
assert old in s, "PATTERN ABSENT"
open(p,"w").write(s.replace(old,"    let mut stderr = std::io::stdout().lock(); // MUTANT: wrong stream"))
' 'MUTANT: wrong stream' RED

# --- M7: gated on isatty (the forbidden pipe-detection design) ---
mutate_and_test "M7 banner gated on stderr being a terminal" '
p="src/exit.rs"; s=open(p).read()
old="pub fn emit_exit_banner(kind: ExitKind, label: &str, status: i32) {"
new="""pub fn emit_exit_banner(kind: ExitKind, label: &str, status: i32) {
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        return; // MUTANT: isatty gate
    }"""
assert old in s, "PATTERN ABSENT"
open(p,"w").write(s.replace(old,new))
' 'MUTANT: isatty gate' RED

# --- M8: panic path loses the banner ---
mutate_and_test "M8 fatal panic emits no banner" '
p="src/main.rs"; s=open(p).read()
old="        if beads_rust::exit::panic_is_fatal() {"
assert old in s, "PATTERN ABSENT"
open(p,"w").write(s.replace(old,"        if false && beads_rust::exit::panic_is_fatal() { // MUTANT: no panic banner"))
' 'MUTANT: no panic banner' RED

# --- M9: Notice wording claims FAILED (unit-tested wording) ---
echo "=================== M9 Notice wording claims FAILED  (expect RED) ==================="
if python3 -c '
p="src/exit.rs"; s=open(p).read()
old="""        ExitKind::Notice => format!("{name}: {label} (exit {status})"),"""
assert old in s, "PATTERN ABSENT"
new="""        ExitKind::Notice => format!("{name}: FAILED ({label}, exit {status})"), // MUTANT: lying wording"""
open(p,"w").write(s.replace(old,new))
'; then
    grep -q 'MUTANT: lying wording' src/exit.rs || { echo "MUTATION NOT PRESENT — inadmissible" >&2; restore; exit 1; }
    echo "mutation proven present in src/"
    cargo test --lib exit:: -j 2 -- --test-threads=2 2>&1 | grep -E "^test |^test result:|^error"
else
    echo "MUTATION SCRIPT FAILED" >&2
fi
restore

# --- M10: wording drift ---
mutate_and_test "M10 banner wording drifts (FAILED -> FAILURE)" '
p="src/exit.rs"; s=open(p).read()
old="""format!("{name}: FAILED ({label}, exit {status})")"""
assert old in s, "PATTERN ABSENT"
open(p,"w").write(s.replace(old,"""format!("{name}: FAILURE ({label}, exit {status})") /* MUTANT: wording drift */"""))
' 'MUTANT: wording drift' RED

# --- M11: two banners instead of one ---
mutate_and_test "M11 banner emitted twice" '
p="src/exit.rs"; s=open(p).read()
old="""    if status != 0 {
        emit_exit_banner(kind, label, status);
    }"""
new="""    if status != 0 {
        emit_exit_banner(kind, label, status);
        emit_exit_banner(kind, label, status); // MUTANT: duplicate banner
    }"""
assert old in s, "PATTERN ABSENT"
open(p,"w").write(s.replace(old,new))
' 'MUTANT: duplicate banner' RED

# --- M12: banner prints a constant code instead of the real one ---
mutate_and_test "M12 banner reports a constant code" '
p="src/main.rs"; s=open(p).read()
old="exit_with_status(exit_code, ExitKind::Failure, structured.code.as_str());"
assert old in s, "PATTERN ABSENT"
open(p,"w").write(s.replace(old,"exit_with_status(exit_code, ExitKind::Failure, \"VALIDATION_FAILED\"); // MUTANT: constant code"))
' 'MUTANT: constant code' RED

# --- M13: a real failure mislabelled as a Notice ---
mutate_and_test "M13 real failure mislabelled Notice" '
p="src/main.rs"; s=open(p).read()
old="exit_with_status(exit_code, ExitKind::Failure, structured.code.as_str());"
assert old in s, "PATTERN ABSENT"
open(p,"w").write(s.replace(old,"exit_with_status(exit_code, ExitKind::Notice, structured.code.as_str()); // MUTANT: wrong kind"))
' 'MUTANT: wrong kind' RED

# --- M14: config-get path bypasses the funnel ---
mutate_and_test "M14 config-get bypasses the funnel" '
import re
p="src/cli/commands/config.rs"; s=open(p).read()
m=re.search(r"exit_with_status\([^;]*CONFIG_KEY_NOT_FOUND[^;]*\);", s, re.S)
assert m, "PATTERN ABSENT"
open(p,"w").write(s[:m.start()]+"std::process::exit(1); // MUTANT: config bypass"+s[m.end():])
' 'MUTANT: config bypass' RED

# --- M15: `version --check` paths bypass the funnel ---
# EXPECTED TO SURVIVE. Both exits are behind a network call to the GitHub
# releases API, so there is no hermetic test; the wording is unit-tested and the
# behaviour was verified by hand (see the PR). This is the one admissible
# "not caught" finding, and it is admissible only because of the grep proof.
mutate_and_test "M15 version --check bypasses the funnel" '
import re
p="src/cli/commands/version.rs"; s=open(p).read()
ms=[m for m in re.finditer(r"exit_with_status\([^;]*\);", s, re.S)]
assert ms, "PATTERN ABSENT"
out=s
for m in reversed(ms):
    out=out[:m.start()]+"std::process::exit(1); // MUTANT: version bypass"+out[m.end():]
open(p,"w").write(out)
print("replaced", len(ms), "exit_with_status call(s)")
' 'MUTANT: version bypass' GREEN

echo "=================== control: unmutated tree ==================="
run_tests
cargo test --lib exit:: -j 2 -- --test-threads=2 2>&1 | grep -E "^test result:"
echo "final tree state (must be empty):"
git status --porcelain src/ && echo "(clean)"
