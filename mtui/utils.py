# -*- coding: utf-8 -*-
#
# occasionally used functions which don't match anywhere else
#

import os
import time
import fcntl
import struct
import termios
import tempfile
import readline
import re


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
            readline.remove_history_item(readline.get_current_history_length() - 1)

        return result


def termsize():
    height, width = struct.unpack('hh', fcntl.ioctl(0, termios.TIOCGWINSZ, '1234'))

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
