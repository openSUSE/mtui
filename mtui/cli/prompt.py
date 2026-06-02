"""Backwards-compatible re-export shim for the interactive REPL.

The implementation lives in :mod:`mtui.cli.repl` (formerly here as a
:class:`cmd.Cmd` subclass). External callers — notably ``main.py`` and
the test suite — keep importing the same names from this module.
"""

from .repl import CmdQueue, CommandAlreadyBoundError, CommandPrompt, QuitLoopError

__all__ = [
    "CmdQueue",
    "CommandAlreadyBoundError",
    "CommandPrompt",
    "QuitLoopError",
]
