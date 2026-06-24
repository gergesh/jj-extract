"""Bash attribution: best-effort before/after working-tree snapshot diff.

Optional and off by default. Within a single agent's session, tool calls are
sequential, so the PreToolUse snapshot and PostToolUse diff for a given Bash
call are well-ordered. Across concurrent agents the windows overlap, so this is
best-effort at file granularity (documented in the README).
"""

from __future__ import annotations

import json
import os
from fnmatch import fnmatch
from pathlib import Path

from .config import Config
from .store import Store

# Directories pruned by name during the walk (perf + obvious noise).
_PRUNE_DIRS = {
    ".git",
    ".jj",
    ".mychanges",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "target",
    "dist",
    "build",
    ".next",
    ".cache",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".idea",
    ".gradle",
}


def _ignored(rel: str, ignore: list[str]) -> bool:
    base = rel.rsplit("/", 1)[-1]
    return any(fnmatch(rel, g) or fnmatch(base, g) for g in ignore)


def _manifest(root: Path, ignore: list[str]) -> dict[str, list[int]]:
    """Map repo-relative path -> [mtime_ns, size] for files under ``root``."""
    out: dict[str, list[int]] = {}
    root = root.resolve()
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in _PRUNE_DIRS]
        for fn in filenames:
            full = Path(dirpath) / fn
            try:
                rel = full.relative_to(root).as_posix()
            except ValueError:
                continue
            if _ignored(rel, ignore):
                continue
            try:
                st = full.stat()
            except OSError:
                continue
            out[rel] = [st.st_mtime_ns, st.st_size]
    return out


def _pre_path(base: Path, agent: str) -> Path:
    safe = "".join(c if (c.isalnum() or c in "-_.") else "_" for c in agent) or "agent"
    return base / "pre" / f"{safe}.json"


def snapshot_pre(base: Path, root: Path, agent: str, cfg: Config) -> None:
    p = _pre_path(base, agent)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(_manifest(root, cfg.ignore)))


def diff_post(base: Path, root: Path, agent: str, session_id: str | None, cfg: Config) -> None:
    p = _pre_path(base, agent)
    if not p.exists():
        return
    try:
        pre = json.loads(p.read_text())
    except (OSError, json.JSONDecodeError):
        pre = {}
    post = _manifest(root, cfg.ignore)

    with Store(base / "attribution.db") as store:
        for rel, meta in post.items():
            old = pre.get(rel)
            if old is None:
                store.record(agent, session_id, "Bash", "PostToolUse", rel, "create")
            elif old != meta:
                store.record(agent, session_id, "Bash", "PostToolUse", rel, "modify")
        for rel in pre:
            if rel not in post:
                store.record(agent, session_id, "Bash", "PostToolUse", rel, "delete")
    try:
        p.unlink()
    except OSError:
        pass
