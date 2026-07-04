# jj-collect

Multiple Claude Code agents share **one** jj working copy. Each agent runs one
command — **`jj collect`** — and from then on its edits are automatically
collected into its own jj change. Every agent's work ends up isolated in its own
commit, at **line granularity**, with jj's own diff/rebase engine doing all the
content math.

```
base ─▶ holding ─▶ change(agent-1) ─▶ change(agent-2) ─▶ @   (shared scratch)
 │         │            │                   │
 │         │            └ only a1's edits   └ only a2's edits
 │         └ non-tool ("foreign") edits, unattributed
 └ the user's pre-existing work, sealed
```

It's one command plus a Claude Code hook. Installed as `jj-collect`, it's also
reachable as the native jj subcommand `jj collect …`.

## The one command agents use

```bash
jj collect -m "what I'm about to do"    # like `jj new`: opens my change; edits collect into it
jj collect --to <rev>                   # collect into an existing change instead of a new one
```

Run it **once**, before editing. After that, just edit — a `PostToolUse` hook
squashes each edit into your change. That's the entire workflow; there is no
`list`/`mine`/`commit`/`harvest` — your change simply *is* your collected work,
already separated in the repo.

## Why not track line numbers?

Because line numbers lie. If agent 1 appends a line at the bottom of a file and
agent 2 then inserts three lines at the top, agent 1's line has moved — any
stored line numbers are stale. So jj-collect does **no** line bookkeeping: it
hands each edit to jj as a change and lets jj compose them. Disjoint edits merge
cleanly even as lines shift; deletions are just content; two agents editing the
same lines is the one irreducible case.

## How it works

Each edit is **bracketed** by two hooks, for a session that has opted in via
`jj collect`:

- **PreToolUse** parks the about-to-be-edited files' *current* `@` content into a
  neutral **holding change** — so `@` is clean for them.
- **PostToolUse** squashes what's now in `@` for those files (which is *only*
  this tool use) down into the agent's change.

The bracket is what makes a collected change contain **only** the agent's own
tool edits: anything else that touched the same file — a `jj`/Bash side-effect,
the human, another agent — was parked into the holding change and never leaks in.

Both hooks are **path-scoped** (`jj squash … -- <paths>`): they touch only the
files of the edit at hand, leaving a concurrent agent's other files alone.

### Races are handled

Every repo mutation (collect, and both hooks) runs while holding an exclusive
`flock`, so concurrent agents serialize instead of corrupting the shared working
copy. Combined with path-scoped squashes:

- **different files, any timing** → each agent gets exactly its own file;
- **sequential edits to the same file** → split cleanly by content (line shifts
  and all);
- **truly simultaneous writes to the same file** → the bytes are already merged
  on disk, so that one file goes to whichever hook wins the lock first
  (best-effort, and inherent).

`jj collect` never *moves* the shared `@` (it inserts your change beneath it), so
a peer's not-yet-squashed edit is never absorbed into the wrong change.

### Identity

Each agent is its Claude Code `session_id`. A `SessionStart` hook stamps
`JJ_COLLECT_AGENT=<session_id>` into `$CLAUDE_ENV_FILE`, so the `jj collect` you
run knows which session it is. Export `JJ_COLLECT_AGENT` yourself to name agents.

State (which change each session collects into; jj change ids are stable across
the squashes involved) lives centrally under `~/.claude/jj-collect/`, never in
your repo. `JJ_COLLECT_HOME` relocates it.

## Install

```bash
cargo install --path .        # puts `jj-collect` on PATH
jj-collect --install          # register the hooks + the `jj collect` alias (global)
jj-collect --install --project  # …or just this repo's .claude/settings.json
jj-collect --uninstall        # remove them again (leaves other hooks intact)
```

`--install` merges into your Claude settings (preserving every other key and
hook, with a `*.jj-collect-bak` backup) and registers `jj collect` as a jj user
alias via `jj util exec`. A **skill** at `.claude/skills/jj-collect/` tells agents
to run `jj collect` at the start of a task; copy it where your agents run.

## CLI

| Invocation | What it does |
|---|---|
| `jj collect [-m MSG]` | Open a change and collect this session's edits into it |
| `jj collect --to <rev>` | Collect into an existing change instead |
| `jj-collect --install [--project]` | Register the hooks + `jj collect` alias |
| `jj-collect --uninstall [--project]` | Remove them |
| `jj-collect --hook` | Hook entry point (used in settings.json; not for manual use) |

## Scope

jj-native, by design (`git` is intentionally not a target). Only the file-editing
tools (`Edit`/`Write`/`MultiEdit`) are collected; files changed by arbitrary
`Bash` commands are treated as non-tool content and land in the holding change.

## Test

```bash
cargo build
tests/integration.sh          # drives the real binary via synthetic hook JSON
```

Covers sequential same-file line-shift splitting, concurrent different-file races
(including truly parallel hook processes), non-tool-change isolation via the
holding change, opt-in (an uncollected session is left alone), and new-file
creation under `auto-track=none`.
