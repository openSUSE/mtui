from distutils.version import StrictVersion

__all__ = ['main', 'log', 'export']

__version__ = '1.2.0'
# PEP396

strict_version = StrictVersion(__version__)
