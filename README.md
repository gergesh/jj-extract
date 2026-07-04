# jj-extract

Multiple Claude Code agents share **one** jj working copy. Every edit is recorded
automatically into jj's evolution log — tagged with the acting session — and
**`jj extract`** pulls a session's edits into their **own jj change**, isolated at
line granularity, with jj's rebase engine doing the content math.

There's nothing to start: recording is a hook that tags each edit's snapshot.
`jj extract` is the one command an agent runs, when it wants its work as a commit.

```
record (a hook, per edit)                     extract (jj extract)
  Pre : jj st (neutral) + take edit lock         read @'s evolog
  Post: jj st tagged with the session            for my tagged evolutions:
        release lock                               delta = diff(prev, this)
  → the evolution is attributed in the op log      rebase/squash onto @'s parent
```

Installed as `jj-extract`, it's also the native jj subcommand `jj extract …`.

## The one command agents use

```bash
jj extract -m "what I did"   # pull my edits into their own change
jj extract --all             # build a change for every session in the evolog
```

## The idea: jj's evolog *is* the ledger

`jj st` snapshots the working copy into a content-addressed commit, and `jj evolog
-r @` lists every such snapshot with the *operation* that made it. If each edit's
snapshot is tagged with its agent (`JJ_OP_USERNAME`), then the evolog already
records who changed what — no sidecar, no separate store. `jj extract` reads it
back: an agent's change is the composition of *its* evolutions' deltas, replayed
onto `@`'s parent.

Why not track line numbers? Because they lie — if agent 1 appends at the bottom
and agent 2 inserts at the top, agent 1's line moved. jj-extract does no line
bookkeeping; it diffs content-addressed snapshots and lets jj's rebase compose
them. Two agents editing the same lines is the one irreducible case, surfaced as a
conflict in the extracted change.

## How isolation works

- **Peers.** Each edit is bracketed by an **edit lock** (PreToolUse takes it,
  PostToolUse releases it), so a peer can't write while an edit is in flight —
  your snapshot captures only your edit. The lock guards just a fast
  write+snapshot, and is uncontended when you're editing alone. *(Verified clean
  at 24 simultaneous agents.)*
- **Non-tool changes.** Only `Edit`/`Write`/`MultiEdit` are hooked. A Bash/human
  change to a file lands on disk and is flushed to an *unattributed* evolution by
  the neutral pre-snapshot, so it never folds into an agent's change.

## Install

```bash
cargo install --path .        # puts `jj-extract` on PATH
jj-extract --install          # register the hooks + the `jj extract` alias (global)
jj-extract --install --project  # …or just this repo's .claude/settings.json
jj-extract --uninstall        # remove them again (leaves other hooks intact)
```

`--install` merges into your Claude settings (preserving every other key and hook,
with a `*.jj-extract-bak` backup) and registers `jj extract` as a jj user alias via
`jj util exec`. A **skill** at `.claude/skills/jj-extract/` tells agents to run
`jj extract` when they finish; copy it where your agents run.

Recording is **always-on** once installed — every edit pays a fast tagged snapshot
and an (uncontended-when-solo) lock. That's the cost of never needing a "start"
step; `jj op log` also becomes a permanent record of who edited what.

## CLI

| Invocation | What it does |
|---|---|
| `jj extract [-m MSG]` | Pull this session's edits into their own change |
| `jj extract --all` | Build a change for every session found in the evolog |
| `jj-extract --install [--project]` | Register the hooks + `jj extract` alias |
| `jj-extract --uninstall [--project]` | Remove them |
| `jj-extract --hook` | Hook entry point (used in settings.json; not for manual use) |

## Scope

jj-native, by design (`git` is intentionally not a target). Only the file-editing
tools are recorded; a file changed by an arbitrary `Bash` command is unattributed.

## Test

```bash
cargo build
tests/integration.sh          # drives the real binary via synthetic hook JSON
```

Covers same-file line-shift splitting, concurrent different-file editing,
non-tool-change isolation, single-session extract, and multi-edit composition. The
concurrency ceiling is exercised separately (24 simultaneous agents record and
extract with no loss).
