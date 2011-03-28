#!/usr/bin/env python
# -*- coding: utf-8 -*-

import logging

BLACK, RED, GREEN, YELLOW, BLUE, MAGENTA, CYAN, WHITE = range(8)

RESET_SEQ = "\033[0m"
COLOR_SEQ = "\033[1;%dm"

COLORS = {
    'WARNING': YELLOW,
    'INFO': GREEN,
    'DEBUG': BLUE,
    'CRITICAL': RED,
    'ERROR': RED
}

out = logging.getLogger('mtui')

class ColorFormatter(logging.Formatter):
	def __init__(self, msg):
		logging.Formatter.__init__(self, msg)

	def formatColor(self, levelname):
		return COLOR_SEQ % (30 + COLORS[levelname]) + levelname.lower() + RESET_SEQ

	def format(self, record):
		record.message = record.getMessage()
		if self._fmt.find("%(levelname)") >= 0:
			record.levelname = self.formatColor(record.levelname)

		return logging.Formatter.format(self, record)

out.setLevel(level=logging.INFO)
handler = logging.StreamHandler()
formatter = ColorFormatter("%(levelname)s: %(message)s")
handler.setFormatter(formatter)
out.addHandler(handler)

