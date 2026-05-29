"""ANSI colour helpers.

The four helpers in this module wrap a string in ANSI escape codes for
terminal colour output. Each honours the runtime colour mode controlled
by :mod:`mtui.colorctl` (see the ``--color`` flag and the ``NO_COLOR``
environment variable) and returns the input unchanged when colour is
disabled.
"""

from .colorctl import colors_enabled


def green(xs: str) -> str:
    """Wraps a string in ANSI escape codes to make it green.

    Honours the runtime colour mode (see :mod:`mtui.colorctl`); returns
    the input unchanged when colour is disabled.
    """
    if not colors_enabled():
        return str(xs)
    return f"\033[1;32m{xs!s}\033[1;m\033[0m"


def red(xs: str) -> str:
    """Wraps a string in ANSI escape codes to make it red.

    Honours the runtime colour mode (see :mod:`mtui.colorctl`); returns
    the input unchanged when colour is disabled.
    """
    if not colors_enabled():
        return str(xs)
    return f"\033[1;31m{xs!s}\033[1;m\033[0m"


def yellow(xs: str) -> str:
    """Wraps a string in ANSI escape codes to make it yellow.

    Honours the runtime colour mode (see :mod:`mtui.colorctl`); returns
    the input unchanged when colour is disabled.
    """
    if not colors_enabled():
        return str(xs)
    return f"\033[1;33m{xs!s}\033[1;m\033[0m"


def blue(xs: str) -> str:
    """Wraps a string in ANSI escape codes to make it blue.

    Honours the runtime colour mode (see :mod:`mtui.colorctl`); returns
    the input unchanged when colour is disabled.
    """
    if not colors_enabled():
        return str(xs)
    return f"\033[1;34m{xs!s}\033[1;m\033[0m"
