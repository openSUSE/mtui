"""The Maintenance Test Update Installer (MTUI).

This package provides a command-line tool for running shell commands on
multiple hosts in parallel, with a focus on maintenance update testing.
"""

from looseversion import LooseVersion

__version__ = "16.0.4"

# PEP396
loose_version = LooseVersion(__version__)
