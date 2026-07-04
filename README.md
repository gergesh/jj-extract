# jj-collect

Multiple Claude Code agents share **one** jj working copy. Each agent runs
`jj collect` once; from then on its edits are **recorded**, and `jj collect
--build` reconstructs that agent's edits into their **own jj change** — isolated
at line granularity, with jj's own diff/rebase engine doing the content math.

It leans on the insight that **`jj st` already is the snapshot store**: jj
snapshots the working copy into a content-addressed commit and the op log keeps
every snapshot. So jj-collect never copies files or maintains its own store — it
just records two cheap commit-id pointers around each edit and diffs them later.

```
record (per edit, lock-free)          construct (once, single-threaded)
  Pre : snapshot @  → pre               for each agent, for each edit:
  Post: snapshot @  → post                delta = diff(pre, post) on the edited files
        append (agent, pre, post, files)   rebase/squash it onto the agent's change
```

Installed as `jj-collect`, it's also the native jj subcommand `jj collect …`.

## The commands agents use

```bash
jj collect -m "what I'm doing"    # start recording this session's edits
# … edit normally …
jj collect --build                # build my edits into my own change
```

`--install`/`--uninstall`/`--hook` are plumbing flags. `jj collect --build --all`
builds every collecting session's change at once.

## Why record-and-construct

The obvious design — mutate a live per-agent jj stack on every edit under a lock —
fights itself: the lock needs a stale-timeout so a crashed tool can't wedge
editing, but that timeout breaks mutual exclusion under a heavy concurrent burst.
Recording sidesteps it completely:

- **The hot path takes no lock.** Hooks only run `jj st` (jj serializes snapshots
  on its own working-copy lock) and append one line to a log (a single `O_APPEND`
  `write` — atomic). Nothing does a read-modify-write of shared state, so any
  number of agents record concurrently. *(Tested clean at 16 simultaneous agents;
  the live-lock design degraded at 8.)*
- **Construction is single-threaded**, so it has no concurrency to get wrong.
- **Non-tool edits fall out for free.** A Bash/human change to a file lands in the
  *pre* snapshot of the next edit, so `diff(pre, post)` naturally excludes it — no
  "holding change" needed.
- **Hooks can't wedge editing.** A crashed tool just leaves a dangling `pre`;
  nothing is held.

The trade: it's **deferred, not live** — your change appears at `--build`, not as
you type. Which restores the "don't touch the repo until asked" property.

## Why not track line numbers?

Because line numbers lie: if agent 1 appends at the bottom and agent 2 inserts at
the top, agent 1's line has moved. jj-collect does no line bookkeeping — it diffs
content-addressed snapshots and lets jj's rebase compose them. Disjoint edits
merge cleanly even as lines shift; two agents editing the same lines is the one
irreducible case, surfaced as a conflict in the built change.

## How construction works

For each of an agent's recorded tool calls, in order: check out the `pre`
snapshot, swap the edited files to their `post` content, snapshot to get a **delta
commit**, then rebase/squash it onto the agent's accumulating change. jj's 3-way
merge applies each delta with the right context, so e.g. agent 2's top-insertion
lands as `+TOP` on the base even though it was recorded on top of agent 1's
bottom-append. Building restores the live working copy afterward, so harvesting
doesn't disturb ongoing editing.

## Identity

Each agent is its Claude Code `session_id`. A `SessionStart` hook stamps
`JJ_COLLECT_AGENT=<session_id>` into `$CLAUDE_ENV_FILE` so the `jj collect` you run
knows which session it is. Export `JJ_COLLECT_AGENT` yourself to name agents.
Records live centrally under `~/.claude/jj-collect/`, never in your repo;
`JJ_COLLECT_HOME` relocates them.

## Install

```bash
cargo install --path .        # puts `jj-collect` on PATH
jj-collect --install          # register the hooks + the `jj collect` alias (global)
jj-collect --install --project  # …or just this repo's .claude/settings.json
jj-collect --uninstall        # remove them again (leaves other hooks intact)
```

`--install` merges into your Claude settings (preserving every other key and hook,
with a `*.jj-collect-bak` backup) and registers `jj collect` as a jj user alias via
`jj util exec`. A **skill** at `.claude/skills/jj-collect/` tells agents to run
`jj collect` at the start of a task; copy it where your agents run.

## CLI

| Invocation | What it does |
|---|---|
| `jj collect [-m MSG]` | Start recording this session's edits |
| `jj collect --build [--agent A]` | Build the recorded edits into a change |
| `jj collect --build --all` | Build every collecting session's change |
| `jj-collect --install [--project]` | Register the hooks + `jj collect` alias |
| `jj-collect --uninstall [--project]` | Remove them |
| `jj-collect --hook` | Hook entry point (used in settings.json; not for manual use) |

## Scope

jj-native, by design (`git` is intentionally not a target). Only the file-editing
tools (`Edit`/`Write`/`MultiEdit`) are recorded; a file changed by an arbitrary
`Bash` command counts as non-tool content and is excluded from agents' changes.

## Test

```bash
cargo build
tests/integration.sh          # drives the real binary via synthetic hook JSON
```

Covers same-file line-shift splitting, concurrent different-file recording,
non-tool-change isolation, opt-in, and multi-edit composition. The concurrency
ceiling is exercised separately (16 simultaneous agents record and build with no
loss).
