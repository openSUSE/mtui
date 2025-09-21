"""This package contains parser functions for mtui.

Each module in this package defines a parser function that can be
used to parse data related to target hosts.
"""

from .system import parse_system

__all__ = ["parse_system"]
