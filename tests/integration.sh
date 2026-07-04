#!/usr/bin/env bash
#
# End-to-end test for jj-collect (record-and-construct model). Drives the real
# binary the way Claude Code would: a session opts in with `jj collect`, each
# tool edit is bracketed by Pre/PostToolUse hook JSON piped into `jj-collect
# --hook` (which just records snapshot pointers), then `jj-collect --build`
# reconstructs each session's isolated change. Asserts on the built changes.
#
# Usage: tests/integration.sh [path/to/jj-collect]   (default: target/debug)
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$REPO_ROOT/target/debug/jj-collect}"
[ -x "$BIN" ] || { echo "binary not found: $BIN  (run: cargo build)" >&2; exit 2; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export JJ_GIT_OK=1 JJ_EDITOR=true

PASS=0
FAIL=0
ok() { if eval "$2"; then echo "  PASS: $1"; PASS=$((PASS + 1)); else echo "  FAIL: $1"; FAIL=$((FAIL + 1)); fi; }

new_repo() {
  REPO="$WORK/$1"
  export JJ_COLLECT_HOME="$WORK/home-$1"
  rm -rf "$REPO" "$JJ_COLLECT_HOME"
  mkdir -p "$REPO"
  cd "$REPO" || exit 1
  jj git init . >/dev/null 2>&1
  jj config set --repo user.name tester >/dev/null 2>&1
  jj config set --repo user.email t@e.com >/dev/null 2>&1
  printf 'l1\nl2\nl3\n' >f.txt
  jj file track f.txt >/dev/null 2>&1
  jj describe -m base >/dev/null 2>&1
  jj new >/dev/null 2>&1
}

collect() { JJ_COLLECT_AGENT="$1" "$BIN" ${2:+-m "$2"} >/dev/null 2>&1; }
hookev() { # EVENT SESSION FILE
  printf '{"hook_event_name":"%s","cwd":"%s","session_id":"%s","tool_name":"Edit","tool_input":{"file_path":"%s/%s"}}' \
    "$1" "$REPO" "$2" "$REPO" "$3" | "$BIN" --hook
}
edit() { hookev PreToolUse "$1" "$2"; printf '%s' "$3" >"$REPO/$2"; hookev PostToolUse "$1" "$2"; }

# Build everything, capture output, and pull out a session's change id.
build_all() { BUILT="$("$BIN" --build --all 2>&1)"; }
cid() { echo "$BUILT" | grep "session $1 " | grep -oE 'change [0-9a-z]+' | head -1 | awk '{print $2}'; }
addedlines() { jj diff -r "$1" --git 2>/dev/null | grep '^+' | grep -v '^+++'; }
has() { addedlines "$1" | grep -q "$2"; }
nothas() { ! addedlines "$1" | grep -q "$2"; }

echo "== A: sequential interleaved edits to the SAME file (line numbers shift) =="
new_repo a
collect a1; edit a1 f.txt $'l1\nl2\nl3\nBOTTOM\n'
collect a2; edit a2 f.txt $'TOP\nl1\nl2\nl3\nBOTTOM\n'
build_all
ok "a1 owns +BOTTOM"                 "has '$(cid a1)' BOTTOM"
ok "a1 does not own +TOP"            "nothas '$(cid a1)' TOP"
ok "a2 owns +TOP (despite the shift)" "has '$(cid a2)' TOP"
ok "a2 does not own +BOTTOM"         "nothas '$(cid a2)' BOTTOM"

echo "== B: TRUE parallel edits to different files (no lock; path-scoped construct) =="
new_repo b
printf 'a\nb\n' >f1.txt; printf 'x\ny\n' >f2.txt
jj file track f1.txt f2.txt >/dev/null 2>&1; jj describe -m two >/dev/null 2>&1; jj new >/dev/null 2>&1
collect a1; collect a2
edit a1 f1.txt $'a\nA1-add\nb\n' &
edit a2 f2.txt $'x\nA2-add\ny\n' &
wait
build_all
ok "a1 got its file, not a2's" "has '$(cid a1)' A1-add && nothas '$(cid a1)' A2-add"
ok "a2 got its file, not a1's" "has '$(cid a2)' A2-add && nothas '$(cid a2)' A1-add"

echo "== C: a NON-tool (Bash/human) edit does NOT leak into the agent's change =="
new_repo c
collect a1
edit a1 f.txt $'l1\nA1\nl2\nl3\n'                      # a1's tool edit
printf 'l1\nA1\nl2\nl3\nFOREIGN\n' >"$REPO/f.txt"      # a Bash command changes f, no hook
edit a1 f.txt $'A1TOP\nl1\nA1\nl2\nl3\nFOREIGN\n'      # a1 edits again (pre-image captures FOREIGN)
build_all
ok "a1's change has its own edits"        "has '$(cid a1)' A1 && has '$(cid a1)' A1TOP"
ok "a1's change does NOT contain FOREIGN"  "nothas '$(cid a1)' FOREIGN"

echo "== D: a session that never opted in is not recorded =="
new_repo d
edit nobody f.txt $'l1\nl2\nl3\nUNCOLLECTED\n'         # no `collect` for 'nobody'
ok "no events recorded" "[ ! -s \"\$JJ_COLLECT_HOME\"/*/events.jsonl ] 2>/dev/null || ! grep -q nobody \"\$JJ_COLLECT_HOME\"/*/events.jsonl"
build_all
ok "nothing built for it" "! echo \"\$BUILT\" | grep -q 'session nobody'"

echo "== E: multiple edits by one session compose into one change =="
new_repo e
collect a1
edit a1 g.txt $'first line\n'                          # create a new file
edit a1 g.txt $'first line\nsecond line\n'             # extend it
build_all
ok "new-file create+extend both collected" "has '$(cid a1)' 'first line' && has '$(cid a1)' 'second line'"

echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
