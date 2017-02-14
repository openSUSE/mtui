from distutils.version import StrictVersion

__all__ = ['main', 'log', 'export']

__version__ = '7.1.2'
# PEP396

strict_version = StrictVersion(__version__)
