---
name: jj-extract
description: >-
  Use in a jj repository shared by multiple Claude Code or Codex agents where
  jj-extract is installed. Edits are recorded automatically; run `jj extract`
  to pull only the current session's edits into one linear jj change. Trigger
  when finishing work in a shared or multi-agent jj working copy, or when a
  jj-extract hook is configured.
---

# jj-extract

Several agents may edit one jj working copy at once. Hooks record every supported
file edit with the current session identity, so there is nothing to start. When
the current work is ready, run:

```bash
jj extract
```

Then describe that change, reading its diff first:

```bash
jj show <change-id>
jj describe -r <change-id> -m "<what changed and why>"
```

Use the printed change id for review or handoff. Extracted session changes form
a linear stack below the live working copy. Each extraction creates a new change
with only edits since this session’s previous extraction; no pending edits means
no change. New changes can be reordered to avoid conflicts. Read the printed
placement; do not assume first-edit order or create a separate branch.

## Rules

- Edit normally; recording is automatic for Codex `apply_patch` and supported
  Claude Code file tools, and for a Claude Code Bash call whose only effect is
  writing files.
- Run `jj extract` after a coherent chunk of work, before handing it off.
- Use `--amend` only when intentionally adding pending edits to the session’s
  latest extracted change. It may move or rebase earlier extracted changes to
  avoid conflicts. Default extraction preserves them exactly; chunks can be
  combined later with `jj squash`.
- Describe the extracted change with `jj describe`. `jj extract` takes no `-m`
  and writes no description of its own; it never overwrites one already there.
- Use `jj extract --all` only when intentionally collecting every recorded
  session.
- Preserve the live working-copy change. Attributed edits move into the stack;
  independent shell, formatter, or human edits remain in live `@`. An
  overlapping neutral rewrite may move with a later attributed edit on the same
  path when separating them would create a conflict.
- Extraction refuses to publish conflicts by default. Use `--dry-run` to inspect
  the proposed stack; pass `--allow-conflicts` only when intentionally accepting
  conflicts. If accepted, inspect the printed changes with `jj show` and report
  the conflicts instead of hiding them.
- The tool verifies that the live tree is unchanged. A verification failure must
  be investigated; `--allow-conflicts` does not bypass this check.
- Formatting can be dropped or adopted to avoid conflicts. The tool uses
  word-level merging and optional neutral context. With rustfmt installed,
  Rust formatter-only edits are optional even when made by an agent. Real
  conflicts between substantive edits are preserved. Cyclic dependencies may still require resolution.
- If the command is unavailable, continue working normally and mention that the
  integration is not installed.
