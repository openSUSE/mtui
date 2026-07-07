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
* :func:`pop_last_entry` — drop the most recent entry from both the
  in-memory cache and the on-disk file. A standalone utility with no
  current caller: :func:`mtui.cli.term.prompt_user` used to call it to
  scrub a yes/no answer, but the answer is now read through an ephemeral
  in-memory history and never reaches this shared file, so the pop was
  removed.
"""

from __future__ import annotations

import logging
import threading
from pathlib import Path

from prompt_toolkit.history import FileHistory

logger = logging.getLogger(__name__)

# Record separator written by ``FileHistory.store_string``: every entry is
# prefixed with ``\n# <timestamp>\n`` followed by one or more ``+<line>``
# lines. Finding the LAST occurrence of this separator tells us where the
# tail entry starts, so we can truncate in place without re-parsing.
_RECORD_SEP = b"\n# "

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


def pop_last_entry(path: Path) -> str | None:
    """Remove and return the most recent history entry at ``path``.

    Mirrors the legacy ``readline.remove_history_item`` behaviour. It once
    scrubbed a yes/no answer typed at :func:`mtui.cli.term.prompt_user`, but
    that caller now reads through an ephemeral in-memory history and no longer
    needs (or calls) this; it is retained as a standalone utility.

    Both the on-disk file and any in-memory deque held by a cached
    :class:`FileHistory` for the same path are updated, so the next prompt
    does not resurrect the dropped entry.

    Args:
        path: history file location.

    Returns:
        The popped entry's string, or ``None`` when there was nothing to
        pop (missing file, empty file, or no parseable record).

    """
    key = Path(path).expanduser().resolve(strict=False)

    # Evict from the in-memory deque of any cached instance first, so a
    # concurrent reader cannot see a state where the file was truncated
    # but the deque still holds the dropped entry.
    popped_in_memory: str | None = None
    with _cache_lock:
        hist = _cache.get(key)
    if hist is not None and hist._loaded_strings:  # noqa: SLF001
        popped_in_memory = hist._loaded_strings.pop(0)  # noqa: SLF001

    if not key.exists():
        return popped_in_memory

    try:
        data = key.read_bytes()
    except OSError as e:
        logger.debug("failed to read history file %s: %s", key, e)
        return popped_in_memory

    if not data:
        return popped_in_memory

    # Find the last record header. Every entry produced by
    # ``FileHistory.store_string`` starts with ``\n# `` (the leading
    # newline guarantees the marker is unambiguous even when the file is
    # opened in append mode and the prior write ended without one).
    sep_idx = data.rfind(_RECORD_SEP)
    if sep_idx < 0:
        # File exists but contains no recognisable record (e.g. a file
        # written by the old `readline` backend, or hand-edited). Do not
        # touch it.
        logger.debug("no FileHistory record marker in %s; leaving file untouched", key)
        return popped_in_memory

    tail = data[sep_idx:].decode("utf-8", errors="replace")
    # Reconstruct the popped string the same way ``load_history_strings``
    # does: collect every ``+``-prefixed line, strip the leading ``+``, and
    # drop the final ``\n``.
    parts: list[str] = [line[1:] for line in tail.splitlines() if line.startswith("+")]
    popped_on_disk = "\n".join(parts) if parts else None

    try:
        with key.open("wb") as f:
            f.write(data[:sep_idx])
    except OSError as e:
        logger.debug("failed to rewrite history file %s: %s", key, e)
        return popped_in_memory or popped_on_disk

    return popped_on_disk or popped_in_memory
