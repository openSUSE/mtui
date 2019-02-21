from distutils.version import LooseVersion

__all__ = ["main"]

__version__ = "11.1.0dev"
# PEP396

loose_version = LooseVersion(__version__)
