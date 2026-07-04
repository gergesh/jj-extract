---
name: jj-collect
description: >-
  Use in a jj repository shared by multiple Claude agents where `jj-collect` is
  installed, so this session's edits are collected into their own jj change
  instead of mixing with other agents' work. Run `jj collect` ONCE at the start
  of a coding task, before editing files. Triggers when starting work in a
  shared/multi-agent jj working copy, or when you see a `jj-collect` hook in
  settings.
---

# jj-collect

Several agents may be editing this **one** jj working copy at the same time.
`jj collect` gives *your* session its own jj change and quietly routes every edit
you make into it — so your work stays separate from everyone else's, down to the
line.

## Do this

At the **start** of your task, before your first edit, run once:

```bash
jj collect -m "<short description of what you're doing>"
```

That's the whole workflow. After it, just edit normally (Edit/Write/MultiEdit) —
a hook squashes each edit into your change automatically. You never run it again
for the same task.

- Collecting into an existing change instead of a new one:
  `jj collect --to <rev>`
- It behaves like `jj new` (opens a fresh change) and takes `-m`; it does not
  move the shared working copy `@`, so it's safe to run while other agents work.

## Why

Without it, your edits land in the shared working copy mixed with other agents'
(and any `jj` / Bash side-effects), and can't be told apart afterward. With it,
your change contains **only** the edits you made through your tools — nothing a
teammate or a Bash command touched — even if you and another agent edit the same
file in different places.

## Don't

- Don't run `jj collect` again mid-task (each run opens a *new* change; your
  earlier edits stay in the previous one).
- Don't hand-manage jj changes for your edits — let the hook collect them.
- If `jj-collect` isn't installed (no `jj collect` command), ignore this and
  work normally.
