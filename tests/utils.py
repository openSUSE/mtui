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
import os.path
import string
import random
from posix import stat_result
from tempfile import mktemp
from datetime import date
from time import sleep

from mtui.template import OBSTestReport
from mtui.template import SwampTestReport
from mtui.refhost import Refhosts

from mtui.target import RunCommand
from mtui.target import FileUpload

from pprint import pprint

unused = None

class SysFake(object):
    def __init__(self, argv=unused):
        self.argv = argv
        self.stdout = StringIO()
        self.stderr = StringIO()
        self.stdin = StringIO()

class LogFake:
    def __init__(self):
        self.t_setLevels = []

        self.errors    = []
        self.warnings  = []
        self.debugs    = []
        self.infos     = []
        self.criticals = []

    def _norm(self, x):
        return x

    def error   (self, x): self.__log('errors'   , x)
    def warning (self, x): self.__log('warnings' , x)
    def debug   (self, x): self.__log('debugs'   , x)
    def info    (self, x): self.__log('infos'    , x)
    def critical(self, x): self.__log('criticals', x)

    def __log(self, level, msg):
        getattr(self, level).append(self._norm(msg))

    def setLevel(self, level=None):
        self.t_setLevels.append(level)

class LogTestingWrap(object):
    """
    Wraps LogFake object with interface to ease testing so tests are
    more conscise but the added interface features can not interfere
    with the code under test
    """
    def __init__(self, log = None):
        self.log = log if log else LogFake()

    @classmethod
    def empty(cls):
        return cls().all()

    def all(self):
        """
        :returns: dict(level = [message])
        """
        return dict(
              errors = self.log.errors
            , warnings = self.log.warnings
            , debugs = self.log.debugs
            , infos = self.log.infos
            , criticals = self.log.criticals
        )

    def error   (self, x): self.log.error(x)    ; return self
    def warning (self, x): self.log.warning(x)  ; return self
    def debug   (self, x): self.log.debug(x)    ; return self
    def info    (self, x): self.log.info(x)     ; return self
    def critical(self, x): self.log.critical(x) ; return self

    def __repr__(self):
        return repr(self.all())

    def __str__(self):
        return repr(self)

    def pprint(self):
        pprint(self.all())

class LogFakeStr(LogFake):
    def _norm(self, x):
        return str(x)

class RefhostsFake(Refhosts):
    def __init__(self, *a, **kw):
        xs = list(a)
        xs[0] = refhosts_fixtures['basic']
        super(RefhostsFake, self).__init__(*xs, **kw)

def _find_refhosts_fixtures():
    def p(*xs):
        return os.path.join(
            os.path.dirname(__file__)
          , "fixtures"
          , "refhosts"
          , *xs
        )

    return dict([(os.path.splitext(os.path.basename(x))[0], p(x))
        for x in os.listdir(p())
    ])

refhosts_fixtures = _find_refhosts_fixtures()

class ConfigFake(Config):
    """
    Make sure the interface of the fake is the same as the real one by
    deriving the real config but making sure it doesn't hit the
    filesystem and resolving the config values results in exception
    which then results in using default value.

    To set different desired values in testcase, just assign them.
    """
    def __init__(self, overrides = None, refhosts = RefhostsFake):
        super(ConfigFake, self).__init__(refhosts = refhosts)
        if overrides:
            for k, v in overrides.items():
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
    return random.randrange(1, 9999)

def rand_review_id():
    """
    :return: int random id of review id component in OBS review
        request id
    """
    return random.randrange(1, 9999)

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

def merged_dict(x, y):
    """
    Returns new dict with items from `y` merged into `x`
    """
    return dict(x.items() + y.items())

def random_alphanum(min_, max_):
    return ''.join(random.sample(
        string.digits + string.uppercase + string.lowercase
      , random.randint(min_, max_)
    ))
