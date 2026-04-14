"""Locate package data files bundled with mtui.

Uses importlib.resources to find scripts, helper files, and terminal
launcher scripts that are shipped inside the mtui package.
"""

from __future__ import annotations

from importlib.resources import files
from pathlib import Path


def _package_data_path() -> Path:
    """Return the filesystem path to the mtui package directory.

    This is the root from which ``scripts/``, ``helper/``, and
    ``terms/`` subdirectories can be reached.
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


def helper_path() -> Path:
    """Return the path to the ``helper/`` data directory."""
    return _package_data_path() / "helper"


def terms_path() -> Path:
    """Return the path to the ``terms/`` data directory."""
    return _package_data_path() / "terms"
