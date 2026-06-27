# commit-mychanges

Multiple agents work in **one shared working directory**; this tool records
which agent touched which file, so each agent can later harvest **its own**
changes into a separate commit — `jj split` for Jujutsu, a
`refs/mychanges/<agent>` commit for git.

It is a small CLI (`mychanges`) plus a Claude Code hook that does the recording.
Agents use the CLI; the hook is registered once (globally or per-repo) with
`mychanges install`, and each repo opts in with `mychanges init`.

## How it works

```
                 ┌─ PostToolUse(Edit/Write/MultiEdit) ─ exact file paths, always
hook (per repo) ─┤
                 └─ Pre/PostToolUse(Bash) ── snapshot-diff of the tree, optional
                          │
                          ▼
              .mychanges/attribution.db   (agent → files, SQLite/WAL)
                          │
        agent runs ──────►├─ mychanges mine     list my still-live changes
                          ├─ mychanges status   all agents + overlaps
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

## Set up the hooks once (global) vs per-repo

Hook **registration** is separate from per-repo **opt-in**:

```bash
mychanges install            # register hooks once in ~/.claude/settings.json (all repos)
mychanges install --project  # or just this repo's .claude/settings.json
mychanges uninstall          # remove them again (leaves your other hooks intact)
```

`install` merges into your existing settings (preserving every other key and
hook) and writes a `*.mychanges-bak` backup first. The **global** command is
*gated*: the hook only spawns when the session's repo contains a `.mychanges/`
dir, so it costs ~nothing in every repo you haven't opted into. (The gate checks
`$CLAUDE_PROJECT_DIR/.mychanges`; pass `--no-gate` if you launch Claude from a
subdirectory of the repo.)

## Use

Opt a repo in (creates the `.mychanges/` marker the hook gates on — no settings
edit):

```bash
mychanges init            # opt this repo in
mychanges init --bash     # also attribute Bash-driven file changes
```

Restart the agents so the `SessionStart` hook stamps their identity. Then, from
inside any agent:

```bash
mychanges mine                       # files I changed that are still live
mychanges status                     # every agent + cross-agent overlaps
mychanges commit -m "add parser"     # harvest my changes into their own commit
mychanges commit -m "..." --dry-run  # show the exact jj/git commands first
```

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
commits. This tool works at **file granularity**: `status`/`mine` flag any file
touched by more than one agent, and `commit` takes the current on-disk content
(last writer wins) for shared files. Partition work across agents to avoid this.

Bash attribution is **best-effort**: within one agent its tool calls are
sequential so its before/after window is clean, but concurrent agents' windows
overlap, so a file changed by a Bash command may be attributed to whichever
agent's window saw it.

## Config (`.mychanges/config.toml`)

```toml
[attribution]
bash = false          # attribute Bash-changed files via snapshot-diff
ignore = [ "**/node_modules/**", "**/.venv/**", ... ]   # skipped during Bash scans
```

Toggling `bash` takes effect immediately — no re-init needed.

## Commands

| Command | What it does |
|---|---|
| `mychanges install [--global/--project] [--gate/--no-gate]` | Register the hooks in Claude settings (run once) |
| `mychanges uninstall [--global/--project]` | Remove the hooks (leaves other hooks intact) |
| `mychanges init [--bash] [--force]` | Opt this repo in: create the `.mychanges/` marker |
| `mychanges status [--json]` | All agents, their live files, overlaps |
| `mychanges mine [--agent A] [--json] [--paths]` | My still-live changed files |
| `mychanges commit -m MSG [--agent A] [--dry-run] [--keep]` | Harvest my changes into a commit |
| `mychanges reset [--agent A] [--all]` | Forget attribution records (never touches files/commits) |
| `mychanges hook` | Hook entry point (used in settings.json; not for manual use) |

`.mychanges/` holds only transient state (the SQLite log, pre-snapshots); a
`.gitignore` is written there automatically.
