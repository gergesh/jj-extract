from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass
from pathlib import Path


@dataclass
class Vcs:
    kind: str  # "jj" or "git"
    root: Path


def detect(start: Path) -> Vcs | None:
    """Detect the VCS at or above ``start``. jj wins over git (colocated repos
    have both, but we want jj's semantics)."""
    start = start.resolve()
    for d in (start, *start.parents):
        if (d / ".jj").exists():
            return Vcs("jj", d)
        if (d / ".git").exists():
            return Vcs("git", d)
    return None


def _git_env() -> dict[str, str]:
    # The user runs a `git` shim that refuses to run inside jj-managed repos;
    # JJ_GIT_OK=1 opts our own deliberate git calls past that guard.
    env = dict(os.environ)
    env["JJ_GIT_OK"] = "1"
    return env


def _run(args: list[str], cwd: Path, env: dict | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(args, cwd=str(cwd), capture_output=True, text=True, env=env)


def _safe(name: str) -> str:
    """Sanitise an agent id for use in a filename / git refname component."""
    return "".join(c if (c.isalnum() or c in "-_.") else "-" for c in name) or "agent"


# --------------------------------------------------------------------------- #
# Working-copy change detection: path -> single-letter status                 #
# --------------------------------------------------------------------------- #
def changed_paths(vcs: Vcs) -> dict[str, str]:
    return _jj_changed(vcs.root) if vcs.kind == "jj" else _git_changed(vcs.root)


def _jj_changed(root: Path) -> dict[str, str]:
    """Parse ``jj status``. Lines look like ``M path`` / ``A path`` / ``? path``
    (the last being untracked, which matters because auto-track may be off)."""
    out: dict[str, str] = {}
    res = _run(["jj", "status"], root)
    for line in res.stdout.splitlines():
        if len(line) >= 3 and line[1] == " " and line[0] in "MADCR?":
            path = line[2:].strip()
            if path:
                out[path] = line[0]
    return out


def _git_changed(root: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    res = _run(
        ["git", "status", "--porcelain=v1", "--no-renames", "-z"], root, env=_git_env()
    )
    for entry in res.stdout.split("\0"):
        if len(entry) >= 4:  # "XY path"
            out[entry[3:]] = entry[:2].strip() or entry[:2]
    return out


# --------------------------------------------------------------------------- #
# Harvest one agent's paths into a separate commit                            #
# --------------------------------------------------------------------------- #
def commit_mine(vcs: Vcs, paths: list[str], message: str, agent: str) -> dict:
    if vcs.kind == "jj":
        return _jj_commit(vcs.root, paths, message)
    return _git_commit(vcs.root, paths, message, agent)


def dry_run_plan(vcs: Vcs, paths: list[str], message: str, agent: str) -> list[str]:
    """The exact commands ``commit_mine`` would run, for ``--dry-run``."""
    q = " ".join(_q(p) for p in paths)
    if vcs.kind == "jj":
        existing = " ".join(_q(p) for p in paths)
        return [f"jj file track {existing}", f"jj split -m {_q(message)} {q}"]
    safe = _safe(agent)
    idx = vcs.root / ".git" / f"mychanges-{safe}.index"
    return [
        f"GIT_INDEX_FILE={idx} git read-tree HEAD",
        f"GIT_INDEX_FILE={idx} git add -- {q}",
        f'commit=$(GIT_INDEX_FILE={idx} git write-tree | xargs -I% git commit-tree % -p HEAD -m {_q(message)})',
        f"git update-ref refs/mychanges/{safe} $commit",
    ]


def _jj_commit(root: Path, paths: list[str], message: str) -> dict:
    # New files are untracked when snapshot.auto-track is off; track this
    # agent's existing paths so split can see them. Other agents' untracked
    # files stay untracked and out of this commit.
    to_track = [p for p in paths if (root / p).exists()]
    if to_track:
        _run(["jj", "file", "track", *to_track], root)
    res = _run(["jj", "split", "-m", message, *paths], root)
    if res.returncode != 0:
        return {"ok": False, "error": res.stderr.strip() or res.stdout.strip(), "paths": paths}
    commit = _run(
        ["jj", "log", "--no-graph", "-r", "@-", "-T", 'commit_id.short(12)'], root
    ).stdout.strip()
    change = _run(
        ["jj", "log", "--no-graph", "-r", "@-", "-T", 'change_id.short(8)'], root
    ).stdout.strip()
    return {"ok": True, "kind": "jj", "commit": commit, "change_id": change, "paths": paths}


def _git_commit(root: Path, paths: list[str], message: str, agent: str) -> dict:
    safe = _safe(agent)
    idx = root / ".git" / f"mychanges-{safe}.index"
    env = _git_env()
    env["GIT_INDEX_FILE"] = str(idx)

    parent: list[str] = []
    if _run(["git", "rev-parse", "--verify", "-q", "HEAD"], root, env=_git_env()).returncode == 0:
        head = _run(["git", "rev-parse", "HEAD"], root, env=_git_env()).stdout.strip()
        _run(["git", "read-tree", "HEAD"], root, env=env)
        parent = ["-p", head]
    else:  # unborn branch (no commits yet)
        _run(["git", "read-tree", "--empty"], root, env=env)

    add = _run(["git", "add", "--", *paths], root, env=env)
    if add.returncode != 0:
        return {"ok": False, "error": add.stderr.strip(), "paths": paths}
    tree = _run(["git", "write-tree"], root, env=env).stdout.strip()
    made = _run(["git", "commit-tree", tree, *parent, "-m", message], root, env=_git_env())
    if made.returncode != 0:
        return {"ok": False, "error": made.stderr.strip(), "paths": paths}
    commit = made.stdout.strip()
    ref = f"refs/mychanges/{safe}"
    _run(["git", "update-ref", ref, commit], root, env=_git_env())
    return {"ok": True, "kind": "git", "commit": commit, "ref": ref, "paths": paths}


def _q(s: str) -> str:
    import shlex

    return shlex.quote(s)
