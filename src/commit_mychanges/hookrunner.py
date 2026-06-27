"""Hook entry point. Reads the hook JSON on stdin and records attribution.

Contract: this must NEVER interfere with the tool call. It always exits 0,
emits nothing on stdout, and swallows every error (logged best-effort to
``.mychanges/hook-error.log``).
"""

from __future__ import annotations

import json
import os
import sys
import traceback
from pathlib import Path

from .identity import ENV_VAR
from .paths import central_root, data_dir_for_root, ensure_data_dir, find_repo_root

_FILE_TOOLS = ("Edit", "Write", "MultiEdit")


def run_hook(_argv: list[str]) -> int:
    try:
        raw = sys.stdin.read()
        payload = json.loads(raw) if raw.strip() else {}
    except Exception:
        return 0
    try:
        _dispatch(payload)
    except Exception:
        _log_error(payload, traceback.format_exc())
    return 0


def _dispatch(payload: dict) -> None:
    event = payload.get("hook_event_name")
    cwd = Path(payload.get("cwd") or os.getcwd())

    if event == "SessionStart":
        _session_start(payload)
        return

    # Record automatically in any git/jj repo; nothing to attribute outside one.
    root = find_repo_root(cwd)
    if root is None:
        return

    from .config import load_config
    from .identity import from_payload

    cfg = load_config(data_dir_for_root(root))
    agent = from_payload(payload)
    session_id = payload.get("session_id")
    tool = payload.get("tool_name")

    if event == "PreToolUse" and tool == "Bash" and cfg.bash:
        from .scan import snapshot_pre

        snapshot_pre(ensure_data_dir(root), root, agent, cfg)
        return

    if event == "PostToolUse":
        if tool in _FILE_TOOLS:
            _record_file_edits(payload, ensure_data_dir(root), root, agent, session_id, tool)
        elif tool == "Bash" and cfg.bash:
            from .scan import diff_post

            diff_post(ensure_data_dir(root), root, agent, session_id, cfg)


def _session_start(payload: dict) -> None:
    """Stamp ``MYCHANGES_AGENT=<session_id>`` into ``$CLAUDE_ENV_FILE`` so the
    agent's later CLI invocations know their own identity. Respect an
    already-set value (a deliberately named agent)."""
    if os.environ.get(ENV_VAR):
        return
    env_file = os.environ.get("CLAUDE_ENV_FILE")
    sid = payload.get("session_id")
    if not env_file or not sid:
        return
    try:
        with open(env_file, "a") as f:
            f.write(f"{ENV_VAR}={sid}\n")
    except OSError:
        pass


def _record_file_edits(
    payload: dict, base: Path, root: Path, agent: str, session_id: str | None, tool: str
) -> None:
    from .paths import relpath_within
    from .store import Store

    ti = payload.get("tool_input") or {}
    raw_paths: list[str] = []
    if ti.get("file_path"):
        raw_paths.append(ti["file_path"])
    for edit in ti.get("edits") or []:  # MultiEdit may carry per-edit file_path
        if isinstance(edit, dict) and edit.get("file_path"):
            raw_paths.append(edit["file_path"])

    rels: list[str] = []
    for p in raw_paths:
        rel = relpath_within(p, root)
        if rel:
            rels.append(rel)
    if not rels:
        return

    change = "create" if tool == "Write" else "modify"
    with Store(base / "attribution.db") as store:
        for rel in dict.fromkeys(rels):  # de-dupe, preserve order
            store.record(agent, session_id, tool, "PostToolUse", rel, change)


def _log_error(_payload: dict, message: str) -> None:
    try:
        croot = central_root()
        croot.mkdir(parents=True, exist_ok=True)
        with open(croot / "hook-error.log", "a") as f:
            f.write(message + "\n")
    except Exception:
        pass
