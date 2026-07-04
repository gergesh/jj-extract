#!/usr/bin/env bash
#
# End-to-end test for jj-collect. Drives the real binary the way Claude Code
# would: a session opts in with `jj-collect`, then each tool edit is bracketed by
# Pre/PostToolUse hook JSON piped into `jj-collect --hook`, with the actual edit
# to a scratch jj working copy in between. Asserts on the resulting jj stack.
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

# A session opts in.
collect() { JJ_COLLECT_AGENT="$1" "$BIN" ${2:+--to "$2"} >/dev/null 2>&1; }

hookev() { # EVENT SESSION FILE
  printf '{"hook_event_name":"%s","cwd":"%s","session_id":"%s","tool_name":"Edit","tool_input":{"file_path":"%s/%s"}}' \
    "$1" "$REPO" "$2" "$REPO" "$3" | "$BIN" --hook
}
# A bracketed tool edit: EVENT-agent FILE writes CONTENT between Pre and Post.
edit() { # SESSION FILE CONTENT
  hookev PreToolUse "$1" "$2"
  printf '%s' "$3" >"$REPO/$2"
  hookev PostToolUse "$1" "$2"
}

statejson() { cat "$JJ_COLLECT_HOME"/*/state.json 2>/dev/null; }
field() { statejson | python3 -c "import sys,json;s=sys.stdin.read().strip();d=json.loads(s) if s else {};print(d.get('bindings',{}).get('$1','') if '$1' else (d.get('holding') or ''))"; }
chg() { field "$1"; }
holdingchg() { field ""; }
addedlines() { jj diff -r "$1" --git 2>/dev/null | grep '^+' | grep -v '^+++'; }
has() { addedlines "$1" | grep -q "$2"; }
nothas() { ! addedlines "$1" | grep -q "$2"; }

echo "== A: sequential interleaved edits to the SAME file (line numbers shift) =="
new_repo a
collect a1
edit a1 f.txt $'l1\nl2\nl3\nBOTTOM\n'
collect a2
edit a2 f.txt $'TOP\nl1\nl2\nl3\nBOTTOM\n'
ok "a1 owns +BOTTOM"                 "has '$(chg a1)' BOTTOM"
ok "a1 does not own +TOP"            "nothas '$(chg a1)' TOP"
ok "a2 owns +TOP (despite the shift)" "has '$(chg a2)' TOP"
ok "a2 does not own +BOTTOM"         "nothas '$(chg a2)' BOTTOM"

echo "== B: TRUE parallel edits (the cross-tool edit lock must serialize them) =="
new_repo b
printf 'a\nb\n' >f1.txt; printf 'x\ny\n' >f2.txt
jj file track f1.txt f2.txt >/dev/null 2>&1; jj describe -m two >/dev/null 2>&1; jj new >/dev/null 2>&1
collect a1; collect a2
# Two full Pre→write→Post brackets launched at once. The edit lock (Pre takes
# it, Post releases it) forces one bracket to finish before the other's Pre
# proceeds, so the stack is never mutated concurrently.
edit a1 f1.txt $'a\nA1-add\nb\n' &
edit a2 f2.txt $'x\nA2-add\ny\n' &
wait
ok "a1 got its file, not a2's" "has '$(chg a1)' A1-add && nothas '$(chg a1)' A2-add"
ok "a2 got its file, not a1's" "has '$(chg a2)' A2-add && nothas '$(chg a2)' A1-add"

echo "== C: a NON-tool (Bash/human) edit does NOT leak into the agent's change =="
new_repo c
collect a1
edit a1 f.txt $'l1\nA1\nl2\nl3\n'                       # a1's tool edit
printf 'l1\nA1\nl2\nl3\nFOREIGN\n' >"$REPO/f.txt"       # a Bash command changes f, no hook
edit a1 f.txt $'A1TOP\nl1\nA1\nl2\nl3\nFOREIGN\n'       # a1 edits again (Pre parks FOREIGN)
ok "a1's change has its own edits"        "has '$(chg a1)' A1 && has '$(chg a1)' A1TOP"
ok "a1's change does NOT contain FOREIGN"  "nothas '$(chg a1)' FOREIGN"
ok "FOREIGN landed in the holding change"  "has '$(holdingchg)' FOREIGN"

echo "== D: a session that never opted in is not collected =="
new_repo d
edit nobody f.txt $'l1\nl2\nl3\nUNCOLLECTED\n'          # no `collect` for 'nobody'
ok "uncollected edit stays in @"  "jj diff -r @ 2>/dev/null | grep -q UNCOLLECTED"
ok "no binding was created"       "[ -z \"\$(chg nobody)\" ]"

echo "== E: Write of a brand-new file is collected (auto-track=none) =="
new_repo e
collect a1
edit a1 new.txt $'brand new content\n'
ok "new file collected into a1's change" "has '$(chg a1)' 'brand new content'"

echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
