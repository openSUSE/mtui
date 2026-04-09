"""The Maintenance Test Update Installer (MTUI).

This package provides a command-line tool for running shell commands on
multiple hosts in parallel, with a focus on maintenance update testing.
"""

__version__ = "16.1.0"


def __getattr__(name):
    if name == "loose_version":
        from looseversion import LooseVersion

        return LooseVersion(__version__)
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
