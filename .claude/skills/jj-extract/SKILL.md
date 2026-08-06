---
name: jj-extract
description: >-
  Use in a jj repository shared by multiple Claude Code or Codex agents where
  `jj-extract` is installed. Your edits are recorded automatically; run
  `jj extract` to pull just your session's edits into their own jj change,
  separate from other agents'. Triggers when finishing a chunk of work in a
  shared/multi-agent jj working copy, or when you see a `jj-extract` hook in an
  agent's hook config.
---

# jj-extract

Several agents may be editing this **one** jj working copy at once. Every edit is
recorded automatically (a hook tags it with your session), so there's nothing to
start. When you want your work as its own commit, run:

```bash
jj extract -m "<short description of what you did>"
```

That reconstructs a jj change containing **only your** edits — down to the line,
separate from everyone else's — and prints its id. Extracted session changes are
kept in first-edit order as a linear stack below the live working copy, so the
command does not leave a sibling branch per agent.

## Why

Without it, your edits sit in the shared working copy mixed with other agents'
(and any `jj`/Bash side-effects) and can't be told apart. `jj extract` pulls out
exactly what you changed through your tools — even if you and another agent edited
the same file in different places, and even if a Bash command touched a file you
also edited (that lands unattributed, not in your change).

## Notes

- Nothing to do up front — just edit; recording is automatic.
- `jj extract` is safe to run alongside other agents.
- The live working-copy change and file contents are preserved; attributed edits
  move into the extraction stack and unattributed edits remain in live `@`.
- `jj extract --all` builds a change for every session (for whoever is collecting
  everyone's work).
- If `jj-extract` isn't installed (no `jj extract` command), ignore this and work
  normally.
