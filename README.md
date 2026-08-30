# jj-extract

`jj-extract` separates edits made by multiple Claude Code or Codex sessions that
share a single [Jujutsu](https://jj-vcs.github.io/jj/latest/) working copy. Agent
hooks record each file edit in `jj`'s evolution log; later, `jj extract`
reconstructs one session's edits as an independent change.

```console
$ jj extract -m "Improve install errors"
✓ extracted session 2fb... → change yxw... in stack (inspect: jj show yxw...)
```

There is no start or tracking command. Once the hooks are installed, recording
is automatic and extraction is idempotent: running it again updates the same
session change instead of creating duplicates.

## Requirements

- A `jj` 0.44.x repository (the CLI is needed to invoke `jj extract`)
- Rust 1.89 or newer and Cargo to install from source
- Claude Code or Codex for automatic edit recording

`jj-extract` is intentionally jj-native. A plain Git repository without a `.jj`
working copy is not supported.

## Install

From this repository:

```bash
cargo install --locked --path .
jj-extract --install
```

The second command does three things:

1. Merges `SessionStart`, `PreToolUse`, and `PostToolUse` entries into
   `~/.claude/settings.json`.
2. Merges native `apply_patch` hooks into `~/.codex/hooks.json` (or
   `$CODEX_HOME/hooks.json`).
3. Adds a user-level `jj extract` alias that invokes the installed binary.

To install both integrations for only the current repository, run this from
inside that jj working copy:

```bash
jj-extract --install --project
```

This writes `.claude/settings.json` and `.codex/hooks.json` in the repository.
The `jj extract` alias is still user-level because jj aliases are independent of
agent project settings. Existing settings and hooks are preserved. When an
existing config file is changed, its previous contents are saved beside it with
the `.jj-extract-bak` suffix. Both destinations are validated before either is
changed, so invalid JSON or an unexpected hooks structure cannot cause a partial
install.

Verify the installation:

```bash
jj extract --help
jj config get aliases.extract
```

Restart active agent sessions after installing. Codex requires new or changed
non-managed hooks to be reviewed and trusted: open `/hooks` after restart. A
project install also requires Codex to trust the repository's `.codex` layer;
see the [Codex hooks guide](https://developers.openai.com/codex/hooks).

## Use

An individual session normally runs one command when its work is ready:

```bash
jj extract -m "Short description of the work"
```

Useful variants:

```bash
jj extract --agent <session-id>  # extract an explicitly named session
jj extract --all                 # extract every recorded session
```

`--agent` is useful outside the originating agent process. In normal use,
identity comes from `JJ_EXTRACT_AGENT`, `CLAUDE_CODE_SESSION_ID`, or
`CODEX_THREAD_ID`.

After extraction, the shared live working-copy change remains checked out and its
files are unchanged. Extracted changes are inserted as a chronological stack
between the original base and live `@`; the live change is rebased on top. Each
stack entry's diff contains only that session's edits, while edits that were not
attributed remain in `@`.

Extraction is committed through `jj-lib` as one repository transaction, even
when several session changes are built or updated. One `jj undo` therefore
reverses one complete extraction; undoing a re-extraction restores the prior
version of the same extracted change. Because the library API is versioned with
jj, jj-extract currently embeds `jj-lib` 0.44.0 and supports jj 0.44.x.
Recording, evolution traversal, commit lookup, and extraction all use `jj-lib`
directly; jj-extract never spawns the `jj` CLI. Installation writes its owned
`conf.d/zz-jj-extract.toml` alias fragment with jj-lib's config API.

This produces one linear head instead of a sibling branch per session. It also
lets a later session build on an earlier session's extracted change, avoiding
false conflicts for causally dependent edits. Inspect an entry with the
`jj show <change-id>` command printed in the result.

If an older jj-extract release already left session changes as sibling heads,
the next extraction linearizes those owned changes while preserving their change
IDs.

If two sessions edit the same lines, jj may produce a conflict. `jj-extract`
reports that explicitly and leaves the conflict in the extracted change for
normal jj conflict resolution.

## How recording works

```text
PreToolUse                         PostToolUse
  acquire repository edit lock      snapshot with the session as jj's op user
  neutral jj-lib snapshot            release the edit lock
  allow the file tool to run

`jj extract`
  read @'s evolutions → replay session deltas → stack them → rebase live @ on top
```

The neutral pre-snapshot separates changes already present on disk from the
upcoming tool edit. The lock in `.jj/jj-extract.lock` prevents two file tools
from writing during the same attribution window. It is time-bounded so a missing
post-hook cannot permanently block later edits.

Extraction uses content-addressed snapshots and jj's three-way merge rather than
remembering line numbers. That keeps attribution correct when another session
inserts or deletes lines earlier in the same file.

## Scope and limitations

- Automatic attribution covers Claude Code's `Edit`, `Write`, and `MultiEdit`
  tools and Codex's `apply_patch` tool, including add, update, delete, and move
  paths.
- Changes made by Bash commands, formatters, humans, or other tools are recorded
  neutrally and are not included in a session's extracted change.
- Recording adds a fast jj snapshot around every supported file edit.
- Hook failures never block the agent's tool call. Best-effort diagnostics are
  appended to `~/.jj-extract/hook-error.log` (or
  `$JJ_EXTRACT_HOME/hook-error.log`).
- Extraction failures are shown on stderr. The command attempts to restore the
  original working copy before returning a non-zero exit status.

## Troubleshooting

**`Could not determine which session you are`**

Restart the agent after installation, or pass `--agent <session-id>`. In Codex,
also review the integration with `/hooks`. For a deliberately named process,
export `JJ_EXTRACT_AGENT` before editing.

**`Not inside a jj repo`**

Run the command within a directory whose ancestor contains `.jj`. If this is a
Git repository, colocate jj first with `jj git init --colocate` if that matches
your workflow.

**`jj extract` is not recognized**

Confirm `jj-extract` is on `PATH`, rerun `jj-extract --install`, and inspect
`jj config get aliases.extract`. Installer failures now return a non-zero status
with the underlying `jj config` error.

**Nothing is extracted**

Only supported file-tool edits made after hook installation are attributed.
Check `jj evolog -r @` and the hook error log. Bash-created changes are omitted
by design.

## Uninstall

```bash
jj-extract --uninstall            # global Claude/Codex hooks and the jj alias
jj-extract --uninstall --project  # project Claude/Codex hooks and the jj alias
```

Other Claude Code and Codex settings and hook entries are left intact.

## Development

Run the complete local quality gate:

```bash
scripts/check.sh
```

It checks formatting, runs Clippy with warnings denied, executes the Rust unit
tests, rebuilds the real binary, and then runs the shell integration suite.

The integration suite drives synthetic Claude Code and Codex hook JSON through
the real binary and currently covers:

- interleaved same-file edits whose line positions move;
- concurrent edits serialized by the repository lock;
- exclusion of non-tool changes;
- single-session and all-session extraction;
- composition of multiple edits and new files;
- stable, duplicate-free re-extraction;
- a single linear head with no divergent change IDs;
- conflict-free stacking of causally dependent session edits;
- automatic linearization of legacy sibling extraction heads;
- single-operation extraction and re-extraction with one-step `jj undo`;
- operation with no `jj` executable available to the jj-extract process;
- isolated dual-client install/uninstall and settings preservation;
- Codex `apply_patch` attribution and `CODEX_THREAD_ID` identity;
- malformed settings and incompatible CLI option failures.

To run only that suite:

```bash
cargo build --locked
tests/integration.sh
```

Source layout:

- `src/hook.rs` records attributed snapshots.
- `src/construct.rs` reconstructs isolated changes.
- `src/jj.rs` records and reads repository state through `jj-lib`.
- `src/jj_config.rs` manages jj-extract's owned user-config fragment.
- `src/install.rs` safely merges and removes Claude Code and Codex hooks.
- `src/lock.rs` implements the cross-hook edit lock.
- `.agents/skills/jj-extract` teaches Codex the extraction workflow.
- `.claude/skills/jj-extract` provides the equivalent Claude Code skill.

## License

[MIT](LICENSE)
