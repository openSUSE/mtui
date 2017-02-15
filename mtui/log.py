# -*- coding: utf-8 -*-
#
# implementation of a logging.Formatter to enable color output
#

import inspect
import logging

(BLACK, RED, GREEN, YELLOW, BLUE, MAGENTA, CYAN, WHITE) = range(8)

RESET_SEQ = "\033[0m"
COLOR_SEQ = "\033[1;{}m"

COLORS = {
    'WARNING': YELLOW,
    'INFO': GREEN,
    'DEBUG': BLUE,
    'CRITICAL': RED,
    'ERROR': RED}


class ColorFormatter(logging.Formatter):

    def __init__(self, msg):
        logging.Formatter.__init__(self, msg)

    def formatColor(self, levelname):
        if levelname == 'DEBUG':
            caller = inspect.currentframe()
            frame, filename, line, function, _, _ = inspect.getouterframes(
                caller)[9]
            try:
                module = inspect.getmodule(frame).__name__
            except Exception:
                module = 'unknown'
            return "\033[2K" + COLOR_SEQ.format(30 + COLORS[levelname]) + levelname.lower(
                ) + RESET_SEQ + ' [{!s}:{!s}]'.format(module, function)
        else:
            return "\033[2K" + COLOR_SEQ.format(
                30 + COLORS[levelname]) + levelname.lower() + RESET_SEQ

    def format(self, record):
        record.message = record.getMessage()
        if self._fmt.find('%(levelname)') >= 0:
            record.levelname = self.formatColor(record.levelname)

        return logging.Formatter.format(self, record)


def create_logger():
    out = logging.getLogger('mtui')
    out.setLevel(level=logging.INFO)
    handler = logging.StreamHandler()
    formatter = ColorFormatter('%(levelname)s: %(message)s')
    handler.setFormatter(formatter)
    out.addHandler(handler)
    return out
