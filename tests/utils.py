# -*- coding: utf-8 -*-

try:
    from cStringIO import StringIO
except ImportError:
    from StringIO import StringIO

class LogMock:
    def __init__(self):
        self.errors = []
        self.warnings = []
        self.debugs = []

        self.error = self.errors.append
        self.warning = self.warnings.append
        self.debug = self.debugs.append

    def __repr__(self):
        return repr(self.__dict__)

    def __str__(self):
        return repr(self)

def touch(x):
    open(x, 'a').close()
