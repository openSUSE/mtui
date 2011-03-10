#!/usr/bin/env python
# -*- coding: utf-8 -*-

import rpm

def vercmp(a, b):
	ver1, ver2 = a, b
	if '-' in ver1: ver1, ver1_rel = ver1.rsplit('-')
	if '-' in ver2: ver2, ver2_rel = ver2.rsplit('-')

	return rpm.labelCompare(('1', ver1, ver1_rel), ('1', ver2, ver2_rel))

