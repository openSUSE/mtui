"""Coloured output: ANSI constants, logging formatter, runtime mode switch.

Three sub-modules live here. They are always used together, so every
public name is re-exported at the package level — callers should write
``from mtui.cli.colors import green`` rather than reaching into the
sub-modules directly.
"""

from .ansi import blue, green, red, yellow
from .formatter import COLOR_SEQ, COLORS, RESET_SEQ, ColorFormatter, create_logger
from .mode import ColorMode, colors_enabled, get_mode, set_mode

__all__ = [
    "COLORS",
    "COLOR_SEQ",
    "ColorFormatter",
    "ColorMode",
    "RESET_SEQ",
    "blue",
    "colors_enabled",
    "create_logger",
    "get_mode",
    "green",
    "red",
    "set_mode",
    "yellow",
]
