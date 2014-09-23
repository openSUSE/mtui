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
from datetime import date
from time import sleep

from mtui.template import OBSTestReport
from mtui.template import SwampTestReport

from mtui.target import RunCommand
from mtui.target import FileUpload

from pprint import pprint

unused = None

class SysFake(object):
    def __init__(self, argv=unused):
        self.argv = argv
        self.stdout = StringIO()
        self.stderr = StringIO()

class LogFake:
    _conv = lambda _, x: x
    def __init__(self):
        self.__setup(self._conv)

    def __setup(self, conv):
        self.__levels = ['error', 'warning', 'debug', 'info', 'critical']
        for i in self.__levels:
            setattr(self, i+"s", list())
            setattr(self, i, (lambda i: lambda x: getattr(self, i).append(conv(x)))(i+"s"))
            # because reasons

        self.t_setLevels = []

    def __repr__(self):
        return repr(self.__dict__)

    def __str__(self):
        return repr(self)

    def setLevel(self, level=None):
        self.t_setLevels.append(level)

    def pprint(self):
        pprint(dict([
            (x, getattr(self, x))
            for x in [i+"s" for i in self.__levels]
        ]))

class LogFakeStr(LogFake):
    _conv = lambda _, x: str(x)

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

def testreports():
    return [OBSTestReport, SwampTestReport]

class _Hostnames:
    foo = "foo.example.org"
    bar = "bar.example.org"
    qux = "qux.example.org"

hostnames = _Hostnames()

def TRF(tr, config = None, log = None, date_ = None, **kw):
    if not config:
        config = ConfigFake()

    if not log:
        log = LogFake()

    if not date:
        date_ = date

    return tr(config, log, date_, **kw)

def SF(s, tr, path):
    """
    L{Script} Factory

    :type s: L{Script} class
    :type tr: L{TestReport} instance

    :type path: str
    :param path: path to the script
    """
    return s(
        tr,
        path,
        LogFake(),
        FileUpload,
        RunCommand,
    )

class MD5HexdigestFactory(object):
    def __init__(self):
        self.base = 0

    def __call__(self):
        """
        :Returns: str that is valid md5 hexdigest and has not been returned
             before
        """
        self.base += 1
        return "{0:0=32}".format(self.base -1)

new_md5 = MD5HexdigestFactory()

def wait_for_ctrlc():
    """
    Helpful to insert into testcases for inspecting prepared working
    directory
    """
    try:
        while True:
            sleep(10)
    except KeyboardInterrupt:
        pass
