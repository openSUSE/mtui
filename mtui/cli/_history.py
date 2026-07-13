"""Shared `prompt_toolkit` history backend.

Centralises the on-disk REPL history file so the new REPL
(:mod:`mtui.cli.repl`), the confirmation prompt
(:func:`mtui.cli.term.prompt_user`), and the ``quit`` command all write
through a single :class:`~prompt_toolkit.history.FileHistory` instance per
path. Two writers racing the same file would corrupt
``prompt_toolkit``'s record framing.

Public surface:

* :func:`default_history_path` — the canonical ``~/.mtui_history`` location.
* :func:`get_history` — memoised :class:`FileHistory` accessor keyed on the
  resolved absolute path.
"""

from __future__ import annotations

import threading
from pathlib import Path

from prompt_toolkit.history import FileHistory

_cache: dict[Path, FileHistory] = {}
_cache_lock = threading.Lock()


def default_history_path() -> Path:
    """Return the canonical mtui history file location.

    Single source of truth for the path so callers do not duplicate the
    literal ``~/.mtui_history`` string.
    """
    return Path("~").expanduser() / ".mtui_history"


def get_history(path: Path) -> FileHistory:
    """Return a memoised :class:`FileHistory` for ``path``.

    Repeated calls with the same resolved path return the same instance, so
    every writer in the process appends to one in-memory deque and one file
    handle pattern. Distinct paths (used by tests) get distinct instances.

    Args:
        path: history file location. Need not exist yet; ``FileHistory``
            creates it on first ``append_string``.

    Returns:
        The shared :class:`FileHistory` for that path.

    """
    key = Path(path).expanduser().resolve(strict=False)
    with _cache_lock:
        hist = _cache.get(key)
        if hist is None:
            hist = FileHistory(str(key))
            _cache[key] = hist
        return hist
