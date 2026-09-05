# jj-extract

`jj-extract` separates edits made by multiple Claude Code or Codex sessions that
share a single [Jujutsu](https://jj-vcs.github.io/jj/latest/) working copy. Agent
hooks record each file edit in `jj`'s evolution log; later, `jj extract`
reconstructs one session's edits as an independent change.

```console
$ jj extract
✓ extracted session 2fb... → change yxw... in stack (inspect: jj show yxw...)
$ jj describe -r yxw... -m "Improve install errors"
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
jj extract
```

Extraction writes no description. A new change starts with a `jj-extract:
<session>` placeholder for its author to replace with `jj describe`, having read
the change that was actually built; re-extraction then leaves that description
alone. Descriptions written blind, before the change exists, are the ones worth
nobody's time.

Useful variants:

```bash
jj extract --agent <session-id>  # extract an explicitly named session
jj extract --all                 # extract every recorded session
jj extract --dry-run             # report what that would build, changing nothing
jj extract --allow-conflicts     # explicitly permit publishing conflicts
```

`--agent` is useful outside the originating agent process. In normal use,
identity comes from `JJ_EXTRACT_AGENT`, `CLAUDE_CODE_SESSION_ID`, or
`CODEX_THREAD_ID`.

`--dry-run` reports the extraction instead of performing it: which change each
session's edits would land in, whether that change already exists, which files
it would contain, and whether stacking them would conflict. The preview is
trustworthy because it is the real thing up to the last step — the same deltas
composed through the same merges, descendant rebases, and tree verification —
stopping before the transaction is committed. No operation or visible change is
published, and `@` is left exactly as it was. Speculative trees and commits can
be written to the object store, but remain unreferenced. A change that does not
exist yet is reported as `a new change` rather than by a temporary preview ID.
It combines with `--agent` and `--all`. A conflicting preview is allowed without
`--allow-conflicts`, since it publishes nothing; it explains that the real
extraction requires the flag.

```console
$ jj extract --dry-run
Dry run — the repository was not changed.
✓ would extract session 2fb... → a new change (2 files)
    src/install.rs
    README.md
```

After extraction, the shared live working-copy change remains checked out and its
files are unchanged. Extracted changes are inserted as a linear stack
between the original base and live `@`; extraction chooses their order by
replaying the edits and testing for conflicts. The live change is rebased on top while
its exact pre-extraction tree is preserved. Each stack entry's diff contains
that session's edits, while independent edits that were not attributed remain
in `@`.

Extraction verifies this invariant instead of relying only on assigning the old
tree to the rewritten commit. It holds jj's native working-copy lock and checks
the content-addressed live tree (including conflict labels) before extraction,
after preparing all rewrites and descendant rebases, and after publication.
Checkout must report zero added, removed, updated, or skipped files. A mismatch
fails the command; one detected before publication aborts the transaction.
`--allow-conflicts` never bypasses tree verification.

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

An extracted change is credited with the standard `Co-authored-by:` trailer the
session's own agent writes — `Claude <noreply@anthropic.com>` for Claude Code,
`Codex <noreply@openai.com>` for Codex — and nothing else. Which session a
change was built for is recorded in the extraction operation's own metadata, not
in the description, so `jj describe` is free to replace the text entirely: the
next extraction still updates the same change. That ledger is written to the
operation log, so it survives every rewrite of the change but not a discarded
operation history; an extraction whose ledger entry is gone builds a new change
rather than updating the old one.

Re-extraction updates an existing session change only on the same stable
extraction base. The same session extracted from another branch line receives a
distinct change ID instead of rewriting the earlier line.

Both extraction and dry run print the chosen order, including existing session
changes that are moved or rewritten. Change IDs and descriptions survive a move.

### Conflict reduction

First-edit time is a preference, not a fixed position. For example, X can create
an unrelated file, Y can introduce a function, and X can then modify that
function. X started first, but the clean stack is **Y → X → live @**.
Re-extracting X can move its existing change above Y without creating a new ID.

The planner first tries chronological order. If any entry conflicts, it searches
alternative orders, replaying each session directly onto its candidate parent.
It scores conflicted paths across **every intermediate change**, so a clean tip
cannot hide a conflicted parent. The search keeps up to 32 candidate prefixes and
tries at most 4,096 additional session replays; it retains the best complete
stack found. This is deterministic and bounded, rather than an exhaustive
permutation search for large stacks. Dry runs use precisely the same planner.

Formatting is expendable when it would introduce a conflict:

- Word-level merges separate independent edits to different parts of one line.
- Neutral formatter context is found across intervening edits by other sessions,
  and carried only on paths the session subsequently touches.
- A conflicting neutral rewrite is replayed partially for ordinary text files:
  clean hunks provide context, while conflicting hunks keep the destination's
  content. This can discard formatting that depends on another agent's code.
- Cleanup removes only context actually introduced during replay, independently
  per file. Context already owned by the parent is not subtracted again.
- Files still conflicted after contextual replay are also tried without neutral
  context; a clean result wins over retaining that optional rewrite.

These rules apply to optional neutral context. Attributed edits still use full
three-way merges; extraction never resolves a disagreement between agents by
arbitrarily choosing one agent's code. The partial-context fallback is limited
to regular text files of at most 1 MiB per input, with unchanged executable bits
and copy identity. Binary, symlink, add/delete, and pre-existing conflicts retain
ordinary jj merge behavior. Whitespace inside strings or indentation-sensitive
code is not globally stripped or normalized.

Some dependencies cannot fit into one change per session. If X changes a value,
Y replaces it, and X replaces Y's value again, the history needs **X → Y → X**;
neither two-change order necessarily works. A dependency on an unextracted
session can also remain unresolved: `--agent` reorders existing extractions but
does not silently extract new sessions. `--all` makes all recorded sessions
available for placement. By default, any conflicting stack is refused before
publication. Newly conflicted descendant changes also cause the transaction to
be refused, even if the extracted stack itself is clean. The repository is left
unchanged on these refusals. Pass `--allow-conflicts` to publish those conflicts
for normal jj resolution; they are reported explicitly, and live `@` retains
its exact pre-extraction tree.

## How recording works

```text
PreToolUse                         PostToolUse
  acquire repository edit lock      snapshot as the session, tagged with
  note which targets don't exist     its agent in the operation metadata
  neutral jj-lib snapshot           release the edit lock
  allow the file tool to run

`jj extract`
  read @'s evolutions → replay with causal context → remove commuting neutral edits
  → search stack placements → preserve live @'s exact tree on top
```

Recording never starts tracking a file. Each snapshot covers every path jj
already tracks, and only the paths the tool creates — those that did not exist
when its PreToolUse hook ran — are offered to jj as newly trackable. A file left
untracked on purpose therefore stays untracked however often an agent edits it,
and its content is never extracted.

The neutral pre-snapshot separates changes already present on disk from the
upcoming tool edit. The lock in `.jj/jj-extract.lock` prevents two file tools
from writing during the same attribution window. It is time-bounded so a missing
post-hook cannot permanently block later edits.

Extraction uses content-addressed snapshots and jj's three-way merge rather than
remembering line numbers. That keeps attribution correct when another session
inserts or deletes lines earlier in the same file. If a neutral formatter or
shell rewrite touches a path before the agent edits it, extraction
temporarily carries that rewrite as causal context. It removes the neutral delta
again when it commutes cleanly; if removing it would itself conflict, the
overlapping rewrite is adopted into the session instead of manufacturing a
conflict between two snapshots from that session.

## Scope and limitations

- Automatic attribution covers Claude Code's `Edit`, `Write`, and `MultiEdit`
  tools and Codex's `apply_patch` tool, including add, update, delete, and move
  paths.
- It also covers a Bash call that does nothing but write files — a `cat > f
  <<'EOF'` heredoc, a `tee`, an inline `python3 - <<'PY'` script — since agents
  create and rewrite files that way as readily as with a file tool. Every part
  of the command must be a write or a harmless read; one `git`, `cargo`, `rm`,
  or unrecognized word anywhere in it, a backgrounded command, or an effect that
  can't be read off the text (`$(...)`, a subshell) leaves the whole command
  unattributed. Missing an edit only leaves those lines in live `@`; claiming
  one wrongly would fold a formatter's sweep of the repository into an agent's
  change.
- Changes made by other Bash commands, formatters, humans, or other tools are
  recorded neutrally and remain in live `@` when they commute cleanly with
  attributed edits. An overlapping neutral rewrite may be adopted when a later
  attributed edit on the same path causally depends on it.
- An edit to an untracked file is not recorded: only a file an agent creates
  begins being tracked, so edits to ignored or deliberately untracked paths stay
  out of the repository and out of extracted changes.
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

Only supported edits made after hook installation are attributed: the file
tools, and Bash calls that do nothing but write files. Check `jj evolog -r @`
and the hook error log. Anything a broader shell command changed is omitted by
design and stays in live `@`.

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
- automatic reordering of late dependencies, including create/delete edits;
- moving an existing change while preserving its ID, description, and undo;
- word-level composition and partial replay of optional formatter context;
- neutral context across intervening sessions and independent per-file cleanup;
- deterministic planning and explicit reporting of unavoidable dependency cycles;
- conflict refusal without publication, explicit opt-in, and descendant checks;
- live-tree verification, unchanged file bytes/modes/symlinks, and stale-workspace rejection;
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
- `src/neutral.rs` replays the clean hunks of optional neutral context.
- `src/jj.rs` records and reads repository state through `jj-lib`.
- `src/jj_config.rs` manages jj-extract's owned user-config fragment.
- `src/install.rs` safely merges and removes Claude Code and Codex hooks.
- `src/lock.rs` implements the cross-hook edit lock.
- `.agents/skills/jj-extract` teaches Codex the extraction workflow.
- `.claude/skills/jj-extract` provides the equivalent Claude Code skill.

## License

[MIT](LICENSE)
