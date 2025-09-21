"""This package contains the check dictionaries for mtui.

Each module in this package defines a dictionary of checks that can be
performed on a target host for a specific action, such as installing,
updating, or uninstalling packages.
"""

from .downgrade import downgrade_checks
from .install import install_checks
from .prepare import prepare_checks
from .update import update_checks

__all__ = ["downgrade_checks", "install_checks", "prepare_checks", "update_checks"]
