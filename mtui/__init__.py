from distutils.version import StrictVersion

__all__ = ['main', 'log', 'export']

__version__ = '8.0.0'
# PEP396

strict_version = StrictVersion(__version__)
