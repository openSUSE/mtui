"""Locate filesystem paths used by mtui.

Two flavours of paths live here:

* **Package data paths** (:func:`scripts_path`, :func:`terms_path`) — the
  on-disk locations of the ``scripts/`` and ``terms/`` directories shipped
  inside the installed ``mtui`` package.  Resolved via
  :mod:`importlib.resources`.
* **User cache paths** (:func:`save_cache_path`) — the XDG cache directory
  where mtui persists per-user state.
"""

from __future__ import annotations

from importlib.resources import files
from pathlib import Path

from xdg.BaseDirectory import save_cache_path as x_save_cache_path

# --- Package data paths ---------------------------------------------------


def _package_data_path() -> Path:
    """Return the filesystem path to the mtui package directory.

    This is the root from which ``scripts/`` and ``terms/`` subdirectories
    can be reached.
    """
    pkg = files("mtui")

    # importlib.resources.files() returns a Traversable; for packages
    # installed on disk (the normal case) it is already a Path.  We
    # convert explicitly so callers get a real Path they can pass to
    # subprocess, shutil, glob, etc.
    return Path(str(pkg))


def scripts_path() -> Path:
    """Return the path to the ``scripts/`` data directory."""
    return _package_data_path() / "scripts"


def terms_path() -> Path:
    """Return the path to the ``terms/`` data directory."""
    return _package_data_path() / "terms"


# --- User cache paths -----------------------------------------------------

app = "mtui"


def save_cache_path(*args: str) -> Path:
    """Returns a path to a file in the user's cache directory.

    Args:
        *args: The path components to join to the cache directory.

    Returns:
        A `Path` object representing the full path to the file.

    """
    return Path(x_save_cache_path(app)).joinpath(*args)
