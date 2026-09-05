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
# The suite drives identity through hook payloads, so an inherited one must not
# win over them: a Claude Code or Codex session running these tests exports its
# own agent identity, which would otherwise tag every edit below as that session.
unset JJ_EXTRACT_AGENT CLAUDE_CODE_SESSION_ID CODEX_THREAD_ID

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

# A Claude Bash tool call. The command is written the way it travels in the hook
# payload — one JSON string with \n for its newlines — and expanded to run.
bash_hookev() { # EVENT SESSION JSON_COMMAND
  printf '{"hook_event_name":"%s","cwd":"%s","session_id":"%s","tool_name":"Bash","tool_input":{"command":"%s"}}' \
    "$1" "$REPO" "$2" "$3" | "$BIN" --hook
}
run_bash() { # SESSION JSON_COMMAND
  bash_hookev PreToolUse "$1" "$2"
  ( cd "$REPO" && eval "$(printf '%b' "$2")" ) >/dev/null 2>&1
  bash_hookev PostToolUse "$1" "$2"
}

# The original reconstruction fixtures explicitly exercise opt-in amendment.
extract_all() { BUILT="$("$BIN" --all --amend 2>&1)"; }
cid() { echo "$BUILT" | grep "session $1 " | grep -oE 'change [0-9a-z]+' | head -1 | awk '{print $2}'; }
addedlines() { jj diff -r "$1" --git 2>/dev/null | grep '^+' | grep -v '^+++'; }
has() { addedlines "$1" | grep -q "$2"; }
nothas() { ! addedlines "$1" | grep -q "$2"; }
# One-session extract, echoing just the built change id.
extract_one() { "$BIN" --amend 2>&1 | grep "session $1 " | grep -oE 'change [0-9a-z]+' | head -1 | awk '{print $2}'; }
# How many extracted changes a session has. Extraction identifies its own
# changes through the operation-log ledger, not the description, so this counts
# the default description these fixtures never rewrite.
n_extractions() { jj log -r "description(substring:\"jj-extract: $1\")" --no-graph -T 'change_id.short() ++ "\n"' 2>/dev/null | grep -c .; }
# Every visible commit except the root, to catch a duplicate change.
n_commits() { jj log -r 'all() ~ root()' --no-graph -T '"X\n"' 2>/dev/null | grep -c X; }
desc() { jj log -r "$1" --no-graph -T description 2>/dev/null; }
n_divergent() { jj log -r 'all()' --no-graph -T 'if(divergent,"X","")' 2>/dev/null | grep -c X; }
n_heads() { jj log -r 'heads(all()) ~ root()' --no-graph -T '"X\n"' 2>/dev/null | grep -c X; }
is_conflict() { [ "$(jj log -r "$1" --no-graph -T 'if(conflict,"yes","no")' 2>/dev/null)" = yes ]; }
op_id() { jj --at-op=@ --ignore-working-copy op log --no-graph -n 1 -T 'self.id() ++ "\n"'; }
op_parent() { jj --at-op=@ --ignore-working-copy op log --no-graph -n 1 -T 'self.parents().map(|p| p.id()).join("\n") ++ "\n"'; }
same_tree() {
  local diff
  diff="$(jj diff --from "$1" --to @ --summary)" || return 1
  [ -z "$diff" ]
}
disk_manifest() {
  python3 - <<'PY'
import hashlib, os, stat
for root, dirs, files in os.walk('.'):
    dirs[:] = sorted(d for d in dirs if d not in ('.jj', '.git'))
    for name in sorted(files + [d for d in dirs if os.path.islink(os.path.join(root, d))]):
        path = os.path.join(root, name)
        mode = os.lstat(path).st_mode
        content = os.readlink(path) if stat.S_ISLNK(mode) else hashlib.sha256(open(path, 'rb').read()).hexdigest()
        print(repr(path), oct(mode), repr(content))
PY
}

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

echo "== E2: a later agent edit absorbs same-file neutral formatting context =="
new_repo e2
codex_add a1 picker.ts $'const picker={hour:19}\n'
printf 'const picker = { hour: 19 }\n' >picker.ts             # formatter/shell edit, captured neutrally
printf 'l1\nl2\nl3\nFOREIGN\n' >f.txt                 # unrelated neutral edit must stay live
codex_edit a1 picker.ts $'const picker = { hour: 20 }\n'
extract_all
ok "same-file neutral context composes without conflict" \
  "! is_conflict '$(cid a1)' && has '$(cid a1)' 'hour: 20'"
ok "absorbed new file is fully removed from live @" \
  "! jj diff -r @ --summary | grep -q picker.ts"
ok "unrelated neutral content still remains in live @" \
  "jj diff -r @ --git | grep -q FOREIGN"

echo "== E3: overlapping formatter context also works on an existing file =="
new_repo e3
printf 'const picker={hour:19}\n' >picker.ts
jj file track picker.ts >/dev/null 2>&1
jj describe -m picker-base >/dev/null 2>&1
jj new >/dev/null 2>&1
codex_edit a1 picker.ts $'const picker={hour:19,minute:30}\n'
printf 'const picker = { hour: 19, minute: 30 }\n' >picker.ts
codex_edit a1 picker.ts $'const picker = { hour: 20, minute: 30 }\n'
extract_all
ok "overlapping formatter rewrite is adopted without conflict" \
  "! is_conflict '$(cid a1)' && has '$(cid a1)' 'hour: 20'"
ok "existing formatted file leaves no duplicate residual" \
  "! jj diff -r @ --summary | grep -q picker.ts"

echo "== E4: causal replay handles fileset-special paths =="
new_repo e4
codex_add a1 'picker & time.ts' $'const picker={hour:19}\n'
printf 'const picker = { hour: 19 }\n' >'picker & time.ts'
codex_edit a1 'picker & time.ts' $'const picker = { hour: 20 }\n'
extract_all
ok "special path extracts without conflict" \
  "! is_conflict '$(cid a1)' && has '$(cid a1)' 'hour: 20'"
ok "special path leaves no duplicate residual" \
  "! jj diff -r @ --summary | grep -Fq 'picker & time.ts'"

echo "== F: opt-in amendment updates in place without duplicates =="
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
"$BIN" --all --message "written by the agent" >/dev/null 2>&1
CLI_STATUS=$?
ok "the removed --message is rejected, not ignored" "[ '$CLI_STATUS' -eq 2 ]"
"$BIN" --all --agent ignored >/dev/null 2>&1
CLI_STATUS=$?
ok "--all rejects an ignored --agent" "[ '$CLI_STATUS' -eq 2 ]"
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
CURRENT_A1_ID="$(jj log -r 'description(substring:"jj-extract: a1")' --no-graph -T 'change_id.short()')"
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
JJ_EXTRACT_AGENT=a1 "$BIN" --amend >/dev/null 2>&1
CURRENT_A1_ID="$(jj log -r 'description(substring:"jj-extract: a1")' --no-graph -T 'change_id.short()')"
CURRENT_A2_ID="$(jj log -r 'description(substring:"jj-extract: a2")' --no-graph -T 'change_id.short()')"
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

echo "== M2: live @ preserves its clean tree above a conflicted extraction =="
new_repo m2
edit a1 f.txt $'A1\nl2\nl3\n'
edit a2 f.txt $'A2\nl2\nl3\n'
edit a1 f.txt $'A1-LATER\nl2\nl3\n'
M2_RESULT="$(JJ_EXTRACT_AGENT=a1 "$BIN" --allow-conflicts 2>&1)"
M2_ID="$(echo "$M2_RESULT" | grep 'session a1 ' | grep -oE 'change [0-9a-z]+' | head -1 | awk '{print $2}')"
ok "cyclic session dependency produces the expected extracted conflict fixture" \
  "[ -n '$M2_ID' ] && is_conflict '$M2_ID'"
ok "conflicted extraction does not propagate into live @" \
  "! is_conflict @ && grep -q '^A1-LATER$' f.txt"

echo "== N: one undo reverses one complete extraction =="
new_repo n
edit a1 f.txt $'l1\nl2\nl3\nUNDO-LINE\n'
LIVE_ID_BEFORE="$(jj log -r @ --no-graph -T 'change_id.short()')"
OP_BEFORE="$(op_id)"
JJ_EXTRACT_AGENT=a1 "$BIN" >/dev/null 2>&1
ok "extract publishes exactly one operation" "[ \"$(op_parent)\" = '$OP_BEFORE' ]"
ok "the operation describes the complete extraction" \
  "jj --at-op=@ --ignore-working-copy op log --no-graph -n 1 -T 'description.first_line()' | grep -q '^extract session a1$'"
jj undo >/dev/null 2>&1
ok "one undo removes the extracted change" "[ \"$(n_extractions a1)\" = 0 ]"
ok "one undo restores the live working-copy change" \
  "[ \"$(jj log -r @ --no-graph -T 'change_id.short()')\" = '$LIVE_ID_BEFORE' ] && jj diff -r @ --git | grep -q UNDO-LINE"

echo "== O: undoing a re-extraction restores the prior extracted version =="
new_repo o
edit a1 f.txt $'l1\nl2\nl3\nONE\n'
ID1="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"
edit a1 f.txt $'TOP\nl1\nl2\nl3\nONE\n'
OP_BEFORE_SECOND="$(op_id)"
ID2="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"
ok "re-extraction is one operation" "[ \"$(op_parent)\" = '$OP_BEFORE_SECOND' ]"
ok "re-extraction preserves the extracted change id" "[ -n '$ID1' ] && [ '$ID1' = '$ID2' ]"
ok "re-extraction includes old and new edits" "has '$ID2' ONE && has '$ID2' TOP"
jj undo >/dev/null 2>&1
CURRENT_A1_ID="$(jj log -r 'description(substring:"jj-extract: a1")' --no-graph -T 'change_id.short()')"
ok "undo restores the prior extracted content" \
  "[ '$CURRENT_A1_ID' = '$ID1' ] && has '$CURRENT_A1_ID' ONE && nothas '$CURRENT_A1_ID' TOP"
ok "undo returns the later edit to live @" "echo \"\$(jj diff -r @ --git)\" | grep -q TOP"

echo "== P: jj-extract never invokes the jj executable =="
new_repo p
FAKE_BIN="$WORK/fake-bin"
JJ_SPAWN_MARKER="$WORK/jj-was-spawned"
mkdir -p "$FAKE_BIN"
printf '#!/bin/sh\ntouch "%s"\nexit 97\n' "$JJ_SPAWN_MARKER" >"$FAKE_BIN/jj"
chmod +x "$FAKE_BIN/jj"
PATH="$FAKE_BIN:$PATH" edit a1 f.txt $'l1\nl2\nl3\nNO-SUBPROCESS\n'
BUILT="$(PATH="$FAKE_BIN:$PATH" "$BIN" --all 2>&1)"
ok "recording and extraction do not spawn jj" \
  "[ ! -e '$JJ_SPAWN_MARKER' ] && has '$(cid a1)' NO-SUBPROCESS"

echo "== Q: same-session extraction on another base creates a distinct change =="
new_repo q
COMMON_BASE="$(jj log -r @- --no-graph -T 'change_id.short()')"
edit a1 f.txt $'l1\nl2\nl3\nFIRST-LINE\n'
FIRST_ID="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"
jj new "$COMMON_BASE" >/dev/null 2>&1
printf 'branch base\n' >branch.txt
jj file track branch.txt >/dev/null 2>&1
jj describe -m branch-base >/dev/null 2>&1
jj new >/dev/null 2>&1
edit a1 f.txt $'l1\nl2\nl3\nSECOND-LINE\n'
SECOND_ID="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"
ok "a same-session extraction on another base gets a new change id" \
  "[ -n '$FIRST_ID' ] && [ -n '$SECOND_ID' ] && [ '$FIRST_ID' != '$SECOND_ID' ]"
ok "the extraction on the first line is left unchanged" \
  "has '$FIRST_ID' FIRST-LINE && nothas '$FIRST_ID' SECOND-LINE"

echo "== R: an edit never starts tracking a file; only a created file does =="
new_repo r
jj config set --repo snapshot.auto-track 'none()' >/dev/null 2>&1
printf 'kept out of the repo\n' >untracked.txt   # exists, deliberately untracked
edit a1 untracked.txt $'kept out of the repo\nAGENT-EDIT\n'
edit a1 created.txt $'AGENT-CREATED\n'           # a file the agent itself creates
extract_all
ok "an edit to an untracked file is not extracted" "nothas '$(cid a1)' AGENT-EDIT"
ok "an edited untracked file stays untracked" \
  "! jj file list -r @ 2>/dev/null | grep -q untracked.txt"
ok "a file the agent created is tracked and extracted" \
  "has '$(cid a1)' AGENT-CREATED && jj file list -r @ 2>/dev/null | grep -q created.txt"

echo "== S: extracted changes carry each agent's standard co-author trailer =="
new_repo s
edit a1 f.txt $'l1\nl2\nl3\nCLAUDE-EDIT\n'
codex_edit c1 f.txt $'l1\nl2\nl3\nCLAUDE-EDIT\nCODEX-EDIT\n'
extract_all
ok "a Claude session is credited the standard way" \
  "desc '$(cid a1)' | grep -q '^Co-authored-by: Claude <noreply@anthropic.com>$'"
ok "a Codex session is credited the standard way" \
  "desc '$(cid c1)' | grep -q '^Co-authored-by: Codex <noreply@openai.com>$'"
ok "no private session trailer is written" \
  "! desc '$(cid a1)' | grep -q 'jj-extract-session'"

echo "== T: re-extraction finds its change through a rewritten description =="
new_repo t
edit a1 f.txt $'l1\nl2\nl3\nONE\n'
ID1="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"
jj describe -r "$ID1" -m 'entirely my own words' >/dev/null 2>&1
edit a1 f.txt $'TOP\nl1\nl2\nl3\nONE\n'
ID2="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"
ok "a re-described change is still updated in place" "[ -n '$ID1' ] && [ '$ID1' = '$ID2' ]"
ok "the author's own description survives re-extraction" \
  "desc '$ID2' | grep -q '^entirely my own words$'"
ok "re-extraction creates no duplicate change"       "[ \"$(n_commits)\" = 3 ]"
ok "the update carries both edits"                   "has '$ID2' ONE && has '$ID2' TOP"

echo "== U: shell file writes are recorded like any other edit =="
new_repo u
jj config set --repo snapshot.auto-track 'none()' >/dev/null 2>&1
run_bash a1 "cat > created.txt <<'EOF'\nFROM-A-HEREDOC\nEOF"
run_bash a1 "python3 - <<'PY'\nfrom pathlib import Path\np = Path('f.txt')\np.write_text(p.read_text() + 'FROM-A-SCRIPT' + chr(10))\nPY"
run_bash a1 "rm -f nothing.txt; echo NOT-SIMPLE >> f.txt"
extract_all
ok "a heredoc-created file is tracked and extracted" \
  "has '$(cid a1)' FROM-A-HEREDOC && jj file list -r @ 2>/dev/null | grep -q created.txt"
ok "an inline script's edit to a tracked file is extracted" "has '$(cid a1)' FROM-A-SCRIPT"
ok "a command that does more than write files stays unattributed" \
  "nothas '$(cid a1)' NOT-SIMPLE && jj diff -r @ --git | grep -q NOT-SIMPLE"

echo "== V: a dry run previews the extraction and changes nothing =="
new_repo v
edit a1 f.txt $'l1\nl2\nl3\nDRY-ONE\n'
edit a1 g.txt $'DRY-TWO\n'
LIVE_ID_BEFORE="$(jj log -r @ --no-graph -T 'change_id.short()')"
OP_BEFORE="$(op_id)"
PREVIEW="$(JJ_EXTRACT_AGENT=a1 "$BIN" --dry-run 2>&1)"
ok "a dry run says the repository was not changed" \
  "echo \"\$PREVIEW\" | grep -q 'Dry run'"
ok "a dry run names the session and the files it would extract" \
  "echo \"\$PREVIEW\" | grep -q 'would extract' && echo \"\$PREVIEW\" | grep -q 'session a1' && echo \"\$PREVIEW\" | grep -q 'f.txt' && echo \"\$PREVIEW\" | grep -q 'g.txt'"
ok "a dry run publishes no operation" "[ \"$(op_id)\" = '$OP_BEFORE' ]"
ok "a dry run builds no change" "[ \"$(n_extractions a1)\" = 0 ]"
ok "a dry run leaves the live working-copy change untouched" \
  "[ \"$(jj log -r @ --no-graph -T 'change_id.short()')\" = '$LIVE_ID_BEFORE' ] && echo \"\$(jj diff -r @ --git)\" | grep -q DRY-ONE"
DRY_ID="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"
ok "the extraction that follows contains what the preview named" \
  "has '$DRY_ID' DRY-ONE && has '$DRY_ID' DRY-TWO"
UPDATE_PREVIEW="$(JJ_EXTRACT_AGENT=a1 "$BIN" --amend --dry-run 2>&1)"
ok "a dry run over an extracted session previews an update to its change" \
  "echo \"\$UPDATE_PREVIEW\" | grep -q 'would update' && echo \"\$UPDATE_PREVIEW\" | grep -q 'change $DRY_ID'"
ok "a dry run after extraction still leaves one change and one head" \
  "[ \"$(n_extractions a1)\" = 1 ] && [ \"$(n_heads)\" = 1 ]"

echo "== W: an early-starting session can belong above a later session =="
new_repo w
edit early own.txt $'EARLY-OWN\n'
edit later f.txt $'l1\nLATER\nl3\n'
edit early f.txt $'l1\nEARLY-FINAL\nl3\n'
OP_BEFORE="$(op_id)"
PREVIEW="$("$BIN" --all --dry-run 2>&1)"
ok "order search is also used by dry run without publishing" \
  "! echo \"\$PREVIEW\" | grep -q CONFLICT && [ \"$(op_id)\" = '$OP_BEFORE' ]"
extract_all
ok "dependent early starter is placed after its prerequisite" \
  "[ \"$(jj log -r "$(cid early)-" --no-graph -T 'change_id.short()')\" = '$(cid later)' ]"
ok "reordered changes are clean and keep their own edits" \
  "! is_conflict '$(cid early)' && ! is_conflict '$(cid later)' && has '$(cid early)' EARLY-FINAL && has '$(cid later)' LATER"
ok "reordering preserves the exact live tree and one head" \
  "[ -z \"$(jj diff -r @ --summary)\" ] && [ \"$(n_heads)\" = 1 ] && [ \"$(n_divergent)\" = 0 ]"

echo "== W2: updating an extracted session can move its existing change =="
new_repo w2
edit early own.txt $'EARLY-OWN\n'
EARLY_ID="$(JJ_EXTRACT_AGENT=early extract_one early)"
jj describe -r "$EARLY_ID" -m 'Keep my description' >/dev/null 2>&1
edit later f.txt $'l1\nLATER\nl3\n'
LATER_ID="$(JJ_EXTRACT_AGENT=later extract_one later)"
edit early f.txt $'l1\nEARLY-FINAL\nl3\n'
OP_BEFORE="$(op_id)"
MOVED_ID="$(JJ_EXTRACT_AGENT=early extract_one early)"
ok "re-extraction moves the same change above its new prerequisite" \
  "[ '$MOVED_ID' = '$EARLY_ID' ] && [ \"$(jj log -r "$MOVED_ID-" --no-graph -T 'change_id.short()')\" = '$LATER_ID' ] && ! is_conflict '$MOVED_ID'"
ok "moving preserves descriptions and is one undoable operation" \
  "desc '$MOVED_ID' | grep -q '^Keep my description$' && [ \"$(op_parent)\" = '$OP_BEFORE' ]"
jj undo >/dev/null 2>&1
ok "undo restores the earlier placement and content" \
  "[ \"$(jj log -r "$LATER_ID-" --no-graph -T 'change_id.short()')\" = '$EARLY_ID' ] && nothas '$EARLY_ID' EARLY-FINAL"

echo "== X: neutral context survives an intervening unrelated session =="
new_repo x
codex_add a1 picker.ts $'const picker={hour:19}\n'
printf 'const picker = { hour: 19 }\n' >picker.ts
codex_add a2 other.txt $'OTHER-AGENT\n'
codex_edit a1 picker.ts $'const picker = { hour: 20 }\n'
extract_all
ok "formatter context is found across another session's edit" \
  "! is_conflict '$(cid a1)' && has '$(cid a1)' '20' && nothas '$(cid a1)' OTHER-AGENT"

echo "== Y: separate words on the same line do not force a conflict =="
new_repo y
printf 'const x = 1; const y = 2;\n' >code.ts
jj file track code.ts >/dev/null 2>&1
jj describe -m code-base >/dev/null 2>&1
jj new >/dev/null 2>&1
codex_edit a1 code.ts $'const x = 3; const y = 2;\n'
codex_edit a2 code.ts $'const x = 3; const y = 4;\n'
codex_edit a1 code.ts $'const x = 5; const y = 4;\n'
extract_all
ok "word-level composition preserves independent same-line edits" \
  "! is_conflict '$(cid a1)' && ! is_conflict '$(cid a2)' && has '$(cid a1)' 'x = 5; const y = 2' && has '$(cid a2)' 'x = 5; const y = 4'"
ok "same-line edits leave no residual in live @" "[ -z \"$(jj diff -r @ --summary)\" ]"

echo "== Z: optional formatter context must not manufacture a dependency =="
new_repo z
printf 'const x = 1;\nconst y = 2;\n' >code.ts
jj file track code.ts >/dev/null 2>&1
jj describe -m code-base >/dev/null 2>&1
jj new >/dev/null 2>&1
codex_edit a1 code.ts $'const x = 3;\nconst y = 2;\n'
codex_edit a2 code.ts $'const x = 3;\nconst other = 9;\n'
printf 'const x=3;\nconst other=9;\n' >code.ts
codex_edit a1 code.ts $'const x=5;\nconst other=9;\n'
BUILT="$(JJ_EXTRACT_AGENT=a1 "$BIN" 2>&1)"
ok "optional formatting is dropped when it needs an unextracted session" \
  "! is_conflict '$(cid a1)' && has '$(cid a1)' '5' && nothas '$(cid a1)' other"
ok "the omitted session remains in the live diff" \
  "jj diff -r @ --git | grep -q other && [ \"$(n_extractions a2)\" = 0 ]"

echo "== Z2: three dependencies can reverse the entire first-edit order =="
new_repo z2
edit a own-a.txt $'A\n'
edit b own-b.txt $'B\n'
edit c f.txt $'l1\nC-VALUE\nl3\n'
edit b f.txt $'l1\nB-VALUE\nl3\n'
edit a f.txt $'l1\nA-VALUE\nl3\n'
extract_all
ok "the planner finds c -> b -> a" \
  "[ \"$(jj log -r "$(cid a)-" --no-graph -T 'change_id.short()')\" = '$(cid b)' ] && [ \"$(jj log -r "$(cid b)-" --no-graph -T 'change_id.short()')\" = '$(cid c)' ]"
ok "every intermediate change in the reversed stack is clean" \
  "! is_conflict '$(cid a)' && ! is_conflict '$(cid b)' && ! is_conflict '$(cid c)' && [ -z \"$(jj diff -r @ --summary)\" ]"
ORDER_BEFORE="$(jj log -r 'ancestors(@) ~ root()' --no-graph -T 'change_id.short() ++ "\n"')"
extract_all
ok "replanning is deterministic and preserves all change identities" \
  "[ \"$(jj log -r 'ancestors(@) ~ root()' --no-graph -T 'change_id.short() ++ "\n"')\" = '$ORDER_BEFORE' ] && [ \"$(n_divergent)\" = 0 ]"

echo "== Z3: neutral cleanup is independent for each file =="
new_repo z3
edit a1 created.txt $'ORIGINAL\n'
printf 'NEUTRAL-REPLACEMENT\n' >created.txt
printf 'l1\nl2\nl3\nFOREIGN\n' >f.txt
hookev PreToolUse a1 created.txt
printf 'AGENT-FINAL\n' >created.txt
printf 'TOP\nl1\nl2\nl3\nFOREIGN\n' >f.txt
hookev PostToolUse a1 created.txt
extract_all
ok "dependent neutral rewrite is retained only where needed" \
  "! is_conflict '$(cid a1)' && has '$(cid a1)' AGENT-FINAL && has '$(cid a1)' TOP && nothas '$(cid a1)' FOREIGN"
ok "cleanly removable neutral content remains live" \
  "jj diff -r @ --git | grep -q FOREIGN"

echo "== Z4: creation and deletion dependencies also determine placement =="
new_repo z4
edit early own.txt $'OWN\n'
edit creator new.txt $'CREATED\n'
edit early new.txt $'UPDATED\n'
extract_all
ok "a newly created file is available at the chosen parent" \
  "! is_conflict '$(cid early)' && ! is_conflict '$(cid creator)' && has '$(cid creator)' CREATED && has '$(cid early)' UPDATED && [ -z \"$(jj diff -r @ --summary)\" ]"

new_repo z4-delete
edit early own.txt $'OWN\n'
edit writer f.txt $'REPLACED\n'
codex_hookev PreToolUse early Delete f.txt
rm f.txt
codex_hookev PostToolUse early Delete f.txt
extract_all
ok "a deletion moves above the edit whose contents it deletes" \
  "! is_conflict '$(cid early)' && ! is_conflict '$(cid writer)' && [ \"$(jj log -r "$(cid early)-" --no-graph -T 'change_id.short()')\" = '$(cid writer)' ] && [ ! -e f.txt ] && [ -z \"$(jj diff -r @ --summary)\" ]"

echo "== Z5: unavoidable cycles remain visible, including in dry runs =="
new_repo z5
edit a1 f.txt $'A1\nl2\nl3\n'
edit a2 f.txt $'A2\nl2\nl3\n'
edit a1 f.txt $'A1-LATER\nl2\nl3\n'
PREVIEW="$("$BIN" --all --dry-run 2>&1)"
OP_BEFORE="$(op_id)"
LIVE_BEFORE="$(jj log -r @ --no-graph -T commit_id)"
cp f.txt "$WORK/cycle-before.txt"
REFUSED="$("$BIN" --all 2>&1)"
REFUSED_STATUS=$?
ok "conflicting extraction requires an explicit opt-in" \
  "[ '$REFUSED_STATUS' -ne 0 ] && echo \"\$REFUSED\" | grep -q -- --allow-conflicts"
ok "refusing conflicts changes no operation, commit, or files" \
  "[ \"$(op_id)\" = '$OP_BEFORE' ] && [ \"$(jj log -r @ --no-graph -T commit_id)\" = '$LIVE_BEFORE' ] && cmp -s f.txt '$WORK/cycle-before.txt' && [ \"$(n_extractions a1)\" = 0 ]"
ok "dry run explains that publishing conflicts requires the flag" \
  "echo \"\$PREVIEW\" | grep -q -- --allow-conflicts"
BUILT="$("$BIN" --all --allow-conflicts 2>&1)"
ok "a real cycle is reported by both planning and extraction" \
  "echo \"\$PREVIEW\" | grep -q CONFLICT && echo \"\$BUILT\" | grep -q CONFLICT && { is_conflict '$(cid a1)' || is_conflict '$(cid a2)'; }"
ok "even an unavoidable cycle preserves the clean live working copy" \
  "! is_conflict @ && grep -q '^A1-LATER$' f.txt && [ \"$(n_heads)\" = 1 ]"

echo "== AA: conflict protection also applies to updates and CLI plumbing =="
new_repo aa
edit a1 f.txt $'A1\nl2\nl3\n'
A1_ID="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"
A1_COMMIT="$(jj log -r "$A1_ID" --no-graph -T commit_id)"
edit a2 f.txt $'A2\nl2\nl3\n'
edit a1 f.txt $'A1-LATER\nl2\nl3\n'
OP_BEFORE="$(op_id)"
REFUSED="$(JJ_EXTRACT_AGENT=a1 "$BIN" 2>&1)"
REFUSED_STATUS=$?
ok "a refused re-extraction leaves the previous extracted version intact" \
  "[ '$REFUSED_STATUS' -ne 0 ] && [ \"$(op_id)\" = '$OP_BEFORE' ] && [ \"$(jj log -r "$A1_ID" --no-graph -T commit_id)\" = '$A1_COMMIT' ]"
for MANAGEMENT in --install --uninstall --hook; do
  "$BIN" "$MANAGEMENT" --allow-conflicts >/dev/null 2>&1
  CLI_STATUS=$?
  ok "$MANAGEMENT rejects an irrelevant --allow-conflicts" "[ '$CLI_STATUS' -eq 2 ]"
done

echo "== AB: clean extraction cannot silently conflict an existing descendant =="
new_repo ab
edit a1 f.txt $'A1\nl2\nl3\n'
A1_ID="$(JJ_EXTRACT_AGENT=a1 extract_one a1)"
jj describe -m shared-live >/dev/null 2>&1
LIVE_ID="$(jj log -r @ --no-graph -T 'change_id.short()')"
jj new "$A1_ID" >/dev/null 2>&1
printf 'SIDE\nl2\nl3\n' >f.txt
jj describe -m side-change >/dev/null 2>&1
SIDE_ID="$(jj log -r @ --no-graph -T 'change_id.short()')"
jj edit "$LIVE_ID" >/dev/null 2>&1
edit a1 f.txt $'A1-LATER\nl2\nl3\n'
LIVE_BEFORE="$(jj log -r @ --no-graph -T commit_id)"
OP_BEFORE="$(op_id)"
PREVIEW="$(JJ_EXTRACT_AGENT=a1 "$BIN" --amend --dry-run 2>&1)"
ok "dry run detects descendant conflicts without publishing" \
  "echo \"\$PREVIEW\" | grep -q 'descendant change $SIDE_ID' && [ \"$(op_id)\" = '$OP_BEFORE' ]"
REFUSED="$(JJ_EXTRACT_AGENT=a1 "$BIN" --amend 2>&1)"
REFUSED_STATUS=$?
ok "new descendant conflicts require opt-in before publication" \
  "[ '$REFUSED_STATUS' -ne 0 ] && echo \"\$REFUSED\" | grep -q descendant && [ \"$(op_id)\" = '$OP_BEFORE' ] && ! is_conflict '$SIDE_ID'"
ALLOWED="$(JJ_EXTRACT_AGENT=a1 "$BIN" --amend --allow-conflicts 2>&1)"
ALLOWED_STATUS=$?
ok "explicit opt-in permits and reports the descendant conflict" \
  "[ '$ALLOWED_STATUS' -eq 0 ] && is_conflict '$SIDE_ID' && echo \"\$ALLOWED\" | grep -q 'descendant change $SIDE_ID' && same_tree '$LIVE_BEFORE'"

echo "== AC: complete live trees and on-disk files survive extraction =="
new_repo ac
printf '#!/bin/sh\nprintf hello\n' >run.sh
chmod +x run.sh
ln -s f.txt link.txt
printf '\000binary\377\n' >binary.dat
jj file track run.sh link.txt binary.dat >/dev/null 2>&1
jj describe -m mixed-base >/dev/null 2>&1
jj new >/dev/null 2>&1
codex_edit a1 f.txt $'AGENT\nl2\nl3\n'
codex_add a1 created.txt $'NEW-FILE\n'
printf 'AGENT\nl2\nl3\nNEUTRAL\n' >f.txt
jj status >/dev/null 2>&1
LIVE_BEFORE="$(jj log -r @ --no-graph -T commit_id)"
jj config set --repo snapshot.auto-track 'none()' >/dev/null 2>&1
printf 'untracked\n' >untracked.txt
disk_manifest >"$WORK/disk-before.txt"
BUILT="$(JJ_EXTRACT_AGENT=a1 "$BIN" 2>&1)"
EXTRACT_STATUS=$?
disk_manifest >"$WORK/disk-after.txt"
ok "all tracked content, file types and modes have the identical tree" \
  "[ '$EXTRACT_STATUS' -eq 0 ] && same_tree '$LIVE_BEFORE'"
ok "tracked and untracked files remain byte-for-byte unchanged" \
  "cmp -s '$WORK/disk-before.txt' '$WORK/disk-after.txt'"
JJ_EXTRACT_AGENT=a1 "$BIN" >/dev/null 2>&1
REEXTRACT_STATUS=$?
ok "re-extraction verifies and preserves the same complete live tree" \
  "[ '$REEXTRACT_STATUS' -eq 0 ] && same_tree '$LIVE_BEFORE'"

echo "== AD: mismatched working-copy trees are rejected even with opt-in =="
new_repo ad
edit a1 f.txt $'AGENT\nl2\nl3\n'
cp f.txt "$WORK/stale-before.txt"
jj --ignore-working-copy edit @- >/dev/null 2>&1
OP_BEFORE="$(op_id)"
REFUSED="$(JJ_EXTRACT_AGENT=a1 "$BIN" --allow-conflicts 2>&1)"
REFUSED_STATUS=$?
ok "tree verification fails before publishing or checking out a stale workspace" \
  "[ '$REFUSED_STATUS' -ne 0 ] && echo \"\$REFUSED\" | grep -q 'live tree verification failed' && [ \"$(op_id)\" = '$OP_BEFORE' ] && cmp -s f.txt '$WORK/stale-before.txt'"


echo "== AE: repeated extraction appends only pending edits =="
new_repo ae
edit a1 f.txt $'l1\nl2\nl3\nFIRST\n'
BUILT="$("$BIN" --agent a1 2>&1)"
FIRST="$(cid a1)"
jj describe -r "$FIRST" -m first-chunk >/dev/null 2>&1
FIRST_HASH="$(jj log -r "$FIRST" --no-graph -T commit_id)"
edit a1 f.txt $'l1\nl2\nl3\nFIRST\nSECOND\n'
BEFORE="$(jj log -r @ --no-graph -T commit_id)"
OP_BEFORE="$(op_id)"
PREVIEW="$("$BIN" --agent a1 --dry-run 2>&1)"
ok "incremental preview proposes a new change without consuming edits" \
  "echo \"\$PREVIEW\" | grep -q 'a new change' && [ \"$(op_id)\" = '$OP_BEFORE' ]"
BUILT="$("$BIN" --agent a1 2>&1)"
SECOND="$(cid a1)"
ok "default creates a separate second chunk with only new edits" \
  "[ -n '$SECOND' ] && [ '$FIRST' != '$SECOND' ] && has '$SECOND' SECOND && nothas '$SECOND' FIRST && same_tree '$BEFORE'"
ok "default leaves the earlier commit and description untouched" \
  "[ \"$(jj log -r "$FIRST" --no-graph -T commit_id)\" = '$FIRST_HASH' ] && [ \"$(desc "$FIRST")\" = first-chunk ]"
OP_BEFORE="$(op_id)"
EMPTY="$("$BIN" --agent a1 2>&1)"
ok "no pending edits creates neither a commit nor an operation" \
  "echo \"\$EMPTY\" | grep -q 'Nothing new' && [ \"$(op_id)\" = '$OP_BEFORE' ]"
edit a1 f.txt $'l1\nl2\nl3\nFIRST\nSECOND\nTHIRD\n'
BUILT="$("$BIN" --agent a1 --amend 2>&1)"
ok "amend updates only the latest chunk" \
  "has '$SECOND' SECOND && has '$SECOND' THIRD && nothas '$SECOND' FIRST && [ \"$(jj log -r "$FIRST" --no-graph -T commit_id)\" = '$FIRST_HASH' ]"
SECOND_HASH="$(jj log -r "$SECOND" --no-graph -T commit_id)"
edit a1 f.txt $'l1\nl2\nl3\nFIRST\nSECOND\nTHIRD\nFOURTH\n'
BUILT="$("$BIN" --agent a1 2>&1)"
FOURTH="$(cid a1)"
ok "default after amend begins another independent chunk" \
  "[ -n '$FOURTH' ] && [ '$FOURTH' != '$SECOND' ] && has '$FOURTH' FOURTH && nothas '$FOURTH' THIRD && [ \"$(jj log -r "$SECOND" --no-graph -T commit_id)\" = '$SECOND_HASH' ]"
jj undo >/dev/null 2>&1
BUILT="$("$BIN" --agent a1 2>&1)"
ok "undo restores pending edits as well as the graph" \
  "has '$(cid a1)' FOURTH && nothas '$(cid a1)' THIRD"
# Squashing chunks is a normal user action; the operation checkpoint must still
# remember their edits even when an extraction change disappears.
jj squash --from "$(cid a1)" --into "$SECOND" -m combined >/dev/null 2>&1
SQUASH_STATUS=$?
OP_BEFORE="$(op_id)"
EMPTY="$("$BIN" --agent a1 2>&1)"
ok "manual squash does not cause historical edits to be extracted again" \
  "[ '$SQUASH_STATUS' -eq 0 ] && echo \"\$EMPTY\" | grep -q 'Nothing new' && [ \"$(op_id)\" = '$OP_BEFORE' ]"

echo "== AF: independent session checkpoints and interleaved dependencies =="
new_repo af
edit a1 f.txt $'X\nl2\nl3\n'
BUILT="$("$BIN" --agent a1 2>&1)"
X1="$(cid a1)"
X1_HASH="$(jj log -r "$X1" --no-graph -T commit_id)"
edit a2 f.txt $'Y\nl2\nl3\n'
BUILT="$("$BIN" --all 2>&1)"
Y1="$(cid a2)"
ok "all extracts only sessions with pending edits" \
  "[ -z '$(cid a1)' ] && [ -n '$Y1' ] && has '$Y1' Y"
edit a1 f.txt $'X2\nl2\nl3\n'
BEFORE="$(jj log -r @ --no-graph -T commit_id)"
BUILT="$("$BIN" --agent a1 2>&1)"
X2="$(cid a1)"
ok "X then Y then X appends cleanly instead of creating an amend cycle" \
  "[ -n '$X2' ] && ! is_conflict '$X2' && [ \"$(jj log -r "$X2-" --no-graph -T 'change_id.short()')\" = '$Y1' ] && same_tree '$BEFORE' && [ \"$(jj log -r "$X1" --no-graph -T commit_id)\" = '$X1_HASH' ]"


echo "== AG: amendment retains manual content and descriptions =="
new_repo ag
edit a1 f.txt $'l1\nl2\nl3\nFIRST\n'
BUILT="$("$BIN" --agent a1 2>&1)"
FIRST="$(cid a1)"
jj describe -m shared-live >/dev/null 2>&1
LIVE_ID="$(jj log -r @ --no-graph -T 'change_id.short()')"
jj edit "$FIRST" >/dev/null 2>&1
printf 'MANUAL\n' >>f.txt
jj describe -m reviewed-content >/dev/null 2>&1
jj edit "$LIVE_ID" >/dev/null 2>&1
edit a1 f.txt $'l1\nl2\nl3\nFIRST\nMANUAL\nNEXT\n'
BEFORE="$(jj log -r @ --no-graph -T commit_id)"
BUILT="$("$BIN" --agent a1 --amend 2>&1)"
ok "amend preserves manual edits instead of reconstructing over them" \
  "has '$FIRST' FIRST && has '$FIRST' MANUAL && has '$FIRST' NEXT && [ \"$(desc "$FIRST")\" = reviewed-content ] && same_tree '$BEFORE'"
for mode in --install --uninstall --hook; do
  "$BIN" "$mode" --amend >/dev/null 2>&1
  STATUS=$?
  ok "$mode rejects an irrelevant --amend" "[ '$STATUS' -ne 0 ]"
done

echo
echo "RESULT: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
