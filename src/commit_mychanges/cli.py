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
    force: bool = typer.Option(False, "--force", help="Overwrite an existing config.toml."),
) -> None:
    """Opt this repo in: create the .mychanges/ marker the hook gates on.

    Hooks are registered once, separately, with `mychanges install` (globally or
    per-project). This command only creates the per-repo opt-in marker + config.
    """
    root = base_dir(Path.cwd())
    base = mychanges_dir(Path.cwd())
    base.mkdir(parents=True, exist_ok=True)

    cfg_path = base / "config.toml"
    if force or not cfg_path.exists():
        cfg_path.write_text(render_config(bash=bash, ignore=load_config(base).ignore))
    Store(base / "attribution.db").close()  # create the db

    gitignore = base / ".gitignore"
    if not gitignore.exists():
        gitignore.write_text("# transient attribution state\nattribution.db*\npre/\nhook-error.log\ncommit.lock\n")

    typer.secho(f"Initialised {base}", fg=typer.colors.GREEN)
    typer.echo(f"  config:   {cfg_path}  (bash attribution: {'on' if bash else 'off'})")

    targets = _installed_targets(root)
    if targets:
        typer.echo(f"  hooks:    active ({', '.join(label for label, _ in targets)})")
        typer.echo("\nRestart agents here so the SessionStart hook stamps their id,")
        typer.echo("then each runs `mychanges mine` / `mychanges commit -m \"...\"`.")
    else:
        typer.secho("  hooks:    not registered yet", fg=typer.colors.YELLOW)
        typer.echo("\nRegister them once (every repo; inert without .mychanges/):")
        typer.secho("    mychanges install", bold=True)
        typer.echo("…or just this repo:     mychanges install --project")


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


GLOBAL_SETTINGS = Path.home() / ".claude" / "settings.json"


@app.command()
def install(
    global_: bool = typer.Option(
        True,
        "--global/--project",
        help="Write ~/.claude/settings.json (every repo) or ./.claude/settings.json (this repo).",
    ),
    gate: bool | None = typer.Option(
        None,
        "--gate/--no-gate",
        help="Spawn the hook only when CLAUDE_PROJECT_DIR/.mychanges exists (default: on for --global).",
    ),
    settings: Path | None = typer.Option(None, "--settings", help="Explicit settings.json to patch."),
    command: str | None = typer.Option(None, "--command", help="Override the base hook command."),
) -> None:
    """Register the attribution hooks in Claude's settings (run once)."""
    if gate is None:
        gate = global_
    target = settings or (
        GLOBAL_SETTINGS if global_ else base_dir(Path.cwd()) / ".claude" / "settings.json"
    )
    full = _hook_command(command or _default_hook_command(), gate)
    added = _merge_settings(target, full)
    scope = "custom" if settings else ("global" if global_ else "project")
    verb = "Installed" if added else "Already present —"
    typer.secho(f"{verb} hooks ({scope}) → {target}", fg=typer.colors.GREEN)
    typer.echo(f"  command:  {full}")
    if gate:
        typer.echo("  gated:    fires only in repos with a .mychanges/ dir (`mychanges init` to opt in)")
    else:
        typer.echo("  ungated:  fires in every session; opt repos in with `mychanges init`")


@app.command()
def uninstall(
    global_: bool = typer.Option(True, "--global/--project", help="Which settings file to clean."),
    settings: Path | None = typer.Option(None, "--settings", help="Explicit settings.json to clean."),
) -> None:
    """Remove the attribution hooks from Claude's settings (leaves other hooks intact)."""
    target = settings or (
        GLOBAL_SETTINGS if global_ else base_dir(Path.cwd()) / ".claude" / "settings.json"
    )
    n = _remove_settings(target)
    typer.secho(
        f"Removed {n} hook entr{'y' if n == 1 else 'ies'} from {target}", fg=typer.colors.GREEN
    )


# --------------------------------------------------------------------------- #
def _default_hook_command() -> str:
    import shutil

    exe = shutil.which("mychanges")
    if exe:
        return f'"{exe}" hook'
    return f'"{sys.executable}" -m commit_mychanges hook'


def _hook_command(base_cmd: str, gate: bool) -> str:
    """Optionally wrap the command in a shell gate so it only spawns Python in
    repos that opted in — keeping global registration near-zero cost elsewhere.
    The gate assumes the marker sits at CLAUDE_PROJECT_DIR; use --no-gate if you
    launch Claude from a subdirectory of the repo."""
    if not gate:
        return base_cmd
    return f'if [ -d "${{CLAUDE_PROJECT_DIR:-$PWD}}/.mychanges" ]; then exec {base_cmd}; fi'


def _is_our_hook(command: str) -> bool:
    return "hook" in command and ("commit_mychanges" in command or "mychanges" in command)


def _settings_has_our_hook(path: Path) -> bool:
    try:
        data = _json.loads(path.read_text())
    except (OSError, _json.JSONDecodeError):
        return False
    for groups in (data.get("hooks") or {}).values():
        for grp in groups:
            if any(_is_our_hook(h.get("command", "")) for h in grp.get("hooks", [])):
                return True
    return False


def _installed_targets(project_root: Path) -> list[tuple[str, Path]]:
    candidates = [
        ("global", GLOBAL_SETTINGS),
        ("project", project_root / ".claude" / "settings.json"),
        ("project-local", project_root / ".claude" / "settings.local.json"),
    ]
    return [(label, p) for label, p in candidates if _settings_has_our_hook(p)]


def _backup(path: Path) -> None:
    if path.exists():
        path.with_suffix(path.suffix + ".mychanges-bak").write_text(path.read_text())


def _merge_settings(settings_path: Path, command: str) -> bool:
    """Idempotently add our SessionStart / PreToolUse / PostToolUse hooks,
    preserving every other key and any pre-existing hooks. Returns True if
    anything was added."""
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
            same = grp.get("matcher", None) == matcher or (matcher is None and "matcher" not in grp)
            if same:
                entry = grp.setdefault("hooks", [])
                if any(_is_our_hook(h.get("command", "")) for h in entry):
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
        _backup(settings_path)
        settings_path.write_text(_json.dumps(data, indent=2) + "\n")
    return changed


def _remove_settings(settings_path: Path) -> int:
    if not settings_path.exists():
        return 0
    try:
        data = _json.loads(settings_path.read_text())
    except _json.JSONDecodeError:
        return 0
    hooks = data.get("hooks") or {}
    removed = 0
    for event in list(hooks):
        groups = hooks[event]
        for grp in list(groups):
            entry = grp.get("hooks", [])
            kept = [h for h in entry if not _is_our_hook(h.get("command", ""))]
            removed += len(entry) - len(kept)
            grp["hooks"] = kept
            if not kept:
                groups.remove(grp)
        if not groups:
            del hooks[event]
    if removed:
        _backup(settings_path)
        settings_path.write_text(_json.dumps(data, indent=2) + "\n")
    return removed
