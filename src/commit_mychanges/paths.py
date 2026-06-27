from __future__ import annotations

import hashlib
import os
from pathlib import Path


def find_repo_root(start: Path) -> Path | None:
    """Nearest ancestor (inclusive) containing a ``.jj`` or ``.git`` entry."""
    start = start.resolve()
    for d in (start, *start.parents):
        if (d / ".jj").exists() or (d / ".git").exists():
            return d
    return None


def base_dir(start: Path) -> Path:
    """Repo root, or ``start`` — used to anchor a repo's ``.claude/`` settings."""
    return find_repo_root(start) or start.resolve()


# --------------------------------------------------------------------------- #
# Central storage: attribution lives under ~/.claude/mychanges/<repo>-<hash>/  #
# rather than in a .mychanges/ dir inside every repo, so recording can be on   #
# everywhere without littering working trees.                                  #
# --------------------------------------------------------------------------- #
def central_root() -> Path:
    override = os.environ.get("MYCHANGES_HOME")
    if override:
        return Path(override).expanduser()
    return Path.home() / ".claude" / "mychanges"


def _slug(name: str) -> str:
    s = "".join(c if (c.isalnum() or c in "-_.") else "-" for c in name).strip("-")
    return s or "repo"


def data_dir_for_root(root: Path) -> Path:
    """Deterministic central data dir for a repo root (same input → same dir)."""
    root = root.resolve()
    digest = hashlib.sha1(str(root).encode("utf-8")).hexdigest()[:10]
    return central_root() / f"{_slug(root.name)}-{digest}"


def resolve_data_dir(start: Path) -> tuple[Path | None, Path | None]:
    """(repo_root, data_dir) for the repo containing ``start``, or (None, None)."""
    root = find_repo_root(start)
    if root is None:
        return None, None
    return root, data_dir_for_root(root)


def ensure_data_dir(root: Path) -> Path:
    """Create the repo's central data dir (recording a ``repo`` back-pointer)."""
    base = data_dir_for_root(root)
    base.mkdir(parents=True, exist_ok=True)
    marker = base / "repo"
    if not marker.exists():
        try:
            marker.write_text(str(root.resolve()) + "\n")
        except OSError:
            pass
    return base


def relpath_within(p: str | Path, root: Path) -> str | None:
    """Return ``p`` as a POSIX path relative to ``root``, or None if outside it."""
    try:
        rp = Path(p)
        if not rp.is_absolute():
            rp = root / rp
        return rp.resolve().relative_to(root.resolve()).as_posix()
    except (ValueError, OSError):
        return None
