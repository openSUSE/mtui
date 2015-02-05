from distutils.version import StrictVersion

__all__ = ['main', 'log', 'export']

__version__ = '3.0.4'
# PEP396

strict_version = StrictVersion(__version__)
