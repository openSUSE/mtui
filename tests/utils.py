# -*- coding: utf-8 -*-

try:
    from cStringIO import StringIO
except ImportError:
    from StringIO import StringIO

from mtui.config import Config

class LogFake:
    def __init__(self):
        self.errors = []
        self.warnings = []
        self.debugs = []

        self.error = self.errors.append
        self.warning = self.warnings.append
        self.debug = self.debugs.append

        self.t_setLevels = []

    def __repr__(self):
        return repr(self.__dict__)

    def __str__(self):
        return repr(self)

    def setLevel(self, level=None):
        self.t_setLevels.append(level)

class ConfigFake(Config):
    """
    Make sure the interface of the fake is the same as the real one by
    deriving the real config but making sure it doesn't hit the
    filesystem and resolving the config values results in exception
    which then results in using default value.

    To set different desired values in testcase, just assign them.
    """
    def read(self):
        class ConfigParser(object):
            def get(*a, **kw):
                raise NotImplementedError

            def getboolean(*a, **kw):
                raise NotImplementedError
        self.config = ConfigParser()

def touch(x):
    open(x, 'a').close()


class ProductAlreadyProducedError(RuntimeError):
    pass

class OneShotFactory(object):
    def __init__(self, productClass):
        self.productClass = productClass
        self.product = None

    def __call__(self, *args, **kw):
        if self.product:
            raise ProductAlreadyProducedError(self.productClass)

        self.product = self._make_product(args, kw)
        return self.product

    def _make_product(self, args, kw):
        return self.productClass(*args, **kw)
