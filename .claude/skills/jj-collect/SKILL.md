---
name: jj-collect
description: >-
  Use in a jj repository shared by multiple Claude agents where `jj-collect` is
  installed, so this session's edits are recorded and can be built into their own
  jj change instead of mixing with other agents' work. Run `jj collect` ONCE at
  the start of a coding task before editing, and `jj collect --build` when done.
  Triggers when starting work in a shared/multi-agent jj working copy, or when you
  see a `jj-collect` hook in settings.
---

# jj-collect

Several agents may be editing this **one** jj working copy at once. `jj collect`
records *your* session's edits so they can later be reconstructed into a change
containing only your work — down to the line, kept separate from everyone else's.

## Do this

At the **start** of your task, before your first edit, run once:

```bash
jj collect -m "<short description of what you're doing>"
```

Then edit normally (Edit/Write/MultiEdit) — a hook records each edit. You do not
run it again mid-task.

When you're **done** (or want to materialize your work), build your change:

```bash
jj collect --build
```

That reconstructs your edits into their own jj change and prints its id.

## Why

Without it, your edits sit in the shared working copy mixed with other agents'
(and any `jj`/Bash side-effects) and can't be told apart. With it, your built
change contains **only** the edits you made through your tools — even if you and
another agent edit the same file in different places, and even if a Bash command
touched a file you also edited.

## Notes

- `jj collect` is cheap and safe to run alongside other agents — it doesn't move
  the working copy or take a lock; it just marks your session.
- Your change appears when you run `--build`, not live as you type.
- If `jj-collect` isn't installed (no `jj collect` command), ignore this and work
  normally.
