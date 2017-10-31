from distutils.version import LooseVersion

__all__ = ['main', 'log', 'export']

__version__ = '9.0.0a1'
# PEP396

loose_version = LooseVersion(__version__)
