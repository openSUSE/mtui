"""Filesystem helpers shared across MTUI.

Contains a thin re-export of :func:`contextlib.chdir` (kept for callers
that historically imported it from ``mtui.utils``) plus the
``ensure_dir_exists`` / ``atomic_write_file`` helpers and the
``timestamp`` formatter used by report-writing code paths.
"""

import os
import time
from collections.abc import Callable
from contextlib import chdir as chdir  # noqa: PLC0414  # re-exported for callers
from contextlib import suppress
from pathlib import Path
from shutil import move
from tempfile import mkstemp


def timestamp() -> str:
    """Gets the current time as a Unix timestamp string.

    Returns:
        The current time as a string.

    """
    # remove fractional part
    return str(int(time.time()))


def ensure_dir_exists(*path, **kwargs) -> Path:
    """Ensures that a directory exists, creating it if necessary.

    Args:
        *path: The path components to join to form the directory path.
        **kwargs:
            filepath: If True, the last component of the path is treated
                as a filename, and only its parent directory is created.
            on_create: A callable to be executed on the created directory.

    Returns:
        The Path object for the created directory.

    """

    def empty(*args, **kwds) -> None:
        pass

    on_create: Callable = kwargs.get("on_create", empty)
    filepath: bool = kwargs.get("filepath", False)

    pt = Path().joinpath(*path)
    dirn = pt.parent if filepath else pt

    dirn.absolute().mkdir(parents=True, exist_ok=True)

    on_create(path=dirn)

    return pt


def atomic_write_file(data: bytes | str, path: Path) -> None:
    """Atomically writes data to a file.

    Args:
        data: The data to write, as bytes or a string.
        path: The path to the file.

    """
    if isinstance(data, bytes):
        data = data.decode("utf-8")
    # Ensure the destination directory exists; ``mkstemp(dir=...)`` and the
    # final ``move`` both require it. Cache locations such as
    # ``~/.cache/mtui`` may be absent on a fresh checkout, which otherwise
    # makes the write (e.g. the refhosts.yml HTTPS download) fail with a
    # ``FileNotFoundError`` that masks the real reason for the resolve.
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, fname = mkstemp(dir=path.parent)

    try:
        with os.fdopen(fd, "w") as f:
            f.write(data)
        move(fname, path)
    except BaseException:
        # Don't leave the temp file littering the destination directory if the
        # write or the move failed (on a successful move it is already gone).
        with suppress(OSError):
            os.unlink(fname)
        raise
