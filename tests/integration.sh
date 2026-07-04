#!/usr/bin/env bash
#
# End-to-end test for jj-collect. Drives the real binary the way Claude Code
# would: by piping synthetic hook JSON into `jj-collect hook`, with actual edits
# made to a scratch jj working copy in between. Asserts on the resulting jj
# stack, so it exercises the true jj-native collection path — not mocks.
#
# Usage: tests/integration.sh [path/to/jj-collect]
#        (defaults to target/debug/jj-collect)
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$REPO_ROOT/target/debug/jj-collect}"
if [ ! -x "$BIN" ]; then
  echo "binary not found: $BIN  (run: cargo build)" >&2
  exit 2
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export JJ_GIT_OK=1 JJ_EDITOR=true
export JJ_COLLECT_HOME="$WORK/home"

PASS=0
FAIL=0
ok() { if eval "$2"; then echo "  PASS: $1"; PASS=$((PASS + 1)); else echo "  FAIL: $1"; FAIL=$((FAIL + 1)); fi; }

# Point $REPO at a fresh jj repo (seeded with one committed file) and cd into it.
new_repo() {
  REPO="$WORK/$1"
  export JJ_COLLECT_HOME="$WORK/home-$1"
  rm -rf "$REPO" "$JJ_COLLECT_HOME"
  mkdir -p "$REPO"
  cd "$REPO" || exit 1
  jj git init . >/dev/null 2>&1
  jj config set --repo user.name tester >/dev/null 2>&1
  jj config set --repo user.email t@e.com >/dev/null 2>&1
  printf 'seed\n' >seed.txt
  jj file track seed.txt >/dev/null 2>&1
  jj describe -m base >/dev/null 2>&1
  jj new >/dev/null 2>&1
}

# hookev EVENT TOOL SESSION [TOOL_INPUT_JSON]
hookev() {
  local ev="$1" tool="$2" sid="$3" ti="$4"
  [ -z "$ti" ] && ti='{}'
  printf '{"hook_event_name":"%s","cwd":"%s","session_id":"%s","tool_name":"%s","tool_input":%s}' \
    "$ev" "$REPO" "$sid" "$tool" "$ti" | "$BIN" hook
}
edit_json() { printf '{"file_path":"%s/%s"}' "$REPO" "$1"; }
chg() { "$BIN" list --json 2>/dev/null | python3 -c "import sys,json;d=json.load(sys.stdin);print(next((a['change_id'] for a in d['agents'] if a['id']=='$1'),''))"; }
# Only ADDED lines (git `+text`, minus the `+++` header) count as "owned".
addedlines() { jj diff -r "$1" --git 2>/dev/null | grep '^+' | grep -v '^+++'; }
has() { addedlines "$1" | grep -q "$2"; }
nothas() { ! addedlines "$1" | grep -q "$2"; }

echo "== A: sequential interleaved edits to the SAME file (line numbers shift) =="
new_repo a
hookev PreToolUse Edit a1 "$(edit_json f.txt)"
printf 'l1\nl2\nl3\nBOTTOM\n' >f.txt # a1 appends at the bottom
hookev PostToolUse Edit a1 "$(edit_json f.txt)"
printf 'TOP\nl1\nl2\nl3\nBOTTOM\n' >f.txt # a2 prepends at the top (shifts a1's line)
hookev PostToolUse Edit a2 "$(edit_json f.txt)"
ok "a1 owns +BOTTOM" "has '$(chg a1)' BOTTOM"
ok "a1 does not own +TOP" "nothas '$(chg a1)' TOP"
ok "a2 owns +TOP (despite the shift)" "has '$(chg a2)' TOP"
ok "a2 does not own +BOTTOM" "nothas '$(chg a2)' BOTTOM"

echo "== B: race — different files, both dirty before either hook runs =="
new_repo b
printf 'a\nb\n' >f1.txt
printf 'x\ny\n' >f2.txt
jj file track f1.txt f2.txt >/dev/null 2>&1
jj describe -m two >/dev/null 2>&1
jj new >/dev/null 2>&1
hookev PreToolUse Edit a1 "$(edit_json f1.txt)"
printf 'a\nA1-add\nb\n' >f1.txt # both agents' edits land on disk...
printf 'x\nA2-add\ny\n' >f2.txt # ...before either hook processes them
hookev PostToolUse Edit a1 "$(edit_json f1.txt)"
hookev PostToolUse Edit a2 "$(edit_json f2.txt)"
ok "a1 claims only f1" "has '$(chg a1)' A1-add && nothas '$(chg a1)' A2-add"
ok "a2 claims only f2" "has '$(chg a2)' A2-add && nothas '$(chg a2)' A1-add"

echo "== B2: TRUE parallel hooks (the flock must serialize them) =="
printf 'a\nP1\nb\n' >f1.txt
printf 'x\nP2\ny\n' >f2.txt
hookev PostToolUse Edit a1 "$(edit_json f1.txt)" &
hookev PostToolUse Edit a2 "$(edit_json f2.txt)" &
wait
ok "parallel: a1 got P1, not P2" "has '$(chg a1)' P1 && nothas '$(chg a1)' P2"
ok "parallel: a2 got P2, not P1" "has '$(chg a2)' P2 && nothas '$(chg a2)' P1"

echo "== C: genuine same-line clash surfaces as a conflict on harvest =="
new_repo c
printf 'x\nTARGET\nz\n' >f.txt
jj file track f.txt >/dev/null 2>&1
jj describe -m one >/dev/null 2>&1
jj new >/dev/null 2>&1
hookev PreToolUse Edit a1 "$(edit_json f.txt)"
printf 'x\nA1\nz\n' >f.txt
hookev PostToolUse Edit a1 "$(edit_json f.txt)"
printf 'x\nA2\nz\n' >f.txt # a2 changes the same line
hookev PostToolUse Edit a2 "$(edit_json f.txt)"
OUT=$("$BIN" commit --agent a2 -m "a2" 2>&1)
ok "harvesting a2 reports a conflict" "echo \"\$OUT\" | grep -qi conflict"
OUTA=$("$BIN" commit --agent a1 -m "a1" 2>&1)
ok "harvesting a1 is clean" "echo \"\$OUTA\" | grep -qi harvested && ! echo \"\$OUTA\" | grep -qi 'WITH CONFLICTS'"

echo "== D: Write of a brand-new file is collected (auto-track=none) =="
new_repo d
hookev PreToolUse Write a1 "$(edit_json new.txt)"
printf 'brand new content\n' >new.txt
hookev PostToolUse Write a1 "$(edit_json new.txt)"
ok "new file collected into a1's change" "has '$(chg a1)' 'brand new content'"

echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
