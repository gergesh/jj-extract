from __future__ import annotations

import tomllib
from dataclasses import dataclass, field
from pathlib import Path

# File-level ignore globs for Bash snapshot-diff (matched against the relative
# POSIX path and the basename). Heavy/irrelevant directories are also pruned by
# name in scan.py for speed.
DEFAULT_IGNORE: list[str] = [
    "**/.git/**",
    "**/.jj/**",
    "**/.mychanges/**",
    "**/node_modules/**",
    "**/.venv/**",
    "**/__pycache__/**",
    "**/*.pyc",
    "**/target/**",
    "**/dist/**",
    "**/build/**",
    "**/.next/**",
]


@dataclass
class Config:
    # Attribute files changed by Bash commands via before/after snapshot-diff.
    # Off by default: it walks the working tree on every Bash call.
    bash: bool = False
    ignore: list[str] = field(default_factory=lambda: list(DEFAULT_IGNORE))


def load_config(mychanges_dir: Path) -> Config:
    path = mychanges_dir / "config.toml"
    if not path.exists():
        return Config()
    try:
        data = tomllib.loads(path.read_text())
    except (OSError, tomllib.TOMLDecodeError):
        return Config()
    attr = data.get("attribution", {}) or {}
    ignore = attr.get("ignore")
    ignore_list = [str(x) for x in ignore] if isinstance(ignore, list) else list(DEFAULT_IGNORE)
    return Config(bash=bool(attr.get("bash", False)), ignore=ignore_list)


CONFIG_TEMPLATE = """\
# commit-mychanges configuration

[attribution]
# Attribute files changed by Bash commands (e.g. `sed -i`, codegen, `mv`).
# This snapshots the working tree before and after every Bash tool call, so it
# costs a directory walk per Bash command. Edit/Write/MultiEdit are always
# attributed regardless of this setting.
bash = {bash}

# Globs ignored when scanning for Bash-induced changes (matched against the
# path relative to the repo root and against the basename).
ignore = [
{ignore}
]
"""


def render_config(*, bash: bool, ignore: list[str]) -> str:
    body = ",\n".join(f'  "{g}"' for g in ignore)
    return CONFIG_TEMPLATE.format(bash="true" if bash else "false", ignore=body)
