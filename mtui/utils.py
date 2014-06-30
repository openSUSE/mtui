# -*- coding: utf-8 -*-
#
# occasionally used functions which don't match anywhere else
#

import os
import time
import fcntl
import struct
import termios
import logging
import tempfile
import readline
import re
from errno import EEXIST

from tempfile import mkstemp
from shutil import move

out = logging.getLogger('mtui')

flatten = lambda xs: [y for ys in xs for y in ys if not y is None]

def timestamp():
    return str(int(time.time()))


def edit_text(text):
    editor = os.environ.get('EDITOR', 'vi')
    tmpfile = tempfile.NamedTemporaryFile()

    try:
        with open(tmpfile.name, 'w') as tmp:
            tmp.write(text)
    except Exception:
        out.error('failed to write temp file')

    try:
        os.system('%s %s' % (editor, tmpfile.name))
    except Exception:
        out.error('failed to open temp file')

    try:
        with open(tmpfile.name, 'r') as tmp:
            text = tmp.read().strip('\n')
            text = text.replace("'", '"')
    except Exception:
        out.error('failed to read temp file')

    del tmpfile

    return text


def green(text):
    return "\033[1;32m%s\033[1;m" % text


def red(text):
    return "\033[1;31m%s\033[1;m" % text


def yellow(text):
    return "\033[1;33m%s\033[1;m" % text


def blue(text):
    return "\033[1;34m%s\033[1;m" % text


def input(text, options, interactive=True):
    result = False
    response = False

    if not interactive:
        print text
        return False

    try:
        response = raw_input(text).lower()
        if response and response in options:
            result = True
    except KeyboardInterrupt:
        pass
    finally:
        if response:
            hlen = readline.get_current_history_length()
            if hlen > 0:
                # this is normaly not a problem but it breaks acceptance
                # tests
                readline.remove_history_item(hlen -1)

        return result


def termsize():
    try:
        x = fcntl.ioctl(0, termios.TIOCGWINSZ, '1234')
        height, width = struct.unpack('hh', x)
    except IOError:
        # FIXME: remove this when you figure out how to simulate tty
        # this might work:
        # https://github.com/dagwieers/ansible/commit/7192eb30477f8987836c075eece6e530eb9b07f2
        k_rows, k_cols = 'ACCTEST_ROWS', 'ACCTEST_COLS'
        env = os.environ
        if not (env.has_key(k_rows) and env.has_key(k_cols)):
            raise

        return int(env[k_rows]), int(env[k_cols])

    return width, height


def filter_ansi(text):
    text = re.sub(chr(27), '', text)
    text = re.sub('\[[0-9;]*[mA]', '', text)
    text = re.sub('\[K', '', text)

    return text


def page(text, interactive=True):
    if not interactive:
        return

    prompt = "Press Enter to continue... (q to quit)"

    width, height = termsize()

    text.reverse()

    try:
        line = filter_ansi(text.pop().rstrip('\r\n'))
    except IndexError:
        return

    while True:
        linesleft = height - 1
        while linesleft:
            linelist = [line[i:i+width] for i in xrange(0, len(line), width)]
            if not linelist:
                linelist = ['']
            lines2print = min(len(linelist), linesleft)
            for i in range(lines2print):
                print linelist[i]
            linesleft -= lines2print
            linelist = linelist[lines2print:]

            if linelist:
                line = ''.join(linelist)
                continue
            else:
                try:
                    line = filter_ansi(text.pop().rstrip('\r\n'))
                except IndexError:
                    return

        if input(prompt, "q"):
            return

def log_exception(eclass, logger):
    def wrap(fn):
        def wrap2(*args, **kw):
            try:
                return fn(*args, **kw)
            except Exception as e:
                if isinstance(e, eclass):
                    logger(e)
                    logger(traceback.format_exc(e))
                raise e
        return wrap2
    return wrap

def ensure_dir_exists(dir_, on_create=None):
    try:
        os.makedirs(dir_)
    except OSError as e:
        if e.errno != EEXIST:
            raise
    else:
        if callable(on_create):
            on_create(path=dir_)

class chdir:
    """Context manager for changing the current working directory"""

    def __init__(self, newPath):
        self.newPath = newPath

    def __enter__(self):
        self.savedPath = os.getcwd()
        os.chdir(self.newPath)

    def __exit__(self, etype, value, traceback):
        os.chdir(self.savedPath)

def atomic_write_file(data, path):
    fd, fname = mkstemp(dir=dirname(path))
    with os.fdopen(fd, "w") as f:
        f.write(data)

    move(fname, path)
