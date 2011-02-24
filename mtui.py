#!/usr/bin/env python
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
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.	See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program; if not, write to the Free Software Foundation,
# Inc., 51 Franklin Street, Fifth Floor, Boston, MA 02110-1301, USA.

from main import *

if __name__ == "__main__":
	try:
		main()
	except:
		print "you found a bug. please notify ckornacker@suse.de"
		print "backtrace:"
		print '-'*60
		traceback.print_exc(file=sys.stdout)
		print '-'*60

