# commit-mychanges

Multiple agents work in **one shared working directory**; this tool records
which agent touched which file, so each agent can later harvest **its own**
changes into a separate commit — `jj split` for Jujutsu, a
`refs/mychanges/<agent>` commit for git.

It is a small CLI (`mychanges`) plus a Claude Code hook that does the recording.
Register the hook once with `mychanges install` and recording is **automatic in
every git/jj repo** — no per-repo setup. Attribution is stored centrally under
`~/.claude/mychanges/`, so nothing is written into your repos.

## How it works

```
              ┌─ PostToolUse(Edit/Write/MultiEdit) ─ exact file paths, always
hook (global) ┤
              └─ Pre/PostToolUse(Bash) ── snapshot-diff of the tree, optional
                          │
                          ▼
     ~/.claude/mychanges/<repo>-<hash>/attribution.db   (agent → files, SQLite/WAL)
                          │
        agent runs ──────►├─ mychanges mine     list my still-live changes
                          ├─ mychanges list     numbered agents + overlaps
                          └─ mychanges commit    jj split / git per-agent commit
```

**Tiered attribution.** `Edit`/`Write`/`MultiEdit` carry the exact file path in
the hook payload, so those are attributed precisely and always. `Bash` can
change files in arbitrary ways (`sed -i`, codegen, `mv`), so it's handled by an
**optional** before/after snapshot-diff of the working tree — off by default
because it walks the tree on every Bash call.

**Identity.** Each agent is identified by its Claude Code `session_id` (distinct
per top-level `claude` process). A `SessionStart` hook stamps
`MYCHANGES_AGENT=<session_id>` into `$CLAUDE_ENV_FILE` so the agent's own CLI
calls know who they are. To name agents yourself, export `MYCHANGES_AGENT`
before launching `claude` — it's respected end to end.

## Install

```bash
uv tool install /path/to/commit-mychanges      # puts `mychanges` on PATH
# or, for development from this repo:
uv run mychanges --help
```

## Set up once

```bash
mychanges install            # register hooks in ~/.claude/settings.json (every repo)
mychanges install --project  # or just this repo's .claude/settings.json
mychanges uninstall          # remove them again (leaves your other hooks intact)
```

`install` merges into your existing settings (preserving every other key and
hook) and writes a `*.mychanges-bak` backup first. After this, recording is
automatic in every git/jj repo — the hook is inert outside a repo and writes
data centrally, never into the working tree.

`mychanges init` is **optional** now: use it only to turn on Bash attribution
for a repo (`mychanges init --bash`) or pre-create its store. To enable Bash
attribution everywhere, put `[attribution]\nbash = true` in
`~/.claude/mychanges/config.toml`.

## Use

Just edit, in any repo. To inspect or harvest changes — including when *you* run
the tool and don't know the agent ids:

```bash
mychanges list                       # numbered agents + their changes (+ overlaps)
mychanges mine                       # files I (this session) changed, still live
mychanges commit --agent 1 -m "..."  # harvest agent [1] from `list` (or a name prefix)
mychanges commit -m "add parser"     # harvest my own changes
mychanges commit -m "..." --dry-run  # show the exact jj/git commands first
```

`list` numbers every agent so you never need the raw id — pass the number (or a
unique name prefix) to `--agent` on `mine`/`commit`. `mychanges list --repos`
shows all repos that have recordings.

- **jj:** `commit` runs `jj file track <my paths>` (needed when
  `snapshot.auto-track` is off) then `jj split -m <msg> <my paths>`, peeling your
  files into their own commit and leaving everyone else's changes in `@`. Run
  agents' commits one after another to stack them.
- **git:** `commit` builds a temporary index from `HEAD`, stages only your
  paths, and writes a commit to `refs/mychanges/<agent>` **without** moving
  `HEAD` or touching the working tree. Inspect with `git show <ref>`; then
  cherry-pick / branch / merge as you like. (The tool sets `JJ_GIT_OK=1` on its
  own git calls to pass a `git`-in-jj guard if you run one.)

## The one caveat

If two agents edit **overlapping regions of the same file**, the file on disk
holds only the final bytes — there is no way to split that into two independent
commits. This tool works at **file granularity**: `list`/`mine` flag any file
touched by more than one agent, and `commit` takes the current on-disk content
(last writer wins) for shared files. Partition work across agents to avoid this.

Bash attribution is **best-effort**: within one agent its tool calls are
sequential so its before/after window is clean, but concurrent agents' windows
overlap, so a file changed by a Bash command may be attributed to whichever
agent's window saw it.

## Config

A repo's config lives at `~/.claude/mychanges/<repo>-<hash>/config.toml`, with a
global fallback at `~/.claude/mychanges/config.toml`:

```toml
[attribution]
bash = false          # attribute Bash-changed files via snapshot-diff
ignore = [ "**/node_modules/**", "**/.venv/**", ... ]   # skipped during Bash scans
```

Toggling `bash` takes effect immediately. Set `MYCHANGES_HOME` to relocate the
central store.

## Commands

| Command | What it does |
|---|---|
| `mychanges install [--global/--project]` | Register the hooks in Claude settings (run once) |
| `mychanges uninstall [--global/--project]` | Remove the hooks (leaves other hooks intact) |
| `mychanges init [--bash] [--force]` | Optional: enable Bash attribution for this repo / pre-create its store |
| `mychanges list [--repos] [--json]` | Numbered agents + their changes (or, with `--repos`, all repos) |
| `mychanges mine [--agent A] [--json] [--paths]` | My still-live changed files |
| `mychanges commit -m MSG [--agent A] [--dry-run] [--keep]` | Harvest an agent's changes into a commit (`A` = id, index, or prefix) |
| `mychanges reset [--agent A] [--all]` | Forget attribution records (never touches files/commits) |
| `mychanges where` | Print the central data dir for the current repo |
| `mychanges hook` | Hook entry point (used in settings.json; not for manual use) |

Attribution data lives entirely under `~/.claude/mychanges/` — nothing is
written into your repositories.
