# -*- coding: utf-8 -*-
#
# occasionally used functions which don't match anywhere else
#

from __future__ import print_function

import os
import os.path as opa
import time
import fcntl
import struct
import termios
import tempfile
import readline
import subprocess
import re
from errno import EEXIST

from tempfile import mkstemp
from shutil import move
from os.path import dirname
from os.path import join
from mtui.messages import TestReportNotLoadedError

try:
    from nose.tools import nottest
except ImportError:
    nottest = lambda x: x

try:
    xrange
except NameError:
    xrange = range

try:
    user_input = raw_input
except NameError:
    user_input = input

flatten = lambda xs: [y for ys in xs for y in ys if y is not None]


def timestamp():
    return str(int(time.time()))


def edit_text(text):
    editor = os.environ.get('EDITOR', 'vi')
    tmpfile = tempfile.NamedTemporaryFile()

    with open(tmpfile.name, 'w') as tmp:
        tmp.write(text)

    subprocess.check_call((editor, tmpfile.name))

    with open(tmpfile.name, 'r') as tmp:
        text = tmp.read().strip('\n')
        text = text.replace("'", '"')

    del tmpfile

    return text

if os.getenv('COLOR', 'always') == 'always':
    green = lambda xs: "\033[1;32m%s\033[1;m" % xs
    red = lambda xs: "\033[1;31m%s\033[1;m" % xs
    yellow = lambda xs: "\033[1;33m%s\033[1;m" % xs
    blue = lambda xs: "\033[1;34m%s\033[1;m" % xs
else:
    green = red = yellow = blue = lambda xs: str(xs)


def prompt_user(text, options, interactive=True):
    result = False
    response = False

    if not interactive:
        print(text)
        return False

    try:
        response = user_input(text).lower()
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
                readline.remove_history_item(hlen - 1)

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
        if not (k_rows in env and k_cols in env):
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
                print(linelist[i])
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

        if prompt_user(prompt, "q"):
            return


def mkdir_p(path):
    try:
        os.makedirs(path)
    except EnvironmentError as e:
        if e.errno == EEXIST and opa.isdir(path):
            return
        raise e


def ensure_dir_exists(*path, **kwargs):
    """
    :returns: str joined path with dirs created as needed.
    :type path: [str] to join

    :type filepath: bool
    :param filepath: path is treated as directory if False, otherwise as
        file and last component is not created as directory.
    """

    on_create = kwargs.get('on_create', None)
    filepath = kwargs.get('filepath', False)

    path = join(*path)
    dirn = dirname(path) if filepath else path

    mkdir_p(dirn)

    if callable(on_create):
        on_create(path=dirn)

    return path


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


class check_eq(object):

    """
    Usage: check_eq(x)(y)
    :return: None for y if (x == y) is True otherwise raises
    :raises: ValueError
    """

    def __init__(self, *x):
        self.x = x

    def __call__(self, x):
        if x not in self.x:
            raise ValueError("Expected: {0!r}, got: {1!r}".format(
                self.x, x))

    def __repr__(self):
        return "<{0}.{1} {2!r}>".format(
            self.__class__.__module__,
            self.__class__.__name__,
            self.x
        )


def requires_update(fn):
    def wrap(self, *a, **kw):
        if not self.metadata:
            raise TestReportNotLoadedError()

        return fn(self, *a, **kw)

    wrap.__name__ = fn.__name__
    wrap.__doc__ = fn.__doc__
    return wrap


def ass_is(x, class_, maybe_none=False):
    if maybe_none and x is None:
        return
    assert isinstance(x, class_), "got {0!r}".format(x)

if __debug__:
    def ass_isL(xs, class_):
        for x in xs:
            ass_is(x, class_)
else:
    def ass_isL(_, __):
        pass


class UnknownSystemError(ValueError):
    pass


def get_release(systems):
    systems = ' '.join(systems)

    for rexp, release in {
        'rhel': 'YUM',
        'sle[sd]12': 'ZYPPER',
        'sap-aio12': 'ZYPPER',
        '(manager|mgr|sles4vmware|cloud|studio|slms|sle.11)': 'ZYPPER',
    }.items():
        if re.search(rexp, systems):
            return release

    raise UnknownSystemError(systems)


class DictWithInjections(dict):

    def __init__(self, *args, **kw):
        self.key_error = kw.pop('key_error', KeyError)

        super(DictWithInjections, self).__init__(*args, **kw)

    def __getitem__(self, x):
        try:
            return super(DictWithInjections, self).__getitem__(x)
        except KeyError:
            raise self.key_error(x)


def complete_choices(synonyms, line, text, hostnames=None):
    """
    :returns: [str] completion choices appropriate for given line and
        text

    :type synonyms: [[str]]
    :param synonyms: each element of the list is a list of
        synonymous arguments. Example: [("-a", "--all")]

    :type hostnames: [str] or None
    :param hostnames: hostnames to add to possible completions

    :param line: line from L{cmd.Cmd} completion callback
    :param text: text from L{cmd.Cmd} completion callback
    """

    if not hostnames:
        hostnames = []

    choices = set(flatten(synonyms) + hostnames)

    ls = line.split(" ")
    ls.pop(0)

    for l in ls:
        if len(l) >= 2 and l[0] == "-" and l[1] != "-":
            if len(l) > 2:
                for c in list(l[1:]):
                    ls.append("-" + c)

                continue

        for s in synonyms:
            if l in s:
                choices = choices - set(s)

    endchoices = []
    for c in choices:
        if text == c:
            return [c]
        if text == c[0:len(text)]:
            endchoices.append(c)

    return endchoices

def complete_choices_filelist(synonyms, line, text, hostnames=None):
    dirname = ''
    filename = ''

    if text.startswith('~'):
        text = text.replace('~', os.path.expanduser('~'), 1)
        text += '/'

    if '/' in text:
        dirname = '/'.join(text.split('/')[:-1])
        dirname += '/'

    if not dirname:
        dirname = './'

    synonyms += [(dirname + i,)
                 for i in os.listdir(dirname) if i.startswith(filename)]

    return complete_choices(synonyms, line, text, hostnames)
