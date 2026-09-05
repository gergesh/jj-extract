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
jj extract
```

That reconstructs a jj change containing your attributed edits and prints its
id. Extracted session changes form a linear stack below the live working copy.
The tool can reorder them to avoid conflicts, including moving an existing
change while preserving its ID. Read the printed placement rather than assuming
first-edit order.

Then describe it — after looking at what actually landed in it, not from memory:

```bash
jj show <change-id>
jj describe -r <change-id>   # or -m "<what changed and why>"
```

`jj extract` writes no description of its own, and never overwrites yours:
re-running it after more edits updates the same change and leaves your words
alone.

## Why

Without it, your edits sit in the shared working copy mixed with other agents'
(and any build, formatter, or `jj` side-effects) and can't be told apart. `jj extract` pulls out
what you changed through your tools — even if you and another agent edited the
same file in different places. Independent Bash changes stay unattributed; an
overlapping rewrite that a later attributed edit depends on may move with it to
avoid a false conflict.

## Notes

- Nothing to do up front — just edit; recording is automatic. Writing a file
  from the shell counts too (`cat > f <<'EOF'`, `tee`, an inline `python3 -`
  script), as long as that is all the command does.
- Describe the change yourself with `jj describe` once you can see the whole
  diff; there is no `-m` on `jj extract`.
- `jj extract` is safe to run alongside other agents.
- The live working-copy change and file contents are preserved; attributed edits
  move into the extraction stack and independent unattributed edits remain in
  live `@`. An overlapping neutral rewrite may move with a later attributed edit
  on the same path when separating them would create a conflict.
- `jj extract --all` builds a change for every session (for whoever is collecting
  everyone's work).
- Formatting can be dropped or adopted to avoid conflicts. The tool uses
  word-level merging and optional neutral context, but preserves real conflicts
  between attributed edits. Conflicts are refused before publication unless
  `--allow-conflicts` is explicitly passed. Use `--dry-run` to inspect the plan;
  if intentionally accepting conflicts, inspect the printed changes with
  `jj show` and report them rather than hiding them.
- The tool verifies that the live tree is unchanged. Investigate a verification
  failure; `--allow-conflicts` does not bypass that check.
- If `jj-extract` isn't installed (no `jj extract` command), ignore this and work
  normally.
