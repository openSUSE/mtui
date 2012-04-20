#!/usr/bin/python
# -*- coding: utf-8 -*-

# Copyright (C) 2011: ckornacker@suse.de
#
# This program is free software; you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation; either version 2 of the License,
# or (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.    See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program; if not, write to the Free Software Foundation,
# Inc., 51 Franklin Street, Fifth Floor, Boston, MA 02110-1301, USA.

#
# mtui starting point. checking for all needed modules and invoke main()
#

import sys
import traceback
import logging
import warnings

from log import *

out = logging.getLogger('mtui')


def check_modules():
    """check if all mandatory modules are installed on the system

    currently we need:
    paramiko - for the ssh/network management. this has most likely a
               dependency to python-crypto
    rpm      - for comparing rpm versions and getting rpm metadata
               on the local machine

    """

    modules = {'paramiko': 'python-paramiko', 'rpm': 'rpm-python'}

    for (module, package) in modules.items():
        try:
            with warnings.catch_warnings():
                # paramiko uses some deprecated python code, ignore it since
                # it's only internal stuff and doesn't need to bother the tester
                warnings.filterwarnings('ignore', category=DeprecationWarning)
                exec 'import %s' % module
        except ImportError:
            # exit if a mandatory module couldn't be loaded
            out.error('missing %s module. please install %s' % (module, modules[module]))
            sys.exit(-1)
        else:
            # unload module again after we made sure it exists
            exec 'del %s' % module


if __name__ == '__main__':
    try:
        check_modules()

        from main import main
        main()
    except Exception:
        # all uncatched exceptions end up here and generate a nice backtrace
        out.error('you found a bug. please notify ckornacker@suse.de')
        print 'backtrace:'
        print '-' * 60
        traceback.print_exc(file=sys.stdout)
        print '-' * 60

