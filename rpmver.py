#!/usr/bin/python
# -*- coding: utf-8 -*-

import os
import rpm


class RPMFile(object):

    def __init__(self, filename):
        ts = rpm.ts()
        fdno = os.open(filename, os.O_RDONLY)
        hdr = ts.hdrFromFdno(fdno)
        os.close(fdno)

        self.disturl = hdr[rpm.RPMTAG_DISTURL]
        self.version = hdr[rpm.RPMTAG_VERSION]
        self.release = hdr[rpm.RPMTAG_RELEASE]
        self.name = hdr[rpm.RPMTAG_NAME]


class RPMVersion(object):

    def __init__(self, ver, *args):
        if '-' in ver:
            (self.ver, self.rel) = ver.rsplit('-')
        else:
            self.ver = ver
            self.rel = '0'

    def __lt__(self, other):
        return rpm.labelCompare(('1', self.ver, self.rel), ('1', other.ver, other.rel)) < 0

    def __gt__(self, other):
        return rpm.labelCompare(('1', self.ver, self.rel), ('1', other.ver, other.rel)) > 0

    def __eq__(self, other):
        return rpm.labelCompare(('1', self.ver, self.rel), ('1', other.ver, other.rel)) == 0

    def __le__(self, other):
        return rpm.labelCompare(('1', self.ver, self.rel), ('1', other.ver, other.rel)) <= 0

    def __ge__(self, other):
        return rpm.labelCompare(('1', self.ver, self.rel), ('1', other.ver, other.rel)) >= 0

    def __ne__(self, other):
        return rpm.labelCompare(('1', self.ver, self.rel), ('1', other.ver, other.rel)) != 0


