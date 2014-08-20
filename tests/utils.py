# -*- coding: utf-8 -*-

try:
    from cStringIO import StringIO
except ImportError:
    try:
        from StringIO import StringIO
    except ImportError:
        from io import StringIO

from mtui.config import Config
try:
    from configparser import ConfigParser
except ImportError:
    from ConfigParser import ConfigParser
from os.path import exists
from posix import stat_result
from tempfile import mktemp
from random import randrange

unused = None

class SysFake(object):
    def __init__(self, argv=unused):
        self.argv = argv
        self.stdout = StringIO()
        self.stderr = StringIO()

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
    def __init__(self, overrides=None):
        super(ConfigFake, self).__init__()
        if overrides:
            for k,v in overrides.items():
                self.set_option(k, v)

    def read(self):
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

def get_nonexistent_path():
    return mktemp()

class ConstMtimeStat(object):
    def __init__(self, mtime):
        self.mtime = mtime

    def __call__(self, _):
        return stat_result([self.mtime if x is 8 else None
            for x in range(10)])
        #  This object may be accessed either as a tuple of
        # (0   , 1  , 2  , 3    , 4  , 5  , 6   , 7    , 8    , 9    )
        # (mode, ino, dev, nlink, uid, gid, size, atime, mtime, ctime)

class ConstFloat(object):
    def __init__(self, x):
        self.x = x

    def __call__(self, *_, **__):
        return self.x

class Raiser(object):
    def __init__(self, e):
        """
        :param e: exception to be raised
        """
        self.e = e

    def __call__(self, *_, **__):
        raise self.e

class CallLogger(object):
    def __init__(self):
        self.calls = []

    def __call__(self, *a, **kw):
        self.calls.append((a, kw))

def rand_maintenance_id():
    """
    :return: int random id of maintenance id component in OBS review
        request id
    """
    return randrange(1, 9999)

def rand_review_id():
    """
    :return: int random id of review id component in OBS review
        request id
    """
    return randrange(1, 9999)
