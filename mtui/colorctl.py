"""Runtime control over coloured output.

Both the colour helpers in :mod:`mtui.utils` and the
:class:`mtui.colorlog.ColorFormatter` consult :func:`colors_enabled` at
call time, so the value of the ``--color`` flag (set by
:func:`mtui.main.main`) and the ``NO_COLOR`` environment variable take
effect for every output call without needing reimport.

Decision matrix (highest precedence first):

1. Explicit mode ``"always"``  → colours on.
2. Explicit mode ``"never"``   → colours off.
3. ``NO_COLOR`` env var set    → colours off (per https://no-color.org).
4. ``COLOR=never``  env var    → colours off (legacy mtui knob).
5. ``COLOR=always`` env var    → colours on  (legacy mtui knob).
6. Mode ``"auto"`` (default)   → on iff ``sys.stderr.isatty()``.
"""

from __future__ import annotations

import os
import sys
from typing import Literal

ColorMode = Literal["auto", "always", "never"]

_mode: ColorMode = "auto"


def set_mode(mode: ColorMode) -> None:
    """Set the global colour mode. Called from :func:`mtui.main.main`."""
    global _mode
    _mode = mode


def get_mode() -> ColorMode:
    """Return the currently configured colour mode."""
    return _mode


def colors_enabled() -> bool:
    """Return True if colour escapes should be emitted right now."""
    if _mode == "always":
        return True
    if _mode == "never":
        return False

    # auto: honour NO_COLOR (any non-empty value disables, per spec)
    if os.environ.get("NO_COLOR"):
        return False

    # Legacy knob, kept for backward compatibility with existing setups.
    legacy = os.environ.get("COLOR")
    if legacy == "never":
        return False
    if legacy == "always":
        return True

    return sys.stderr.isatty()
