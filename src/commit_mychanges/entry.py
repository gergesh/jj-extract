import sys


def main() -> None:
    """Console-script entry point.

    The ``hook`` subcommand is on the hot path (it runs on every Edit/Write/Bash
    tool call), so it bypasses Typer entirely to keep import cost minimal and to
    guarantee it never raises into the caller.
    """
    if len(sys.argv) >= 2 and sys.argv[1] == "hook":
        from .hookrunner import run_hook

        raise SystemExit(run_hook(sys.argv[2:]))

    from .cli import app

    app()
