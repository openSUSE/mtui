from distutils.version import StrictVersion

__all__ = ['main', 'log', 'export']

__version__ = '2.0.0a1'
# PEP396

strict_version = StrictVersion(__version__)
