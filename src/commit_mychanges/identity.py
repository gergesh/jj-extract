from __future__ import annotations

import os

ENV_VAR = "MYCHANGES_AGENT"


def from_payload(payload: dict) -> str:
    """Resolve the acting agent's id from a hook payload.

    An explicit ``MYCHANGES_AGENT`` (set per-process for named agents) wins;
    otherwise we fall back to the session id, which is distinct per top-level
    ``claude`` process.
    """
    return os.environ.get(ENV_VAR) or payload.get("session_id") or "unknown"


def from_cli(explicit: str | None) -> str | None:
    """Resolve "me" for a CLI invocation: ``--agent`` flag, else the env var.

    The env var is normally populated by the SessionStart hook, which writes
    ``MYCHANGES_AGENT=<session_id>`` into ``$CLAUDE_ENV_FILE`` so it reaches the
    agent's shell subprocesses.
    """
    return explicit or os.environ.get(ENV_VAR)
