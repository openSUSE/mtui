#!/usr/bin/env python
# -*- coding: utf-8 -*-

import os
import time
import tempfile

def timestamp():
	return str(time.time()).split('.')[0]

def edit_text(text):
	editor = os.environ.get("EDITOR", "vi")
	tmpfile = tempfile.NamedTemporaryFile()

	try:
		with open(tmpfile.name, 'w') as tmp:
			tmp.write(text)
	except Exception:
		out.error("failed to write temp file")

	try:
		os.system("%s %s" % (editor, tmpfile.name))
	except Exception:
		out.error("failed to open temp file")

	try:
		with open(tmpfile.name, 'r') as tmp:
			text = tmp.read().strip('\n')
			text = text.replace("'", '"')

	except Exception:
		out.error("failed to read temp file")

	del tmpfile

	return text

def green(text):
	return "\033[1;32m%s\033[1;m" % text

def red(text):
	return "\033[1;31m%s\033[1;m" % text

def yellow(text):
	return "\033[1;33m%s\033[1;m" % text

def input(text, options):
	try:
		if raw_input(text).lower() in options:
			return True
		else:
			return False
	except KeyboardInterrupt:
		return False

