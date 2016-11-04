# -*- coding: utf-8 -*-
#
# querying tags from local rpm files
#

import os
import rpm


class RPMFile(object):
    """parse local rpm file

    queries some common rpm tags and stores them in the object.
    this could be extended if needed.

    """

    def __init__(self, filename):
        # query rpm metadata and close file again
        ts = rpm.ts()
        fdno = os.open(filename, os.O_RDONLY)
        hdr = ts.hdrFromFdno(fdno)
        os.close(fdno)

        self.disturl = hdr[rpm.RPMTAG_DISTURL]
        self.version = hdr[rpm.RPMTAG_VERSION]
        self.release = hdr[rpm.RPMTAG_RELEASE]
        self.name = hdr[rpm.RPMTAG_NAME]


class RPMVersion(object):
    """RPMVersion holds an rpm version-release string

    this is userd for rpm version arithmetics, like comparing
    if a specific rpm version is lower or higher than another one

    """

    _arch_suffixes = [
        'noarch',
        'x86_64',
        's390x',
        'ppc64le',
        'aarch64'
    ]
    """
    :param _arch_suffixes: arch suffixes we get in addition to version on sle12
    """

    def __init__(self, ver, *args):
        for x in self._arch_suffixes:
            ver = ver.replace('.' + x, '')

        if '-' in ver:
            # split rpm version string into version and release string
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

    def __str__(self):
        s = str(self.ver)
        if self.rel != '0':
            s += "-" + str(self.rel)
        return s
