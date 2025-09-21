"""This package contains the action classes for mtui.

Each module in this package defines a specific action that can be
performed on a target host, such as installing, updating, or
uninstalling packages.
"""

from .downgrade import downgrader
from .install import installer
from .prepare import preparer
from .uninstall import uninstaller
from .update import updater

__all__ = ["installer", "uninstaller", "preparer", "downgrader", "updater"]
