"""Per-agent change attribution for a shared working directory.

Multiple agents edit one working copy; a Claude Code hook records which agent
touched which file; each agent can then harvest *its* changes into a separate
commit (``jj split`` / a git ``refs/mychanges/<agent>`` commit).
"""

__version__ = "0.1.0"
