from distutils.version import LooseVersion

__all__ = ["main"]

__version__ = "12.3.0"

# PEP396
loose_version = LooseVersion(__version__)
