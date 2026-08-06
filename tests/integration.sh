#!/usr/bin/env bash
#
# End-to-end test for jj-extract. Drives the real binary the way Claude Code and
# Codex do: each tool edit is bracketed by Pre/PostToolUse hook JSON piped into
# `jj-extract --hook` (which tags a jj snapshot with the acting session — no
# opt-in), then `jj-extract --all` reconstructs each session's change from the
# evolog. Asserts on the extracted changes.
#
# Usage: tests/integration.sh [path/to/jj-extract]   (default: target/debug)
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$REPO_ROOT/target/debug/jj-extract}"
[ -x "$BIN" ] || { echo "binary not found: $BIN  (run: cargo build)" >&2; exit 2; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export JJ_GIT_OK=1 JJ_EDITOR=true

PASS=0
FAIL=0
ok() { if eval "$2"; then echo "  PASS: $1"; PASS=$((PASS + 1)); else echo "  FAIL: $1"; FAIL=$((FAIL + 1)); fi; }

new_repo() {
  REPO="$WORK/$1"
  export JJ_EXTRACT_HOME="$WORK/home-$1"
  rm -rf "$REPO" "$JJ_EXTRACT_HOME"
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

hookev() { # EVENT SESSION FILE
  printf '{"hook_event_name":"%s","cwd":"%s","session_id":"%s","tool_name":"Edit","tool_input":{"file_path":"%s/%s"}}' \
    "$1" "$REPO" "$2" "$REPO" "$3" | "$BIN" --hook
}
edit() { hookev PreToolUse "$1" "$2"; printf '%s' "$3" >"$REPO/$2"; hookev PostToolUse "$1" "$2"; }
codex_hookev() { # EVENT SESSION OPERATION FILE
  printf '{"hook_event_name":"%s","cwd":"%s","session_id":"%s","tool_name":"apply_patch","tool_input":{"command":"*** Begin Patch\\n*** %s File: %s\\n*** End Patch"}}' \
    "$1" "$REPO" "$2" "$3" "$4" | "$BIN" --hook
}
codex_edit() { codex_hookev PreToolUse "$1" Update "$2"; printf '%s' "$3" >"$REPO/$2"; codex_hookev PostToolUse "$1" Update "$2"; }
codex_add() { codex_hookev PreToolUse "$1" Add "$2"; printf '%s' "$3" >"$REPO/$2"; codex_hookev PostToolUse "$1" Add "$2"; }

extract_all() { BUILT="$("$BIN" --all 2>&1)"; }
cid() { echo "$BUILT" | grep "session $1 " | grep -oE 'change [0-9a-z]+' | head -1 | awk '{print $2}'; }
addedlines() { jj diff -r "$1" --git 2>/dev/null | grep '^+' | grep -v '^+++'; }
has() { addedlines "$1" | grep -q "$2"; }
nothas() { ! addedlines "$1" | grep -q "$2"; }
# One-session extract, echoing just the built change id.
extract_one() { "$BIN" 2>&1 | grep "session $1 " | grep -oE 'change [0-9a-z]+' | head -1 | awk '{print $2}'; }
# How many extracted changes carry a given session's trailer.
n_extractions() { jj log -r "description(substring:\"jj-extract-session: $1\")" --no-graph -T 'change_id.short() ++ "\n"' 2>/dev/null | grep -c .; }
n_divergent() { jj log -r 'all()' --no-graph -T 'if(divergent,"X","")' 2>/dev/null | grep -c X; }
n_heads() { jj log -r 'heads(all()) ~ root()' --no-graph -T '"X\n"' 2>/dev/null | grep -c X; }
is_conflict() { [ "$(jj log -r "$1" --no-graph -T 'if(conflict,"yes","no")' 2>/dev/null)" = yes ]; }

echo "== A: sequential interleaved edits to the SAME file (line numbers shift) =="
new_repo a
edit a1 f.txt $'l1\nl2\nl3\nBOTTOM\n'
edit a2 f.txt $'TOP\nl1\nl2\nl3\nBOTTOM\n'
extract_all
ok "a1 owns +BOTTOM"                 "has '$(cid a1)' BOTTOM"
ok "a1 does not own +TOP"            "nothas '$(cid a1)' TOP"
ok "a2 owns +TOP (despite the shift)" "has '$(cid a2)' TOP"
ok "a2 does not own +BOTTOM"         "nothas '$(cid a2)' BOTTOM"
ok "extract leaves a single linear head" "[ \"$(n_heads)\" = 1 ]"
ok "extract creates no divergent changes" "[ \"$(n_divergent)\" = 0 ]"

echo "== B: TRUE parallel edits to different files (edit lock serializes them) =="
new_repo b
printf 'a\nb\n' >f1.txt; printf 'x\ny\n' >f2.txt
jj file track f1.txt f2.txt >/dev/null 2>&1; jj describe -m two >/dev/null 2>&1; jj new >/dev/null 2>&1
edit a1 f1.txt $'a\nA1-add\nb\n' &
edit a2 f2.txt $'x\nA2-add\ny\n' &
wait
extract_all
ok "a1 got its file, not a2's" "has '$(cid a1)' A1-add && nothas '$(cid a1)' A2-add"
ok "a2 got its file, not a1's" "has '$(cid a2)' A2-add && nothas '$(cid a2)' A1-add"

echo "== C: a NON-tool (Bash/human) edit does NOT leak into the agent's change =="
new_repo c
edit a1 f.txt $'l1\nA1\nl2\nl3\n'                      # a1's tool edit
printf 'l1\nA1\nl2\nl3\nFOREIGN\n' >"$REPO/f.txt"      # a Bash command changes f, no hook
edit a1 f.txt $'A1TOP\nl1\nA1\nl2\nl3\nFOREIGN\n'      # a1 edits again (neutral pre-snapshot flushes FOREIGN)
extract_all
ok "a1's change has its own edits"        "has '$(cid a1)' A1 && has '$(cid a1)' A1TOP"
ok "a1's change does NOT contain FOREIGN"  "nothas '$(cid a1)' FOREIGN"
ok "unattributed content remains in live @" "jj diff -r @ --git | grep -q FOREIGN"

echo "== D: extract a single named session =="
new_repo d
edit a1 f.txt $'l1\nl2\nl3\nfrom-a1\n'
edit a2 g.txt $'only-a2\n'
ONE="$(JJ_EXTRACT_AGENT=a1 "$BIN" 2>&1)"
ok "extracting a1 builds a1"       "echo \"\$ONE\" | grep -q 'session a1'"
ok "extracting a1 does not build a2" "! echo \"\$ONE\" | grep -q 'session a2'"

echo "== E: multiple edits by one session compose into one change =="
new_repo e
edit a1 g.txt $'first line\n'                          # create a new file
edit a1 g.txt $'first line\nsecond line\n'             # extend it
extract_all
ok "new-file create+extend both collected" "has '$(cid a1)' 'first line' && has '$(cid a1)' 'second line'"

echo "== F: re-running extract updates in place (idempotent), no duplicates =="
new_repo f
edit a1 f.txt $'l1\nl2\nl3\nONE\n'
ID1="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"
ID2="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"                 # re-run, no new edits
ok "re-extract makes no duplicate (one change)" "[ \"$(n_extractions a1)\" = 1 ]"
ok "re-extract preserves the change id"         "[ -n \"$ID1\" ] && [ \"$ID1\" = \"$ID2\" ]"
edit a1 f.txt $'TOP\nl1\nl2\nl3\nONE\n'                     # a later edit...
ID3="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"                 # ...update-in-place picks it up
ok "re-extract still one change after new edit"  "[ \"$(n_extractions a1)\" = 1 ]"
ok "updated change keeps the stable id"          "[ \"$ID1\" = \"$ID3\" ]"
ok "updated change reflects the later edit"      "has '$ID3' TOP && has '$ID3' ONE"
ok "extract leaves no divergent commits"         "[ \"$(n_divergent)\" = 0 ]"
ok "re-extract keeps the graph linear"            "[ \"$(n_heads)\" = 1 ]"

echo "== G: project install/uninstall preserves settings and manages the jj alias =="
new_repo g
INSTALL_HOME="$WORK/install-home"
mkdir -p "$INSTALL_HOME/config" .claude .codex
printf '{"permissions":{"allow":["Read"]}}\n' >.claude/settings.json
printf '{"description":"keep","hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"check"}]}]}}\n' >.codex/hooks.json
HOME="$INSTALL_HOME" XDG_CONFIG_HOME="$INSTALL_HOME/config" "$BIN" --install --project >/dev/null 2>&1
ok "install preserves existing Claude settings" "grep -q '\"permissions\"' .claude/settings.json"
ok "install registers all three Claude hook events" \
  "grep -q 'SessionStart' .claude/settings.json && grep -q 'PreToolUse' .claude/settings.json && grep -q 'PostToolUse' .claude/settings.json"
ok "install preserves Codex hooks and registers apply_patch" \
  "grep -q '\"description\": \"keep\"' .codex/hooks.json && grep -q '\"command\": \"check\"' .codex/hooks.json && grep -Fq '\"matcher\": \"^apply_patch$\"' .codex/hooks.json"
ok "install registers the jj alias" \
  "HOME='$INSTALL_HOME' XDG_CONFIG_HOME='$INSTALL_HOME/config' jj config get aliases.extract | grep -q jj-extract"
HOME="$INSTALL_HOME" XDG_CONFIG_HOME="$INSTALL_HOME/config" "$BIN" --uninstall --project >/dev/null 2>&1
ok "uninstall removes only jj-extract hooks" \
  "grep -q '\"permissions\"' .claude/settings.json && ! grep -q 'jj-extract' .claude/settings.json"
ok "uninstall preserves unrelated Codex hooks" \
  "grep -q '\"command\": \"check\"' .codex/hooks.json && ! grep -q 'jj-extract' .codex/hooks.json"
ok "uninstall removes the jj alias" \
  "! HOME='$INSTALL_HOME' XDG_CONFIG_HOME='$INSTALL_HOME/config' jj config get aliases.extract >/dev/null 2>&1"

echo "== H: malformed settings are rejected without data loss =="
new_repo h
mkdir -p .claude
printf '{ not valid json\n' >.claude/settings.json
cp .claude/settings.json "$WORK/malformed-before.json"
HOME="$INSTALL_HOME" XDG_CONFIG_HOME="$INSTALL_HOME/config" "$BIN" --install --project >/dev/null 2>&1
MALFORMED_STATUS=$?
ok "install returns non-zero for malformed settings" "[ '$MALFORMED_STATUS' -ne 0 ]"
ok "malformed settings remain byte-for-byte unchanged" \
  "cmp -s .claude/settings.json '$WORK/malformed-before.json'"
ok "failed install does not create a misleading backup" "[ ! -e .claude/settings.json.jj-extract-bak ]"

new_repo h-codex
mkdir -p .claude .codex
printf '{"permissions":{"allow":["Read"]}}\n' >.claude/settings.json
cp .claude/settings.json "$WORK/claude-before.json"
printf '{ not valid json\n' >.codex/hooks.json
HOME="$INSTALL_HOME" XDG_CONFIG_HOME="$INSTALL_HOME/config" "$BIN" --install --project >/dev/null 2>&1
MALFORMED_STATUS=$?
ok "malformed Codex hooks fail before Claude settings are changed" \
  "[ '$MALFORMED_STATUS' -ne 0 ] && cmp -s .claude/settings.json '$WORK/claude-before.json' && [ ! -e .claude/settings.json.jj-extract-bak ]"

echo "== I: incompatible CLI options fail instead of being ignored =="
"$BIN" --all --message ignored >/dev/null 2>&1
CLI_STATUS=$?
ok "--all rejects an ignored --message" "[ '$CLI_STATUS' -eq 2 ]"
"$BIN" --project >/dev/null 2>&1
CLI_STATUS=$?
ok "--project requires install or uninstall" "[ '$CLI_STATUS' -eq 2 ]"

echo "== J: causally dependent sessions form a conflict-free linear stack =="
new_repo j
edit z-first f.txt $'l1\nl2\nl3\nFIRST\n'
edit a-second f.txt $'l1\nl2\nl3\nFIRST\nSECOND\n'
LIVE_ID_BEFORE="$(jj log -r @ --no-graph -T 'change_id.short()')"
extract_all
ok "earlier session owns only its appended line" \
  "has '$(cid z-first)' FIRST && nothas '$(cid z-first)' SECOND"
ok "later session owns only its dependent line" \
  "has '$(cid a-second)' SECOND && nothas '$(cid a-second)' FIRST"
ok "causal stacking resolves the false sibling conflict" \
  "! is_conflict '$(cid z-first)' && ! is_conflict '$(cid a-second)' && ! echo \"$BUILT\" | grep -q CONFLICT"
ok "causal extraction leaves one head and no divergence" \
  "[ \"$(n_heads)\" = 1 ] && [ \"$(n_divergent)\" = 0 ]"
ok "live working-copy identity and files are preserved" \
  "[ \"$(jj log -r @ --no-graph -T 'change_id.short()')\" = '$LIVE_ID_BEFORE' ] && grep -q SECOND f.txt"
ok "fully attributed edits leave live @ empty" "[ -z \"$(jj diff -r @ --summary)\" ]"

echo "== K: sessions extracting independently extend the same linear stack =="
new_repo k
edit a1 f.txt $'l1\nl2\nl3\nFIRST\n'
A1_ID="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"
edit a2 f.txt $'l1\nl2\nl3\nFIRST\nSECOND\n'
A2_ID="$(JJ_EXTRACT_AGENT=a2 extract_one a2)"
CURRENT_A1_ID="$(jj log -r 'description(substring:"jj-extract-session: a1")' --no-graph -T 'change_id.short()')"
ok "independent extracts preserve both session changes" \
  "[ \"$(n_extractions a1)\" = 1 ] && [ \"$(n_extractions a2)\" = 1 ]"
ok "the later independent extract owns only its edit" \
  "has '$A2_ID' SECOND && nothas '$A2_ID' FIRST"
ok "the earlier extracted change keeps its identity" \
  "[ '$A1_ID' = '$CURRENT_A1_ID' ]"
ok "independent extracts still leave one clean head" \
  "[ \"$(n_heads)\" = 1 ] && [ \"$(n_divergent)\" = 0 ] && ! is_conflict '$A2_ID'"

echo "== L: re-extraction linearizes legacy sibling extraction heads =="
new_repo l
edit a1 f.txt $'l1\nl2\nl3\nFIRST\n'
edit a2 f.txt $'TOP\nl1\nl2\nl3\nFIRST\n'
extract_all
LEGACY_A1="$(cid a1)"
LEGACY_A2="$(cid a2)"
LEGACY_BASE="$(jj log -r '@---' --no-graph -T 'change_id.short()')"
jj rebase -r "$LEGACY_A2" -d "$LEGACY_BASE" >/dev/null 2>&1
jj rebase -r @ -d "$LEGACY_BASE" >/dev/null 2>&1
ok "legacy fixture has one live and two sibling heads" "[ \"$(n_heads)\" = 3 ]"
JJ_EXTRACT_AGENT=a1 "$BIN" >/dev/null 2>&1
CURRENT_A1_ID="$(jj log -r 'description(substring:"jj-extract-session: a1")' --no-graph -T 'change_id.short()')"
CURRENT_A2_ID="$(jj log -r 'description(substring:"jj-extract-session: a2")' --no-graph -T 'change_id.short()')"
ok "next extraction collapses legacy siblings to one head" \
  "[ \"$(n_heads)\" = 1 ] && [ \"$(n_divergent)\" = 0 ]"
ok "legacy extracted change identities survive linearization" \
  "[ '$LEGACY_A1' = '$CURRENT_A1_ID' ] && [ '$LEGACY_A2' = '$CURRENT_A2_ID' ]"

echo "== M: Codex apply_patch hooks attribute edits and use CODEX_THREAD_ID =="
new_repo m
codex_edit codex-1 f.txt $'l1\nl2\nl3\nCODEX\n'
codex_add codex-1 codex-new.txt $'CODEX-NEW\n'
CODEX_RESULT="$(CODEX_THREAD_ID=codex-1 "$BIN" 2>&1)"
BUILT="$CODEX_RESULT"
ok "Codex apply_patch updates and new files are extracted" \
  "echo \"\$CODEX_RESULT\" | grep -q 'session codex-1' && has '$(cid codex-1)' CODEX && has '$(cid codex-1)' CODEX-NEW"
ok "Codex extraction remains a single non-divergent head" \
  "[ \"$(n_heads)\" = 1 ] && [ \"$(n_divergent)\" = 0 ]"

echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
