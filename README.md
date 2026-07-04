# jj-collect

Multiple Claude Code agents work in **one shared jj working copy**. `jj-collect`
routes each agent's edits into that agent's **own jj change**, so every agent's
work ends up isolated in its own commit — automatically, at **line granularity**,
while they type.

```
base ─▶ change(agent-1) ─▶ change(agent-2) ─▶ @   (empty scratch)
         └ only a1's edits   └ only a2's edits
```

It is a tiny CLI (`jj-collect`) plus a Claude Code hook. Because the binary is
named `jj-collect`, jj also exposes it as a native subcommand — `jj collect …`.

## Why not just track line numbers?

Because line numbers lie. If agent 1 appends a line at the bottom of a file and
agent 2 then inserts three lines at the top, agent 1's line has *moved* — any
stored line numbers are now stale. Attribution has to be **content-based**, and
splitting has to do real 3-way merges (especially for deletions, which leave no
line to attribute).

jj already does exactly this. So jj-collect does **no** line bookkeeping of its
own. It hands each edit to jj as a change and lets jj's diff/rebase/squash engine
do all the content math:

- an agent's edits to disjoint regions **compose cleanly**, even as line numbers
  shift underneath them;
- a **deletion** is just content jj tracks — nothing special to record;
- two agents editing the **same lines** is the one irreducible case, and jj
  **surfaces it as a conflict** at harvest instead of silently letting the last
  writer win.

## How it works

On every `Edit`/`Write`/`MultiEdit`, a `PostToolUse` hook:

1. `jj file track`s the edited paths (needed when `snapshot.auto-track=none`);
2. snapshots the working copy into `@`;
3. **squashes only that edit's files** out of `@` and down into the acting
   agent's change (creating it, inserted just beneath `@`, the first time).

Step 3 is the whole trick. `jj squash --from @ --into <agent> -- <paths>` moves
*only the agent's own files*, so a peer's simultaneous edit to a **different**
file stays sitting in `@`, untouched, until that peer's own hook claims it.

### Races are handled

Every repo mutation runs while holding an exclusive `flock`, so concurrent agent
hooks serialize instead of corrupting the shared working copy. Combined with the
path-scoped squash above:

- **different files, any timing** → each agent gets exactly its own file;
- **sequential edits to the same file** → split cleanly by content (line shifts
  and all);
- **truly simultaneous writes to the same file** → the bytes are already merged
  on disk, so that one file goes to whichever hook wins the lock first
  (best-effort, and inherent — you cannot un-merge bytes written at the same
  instant).

A `PreToolUse` hook fires once before the first edit to seal whatever the user
already had into the stack **base**, so pre-existing work is never attributed to
an agent.

### Identity

Each agent is its Claude Code `session_id` (distinct per top-level `claude`
process). A `SessionStart` hook stamps `JJ_COLLECT_AGENT=<session_id>` into
`$CLAUDE_ENV_FILE` so the agent's own CLI calls know who they are. Export
`JJ_COLLECT_AGENT` yourself to name agents.

State (which jj change collects each agent — jj change ids are stable across the
rebases/squashes involved) lives centrally under `~/.claude/jj-collect/`, never
in your repo. `JJ_COLLECT_HOME` relocates it.

## Install

```bash
cargo install --path .          # puts `jj-collect` on PATH
jj-collect install              # register the hooks in ~/.claude/settings.json
jj-collect install --project    # …or just this repo's .claude/settings.json
jj-collect uninstall            # remove them again (leaves other hooks intact)
```

`install` merges into your existing settings (preserving every other key and
hook) and writes a `*.jj-collect-bak` backup first. After that, collection is
automatic in every jj repo.

## Use

Just edit, with multiple agents, in one jj repo. Then:

```bash
jj collect list                       # the stack: each agent + its change + files
jj collect mine                       # the change I (this session) am collecting
jj collect commit --agent 1 -m "..."  # harvest agent [1] onto base as its own commit
jj collect commit -m "add parser"     # harvest my own change
```

`list` numbers every agent, so you pass the number (or a unique name prefix) to
`--agent`. `commit` lifts the agent's collected change onto the stack base with
`jj duplicate`, re-applying it via 3-way merge; if it genuinely overlaps another
agent's edits, the harvested commit is created **with conflict markers** and the
command says so — rather than quietly dropping someone's work. Pass `--in-place`
to just name the in-stack change without lifting a copy onto base.

## Commands

| Command | What it does |
|---|---|
| `jj-collect install [--project]` | Register the hooks in Claude settings (run once) |
| `jj-collect uninstall [--project]` | Remove the hooks (leaves other hooks intact) |
| `jj-collect list [--json]` | The collect stack: agents, their changes, files |
| `jj-collect mine [--agent A]` | The change I'm collecting |
| `jj-collect commit -m MSG [--agent A] [--in-place]` | Harvest an agent's change (`A` = id, index, or prefix) |
| `jj-collect reset [--agent A] [--all]` | Forget collection state (never touches your changes) |
| `jj-collect where` | Print the central data dir for the current repo |
| `jj-collect hook` | Hook entry point (used in settings.json; not for manual use) |

## Scope

jj-native, by design (`git` is intentionally not a target). Attribution is for
the file-editing tools (`Edit`/`Write`/`MultiEdit`); files changed by arbitrary
`Bash` commands are not attributed.

## Test

```bash
cargo build
tests/integration.sh          # drives the real binary via synthetic hook JSON
```

The suite covers sequential same-file line-shift splitting, concurrent
different-file races (including truly parallel hook processes), same-line
conflict surfacing, and new-file creation under `auto-track=none`.
