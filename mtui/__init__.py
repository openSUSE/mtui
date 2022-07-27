from distutils.version import LooseVersion

__all__ = ["main"]

__version__ = "13.2.1"

# PEP396
loose_version = LooseVersion(__version__)
