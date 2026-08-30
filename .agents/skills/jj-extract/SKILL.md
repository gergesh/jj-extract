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

Use the printed change id for review or handoff. Extracted session changes stay
in first-edit order as a linear stack below the live working copy; do not create
a separate branch for the session.

## Rules

- Edit normally; recording is automatic for Codex `apply_patch` and supported
  Claude Code file tools.
- Run `jj extract` after a coherent chunk of work, before handing it off.
- Describe the extracted change with `jj describe`. `jj extract` takes no `-m`
  and writes no description of its own; it never overwrites one already there.
- Use `jj extract --all` only when intentionally collecting every recorded
  session.
- Preserve the live working-copy change. Attributed edits move into the stack;
  independent shell, formatter, or human edits remain in live `@`. An
  overlapping neutral rewrite may move with a later attributed edit on the same
  path when separating them would create a conflict.
- If `jj extract` reports conflicts, inspect the printed change with `jj show`
  and report the conflict instead of hiding it.
- If the command is unavailable, continue working normally and mention that the
  integration is not installed.
