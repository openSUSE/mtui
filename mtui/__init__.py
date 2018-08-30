from distutils.version import LooseVersion
from qamlib.utils import Path

__all__ = ["main"]

__version__ = "10.2.0"
# PEP396

loose_version = LooseVersion(__version__)
