from __future__ import annotations

import sqlite3
import time
from pathlib import Path

_SCHEMA = """
CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts         REAL NOT NULL,
    agent      TEXT NOT NULL,
    session_id TEXT,
    tool       TEXT NOT NULL,
    event      TEXT NOT NULL,
    path       TEXT NOT NULL,
    change     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_agent ON events(agent);
CREATE INDEX IF NOT EXISTS idx_events_path  ON events(path);
"""


class Store:
    """SQLite-backed attribution log.

    WAL mode + a busy timeout let concurrent agents' hooks write safely without
    an external lock.
    """

    def __init__(self, db_path: Path):
        db_path.parent.mkdir(parents=True, exist_ok=True)
        self.conn = sqlite3.connect(str(db_path), timeout=15)
        self.conn.execute("PRAGMA journal_mode=WAL;")
        self.conn.execute("PRAGMA busy_timeout=15000;")
        self.conn.executescript(_SCHEMA)
        self.conn.commit()

    def record(
        self,
        agent: str,
        session_id: str | None,
        tool: str,
        event: str,
        path: str,
        change: str,
        ts: float | None = None,
    ) -> None:
        self.conn.execute(
            "INSERT INTO events (ts, agent, session_id, tool, event, path, change)"
            " VALUES (?, ?, ?, ?, ?, ?, ?)",
            (time.time() if ts is None else ts, agent, session_id, tool, event, path, change),
        )
        self.conn.commit()

    def agents(self) -> list[str]:
        return [r[0] for r in self.conn.execute("SELECT DISTINCT agent FROM events ORDER BY agent")]

    def paths_for(self, agent: str) -> list[str]:
        return [
            r[0]
            for r in self.conn.execute(
                "SELECT DISTINCT path FROM events WHERE agent = ? ORDER BY path", (agent,)
            )
        ]

    def path_agents(self) -> dict[str, set[str]]:
        """Map each path to the set of agents that touched it (overlap detection)."""
        out: dict[str, set[str]] = {}
        for path, agent in self.conn.execute("SELECT DISTINCT path, agent FROM events"):
            out.setdefault(path, set()).add(agent)
        return out

    def delete_paths(self, agent: str, paths: list[str]) -> None:
        self.conn.executemany(
            "DELETE FROM events WHERE agent = ? AND path = ?", [(agent, p) for p in paths]
        )
        self.conn.commit()

    def clear(self, agent: str | None = None) -> None:
        if agent is None:
            self.conn.execute("DELETE FROM events")
        else:
            self.conn.execute("DELETE FROM events WHERE agent = ?", (agent,))
        self.conn.commit()

    def close(self) -> None:
        self.conn.close()

    def __enter__(self) -> "Store":
        return self

    def __exit__(self, *exc) -> None:
        self.close()
