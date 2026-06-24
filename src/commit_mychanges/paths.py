from __future__ import annotations

from pathlib import Path

MYCHANGES_DIRNAME = ".mychanges"


def find_repo_root(start: Path) -> Path | None:
    """Nearest ancestor (inclusive) containing a ``.jj`` or ``.git`` entry."""
    start = start.resolve()
    for d in (start, *start.parents):
        if (d / ".jj").exists() or (d / ".git").exists():
            return d
    return None


def base_dir(start: Path) -> Path:
    """Directory that anchors ``.mychanges`` — the repo root, or ``start``."""
    return find_repo_root(start) or start.resolve()


def mychanges_dir(start: Path) -> Path:
    return base_dir(start) / MYCHANGES_DIRNAME


def relpath_within(p: str | Path, root: Path) -> str | None:
    """Return ``p`` as a POSIX path relative to ``root``, or None if outside it.

    Resolves ``..``/symlinks defensively; absolute and relative inputs both work.
    """
    try:
        rp = Path(p)
        if not rp.is_absolute():
            rp = root / rp
        return rp.resolve().relative_to(root.resolve()).as_posix()
    except (ValueError, OSError):
        return None
