from __future__ import annotations

import json as _json
import sys
from contextlib import contextmanager
from pathlib import Path

import typer

from . import vcs as vcsmod
from .config import load_config, render_config
from .identity import ENV_VAR, from_cli
from .paths import base_dir, mychanges_dir
from .store import Store

app = typer.Typer(
    no_args_is_help=True,
    add_completion=False,
    help="Per-agent change attribution for a shared working dir (jj & git).",
)


def _err(msg: str) -> None:
    typer.secho(msg, fg=typer.colors.RED, err=True)


def _resolve_agent(explicit: str | None) -> str:
    agent = from_cli(explicit)
    if not agent:
        _err(
            f"Could not determine which agent you are. Pass --agent <id> or set ${ENV_VAR}.\n"
            "(`mychanges init` installs a SessionStart hook that sets this automatically.)"
        )
        raise typer.Exit(2)
    return agent


def _open_store(base: Path) -> Store:
    if not base.is_dir():
        _err(f"Not initialised: {base} does not exist. Run `mychanges init` here first.")
        raise typer.Exit(2)
    return Store(base / "attribution.db")


def _mine_paths(store: Store, agent: str, vcs: vcsmod.Vcs | None) -> tuple[list[str], dict[str, set[str]]]:
    """Paths attributed to ``agent`` that are still live in the working copy,
    plus the full path->agents overlap map."""
    candidates = store.paths_for(agent)
    overlaps = store.path_agents()
    if vcs is None:
        return candidates, overlaps
    changed = vcsmod.changed_paths(vcs)
    mine = [p for p in candidates if p in changed]
    return mine, overlaps


@contextmanager
def _commit_lock(base: Path):
    """Serialise commit/split across concurrent agents (they mutate shared VCS
    state — jj's @ or git refs)."""
    import fcntl

    base.mkdir(parents=True, exist_ok=True)
    lock = base / "commit.lock"
    with open(lock, "w") as fh:
        fcntl.flock(fh, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(fh, fcntl.LOCK_UN)


# --------------------------------------------------------------------------- #
@app.command()
def init(
    bash: bool = typer.Option(
        False, "--bash/--no-bash", help="Also attribute files changed by Bash commands."
    ),
    command: str | None = typer.Option(
        None, "--command", help="Override the hook command written to settings.json."
    ),
    settings: Path | None = typer.Option(
        None, "--settings", help="settings.json to patch (default: ./.claude/settings.json)."
    ),
    force: bool = typer.Option(False, "--force", help="Overwrite an existing config.toml."),
) -> None:
    """Set up attribution in the current repo: create .mychanges/ and install hooks."""
    base = mychanges_dir(Path.cwd())
    base.mkdir(parents=True, exist_ok=True)

    cfg_path = base / "config.toml"
    if force or not cfg_path.exists():
        cfg_path.write_text(render_config(bash=bash, ignore=load_config(base).ignore))
    Store(base / "attribution.db").close()  # create the db

    cmd = command or _default_hook_command()
    settings_path = settings or (base_dir(Path.cwd()) / ".claude" / "settings.json")
    added = _merge_settings(settings_path, cmd)

    gitignore = base / ".gitignore"
    if not gitignore.exists():
        gitignore.write_text("# transient attribution state\nattribution.db*\npre/\nhook-error.log\ncommit.lock\n")

    typer.secho(f"Initialised {base}", fg=typer.colors.GREEN)
    typer.echo(f"  config:   {cfg_path}  (bash attribution: {'on' if bash else 'off'})")
    typer.echo(f"  hooks:    {settings_path}  ({'updated' if added else 'already present'})")
    typer.echo(f"  command:  {cmd}")
    typer.echo("\nNext: restart agents in this repo so the SessionStart hook stamps their id,")
    typer.echo("then each agent runs `mychanges mine` / `mychanges commit -m \"...\"`.")


@app.command()
def status(
    json: bool = typer.Option(False, "--json", help="Machine-readable output."),
) -> None:
    """Show every agent, the files it changed, and any cross-agent overlaps."""
    base = mychanges_dir(Path.cwd())
    vcs = vcsmod.detect(Path.cwd())
    with _open_store(base) as store:
        agents = store.agents()
        overlaps = store.path_agents()
        per_agent = {}
        for a in agents:
            paths, _ = _mine_paths(store, a, vcs)
            per_agent[a] = paths
    shared = {p: sorted(ags) for p, ags in overlaps.items() if len(ags) > 1}

    if json:
        typer.echo(_json.dumps({"vcs": vcs.kind if vcs else None, "agents": per_agent, "overlaps": shared}, indent=2))
        return

    if not agents:
        typer.echo("No attributed changes yet.")
        return
    typer.echo(f"VCS: {vcs.kind if vcs else 'none'}\n")
    for a, paths in per_agent.items():
        typer.secho(f"{a}", fg=typer.colors.CYAN, bold=True)
        if not paths:
            typer.echo("  (no live changes)")
        for p in paths:
            mark = "  ⚠ also: " + ", ".join(x for x in shared.get(p, []) if x != a) if p in shared else ""
            typer.echo(f"  {p}{mark}")
        typer.echo("")
    if shared:
        typer.secho(f"{len(shared)} file(s) touched by multiple agents — last writer's content wins on commit.", fg=typer.colors.YELLOW)


@app.command()
def mine(
    agent: str | None = typer.Option(None, "--agent", help=f"Agent id (default: ${ENV_VAR})."),
    json: bool = typer.Option(False, "--json", help="Machine-readable output."),
    paths_only: bool = typer.Option(False, "--paths", help="Print just the paths, one per line."),
) -> None:
    """List the files attributed to me that are still changed in the working copy."""
    who = _resolve_agent(agent)
    base = mychanges_dir(Path.cwd())
    vcs = vcsmod.detect(Path.cwd())
    with _open_store(base) as store:
        mine_paths, overlaps = _mine_paths(store, who, vcs)

    if paths_only:
        typer.echo("\n".join(mine_paths))
        return
    if json:
        shared = {p: sorted(overlaps.get(p, set())) for p in mine_paths if len(overlaps.get(p, set())) > 1}
        typer.echo(_json.dumps({"agent": who, "vcs": vcs.kind if vcs else None, "paths": mine_paths, "overlaps": shared}, indent=2))
        return
    typer.secho(f"agent {who} — {len(mine_paths)} file(s):", bold=True)
    for p in mine_paths:
        others = sorted(x for x in overlaps.get(p, set()) if x != who)
        mark = "  ⚠ also: " + ", ".join(others) if others else ""
        typer.echo(f"  {p}{mark}")


@app.command()
def commit(
    message: str = typer.Option(..., "-m", "--message", help="Commit / change description."),
    agent: str | None = typer.Option(None, "--agent", help=f"Agent id (default: ${ENV_VAR})."),
    dry_run: bool = typer.Option(False, "--dry-run", help="Print the commands instead of running."),
    keep: bool = typer.Option(False, "--keep", help="Keep attribution records after committing."),
) -> None:
    """Harvest my changes into a separate commit (jj split / git refs/mychanges/<agent>)."""
    who = _resolve_agent(agent)
    base = mychanges_dir(Path.cwd())
    vcs = vcsmod.detect(Path.cwd())
    if vcs is None:
        _err("No jj or git repo found here.")
        raise typer.Exit(2)

    with _open_store(base) as store:
        mine_paths, overlaps = _mine_paths(store, who, vcs)
    if not mine_paths:
        typer.echo(f"Nothing to commit for agent {who}.")
        return

    shared = [p for p in mine_paths if len(overlaps.get(p, set())) > 1]
    if shared:
        typer.secho(f"⚠ {len(shared)} file(s) also touched by other agents; committing current on-disk content:", fg=typer.colors.YELLOW)
        for p in shared:
            typer.echo(f"    {p}")

    if dry_run:
        for line in vcsmod.dry_run_plan(vcs, mine_paths, message, who):
            typer.echo(line)
        return

    with _commit_lock(base):
        # Re-evaluate under the lock: another agent may have committed since.
        with _open_store(base) as store:
            mine_paths, _ = _mine_paths(store, who, vcs)
        if not mine_paths:
            typer.echo(f"Nothing to commit for agent {who}.")
            return
        result = vcsmod.commit_mine(vcs, mine_paths, message, who)

    if not result.get("ok"):
        _err(f"commit failed: {result.get('error')}")
        raise typer.Exit(1)

    if result["kind"] == "jj":
        typer.secho(f"✓ jj commit {result['change_id']} ({result['commit']}) — {len(mine_paths)} file(s)", fg=typer.colors.GREEN)
    else:
        typer.secho(f"✓ git commit {result['commit'][:12]} → {result['ref']} — {len(mine_paths)} file(s)", fg=typer.colors.GREEN)
        typer.echo(f"  inspect: git show {result['commit'][:12]}")

    if not keep:
        with _open_store(base) as store:
            store.delete_paths(who, mine_paths)


@app.command()
def reset(
    agent: str | None = typer.Option(None, "--agent", help="Clear just this agent."),
    all: bool = typer.Option(False, "--all", help="Clear all attribution records."),
) -> None:
    """Forget attribution records (does not touch your files or commits)."""
    base = mychanges_dir(Path.cwd())
    if not agent and not all:
        _err("Specify --agent <id> or --all.")
        raise typer.Exit(2)
    with _open_store(base) as store:
        store.clear(None if all else agent)
    typer.secho("cleared.", fg=typer.colors.GREEN)


# --------------------------------------------------------------------------- #
def _default_hook_command() -> str:
    import shutil

    exe = shutil.which("mychanges")
    if exe:
        return f'"{exe}" hook'
    return f'"{sys.executable}" -m commit_mychanges hook'


def _merge_settings(settings_path: Path, command: str) -> bool:
    """Idempotently add our SessionStart / PreToolUse / PostToolUse hooks.

    Returns True if anything was added.
    """
    data: dict = {}
    if settings_path.exists():
        try:
            data = _json.loads(settings_path.read_text())
        except _json.JSONDecodeError:
            data = {}
    hooks = data.setdefault("hooks", {})
    changed = False

    def ensure(event: str, matcher: str | None) -> None:
        nonlocal changed
        groups = hooks.setdefault(event, [])
        for grp in groups:
            if grp.get("matcher", None) == matcher or (matcher is None and "matcher" not in grp):
                entry = grp.setdefault("hooks", [])
                if any(h.get("command") == command for h in entry):
                    return
                entry.append({"type": "command", "command": command})
                changed = True
                return
        grp = {"hooks": [{"type": "command", "command": command}]}
        if matcher is not None:
            grp = {"matcher": matcher, **grp}
        groups.append(grp)
        changed = True

    ensure("SessionStart", None)
    ensure("PreToolUse", "Bash")
    ensure("PostToolUse", "Edit|Write|MultiEdit|Bash")

    if changed:
        settings_path.parent.mkdir(parents=True, exist_ok=True)
        settings_path.write_text(_json.dumps(data, indent=2) + "\n")
    return changed
