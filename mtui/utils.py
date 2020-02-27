import fcntl
import os
import re
import readline
import struct
import subprocess
import tempfile
import termios
import time
from collections.abc import Callable
from contextlib import contextmanager
from copy import deepcopy
from functools import wraps
from itertools import chain
from os.path import dirname
from pathlib import Path
from shutil import move
from tempfile import mkstemp

from mtui.messages import TestReportNotLoadedError


def edit_text(text):
    editor = os.getenv("EDITOR", "vi")
    tmpfile = tempfile.NamedTemporaryFile()

    with open(tmpfile.name, "w") as tmp:
        tmp.write(text)

    subprocess.check_call((editor, tmpfile.name))

    with open(tmpfile.name, "r") as tmp:
        text = tmp.read().strip("\n")
        text = text.replace("'", '"')

    del tmpfile

    return text


if os.getenv("COLOR", "always") == "always":

    def green(xs):
        return "\033[1;32m{!s}\033[1;m".format(xs)

    def red(xs):
        return "\033[1;31m{!s}\033[1;m".format(xs)

    def yellow(xs):
        return "\033[1;33m{!s}\033[1;m".format(xs)

    def blue(xs):
        return "\033[1;34m{!s}\033[1;m".format(xs)


else:
    green = red = yellow = blue = lambda xs: str(xs)


def prompt_user(text, options, interactive=True):
    result = False
    response = False

    if not interactive:
        print(text)
        return False

    try:
        response = input(text).lower()
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
        x = fcntl.ioctl(0, termios.TIOCGWINSZ, "1234")
        height, width = struct.unpack("hh", x)
    except IOError:
        # FIXME: remove this when you figure out how to simulate tty
        # this might work:
        # https://github.com/dagwieers/ansible/commit/7192eb30477f8987836c075eece6e530eb9b07f2
        k_rows, k_cols = "ACCTEST_ROWS", "ACCTEST_COLS"
        env = os.environ
        if not (k_rows in env and k_cols in env):
            raise

        return int(env[k_rows]), int(env[k_cols])

    return width, height


def filter_ansi(text):
    text = re.sub(chr(27), "", text)
    text = re.sub(r"\[[0-9;]*[mA]", "", text)
    text = re.sub(r"\[K", "", text)

    return text


def page(text, interactive=True):
    if not interactive:
        return

    prompt = "Press Enter to continue... (q to quit)"

    width, height = termsize()

    text.reverse()

    try:
        line = filter_ansi(text.pop().rstrip("\r\n"))
    except IndexError:
        return

    while True:
        linesleft = height - 1
        while linesleft:
            linelist = [line[i : i + width] for i in range(0, len(line), width)]
            if not linelist:
                linelist = [""]
            lines2print = min(len(linelist), linesleft)
            for i in range(lines2print):
                print(linelist[i])
            linesleft -= lines2print
            linelist = linelist[lines2print:]

            if linelist:
                line = "".join(linelist)
                continue
            else:
                try:
                    line = filter_ansi(text.pop().rstrip("\r\n"))
                except IndexError:
                    return

        if prompt_user(prompt, "q"):
            return


def requires_update(fn):
    @wraps(fn)
    def wrap(self, *a, **kw):
        if not self.metadata:
            raise TestReportNotLoadedError()
        return fn(self, *a, **kw)

    return wrap


class DictWithInjections(dict):
    def __init__(self, *args, **kw):
        self.key_error = kw.pop("key_error", KeyError)

        super(DictWithInjections, self).__init__(*args, **kw)

    def __getitem__(self, x):
        try:
            return super(DictWithInjections, self).__getitem__(x)
        except KeyError:
            raise self.key_error(x)


class SUTParse:
    def __init__(self, args):
        suts = args.split(",")
        targets = ["-t {!s}".format(i) for i in suts]
        self.args = " ".join(targets)

    def print_args(self):
        return self.args


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

    choices = set(list(chain.from_iterable(synonyms)) + hostnames)

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
        if text == c[0 : len(text)]:
            endchoices.append(c)

    return endchoices


def complete_choices_filelist(synonyms, line, text, hostnames=None):
    dirname = ""
    filename = ""

    if text.startswith("~"):
        text = text.replace("~", os.path.expanduser("~"), 1)
        text += "/"

    if "/" in text:
        dirname = "/".join(text.split("/")[:-1])
        dirname += "/"

    if not dirname:
        dirname = "./"

    synonyms += [(dirname + i,) for i in os.listdir(dirname) if i.startswith(filename)]

    return complete_choices(synonyms, line, text, hostnames)


def timestamp():
    # remove fractional part
    return str(int(time.time()))


class check_eq(object):

    """
    Usage: check_eq(x)(y)
    :return: y for y if (x == y) is True otherwise raises
    :raises: ValueError
    """

    def __init__(self, *x):
        self.x = x

    def __call__(self, y):
        if y not in self.x:
            raise ValueError("Expected: {0!r}, got: {1!r}".format(self.x, y))
        return y

    def __repr__(self):
        return "<{0}.{1} {2!r}>".format(
            self.__class__.__module__, self.__class__.__name__, self.x
        )


@contextmanager
def chdir(newpath):
    """Context manager for changing the current working directory"""
    storedpath = os.getcwd()
    os.chdir(newpath)
    yield
    os.chdir(storedpath)


def ensure_dir_exists(*path, **kwargs):
    """
    :returns: str joined path with dirs created as needed.
    :type path: [str] to join

    :type filepath: bool
    :param filepath: path is treated as directory if False, otherwise as
        file and last component is not created as directory.
    :param on_create: Callable operation on created dir
    """

    on_create = kwargs.get("on_create", None)
    filepath = kwargs.get("filepath", False)

    path = Path().joinpath(*path)
    dirn = path.parent if filepath else path

    os.makedirs(dirn, exist_ok=True)

    if isinstance(on_create, Callable):
        on_create(path=dirn)

    return path


def atomic_write_file(data, path):
    if isinstance(data, bytes):
        data = data.decode("utf-8")
    fd, fname = mkstemp(dir=dirname(path))
    with os.fdopen(fd, "w") as f:
        f.write(data)

    move(fname, path)


def walk(inc):
    if isinstance(inc, list):
        for i, j in enumerate(inc):
            inc[i] = walk(j)
    if isinstance(inc, dict):
        if len(inc) == 1:
            if "edges" in inc:
                return walk(inc["edges"])
            elif "node" in inc:
                tmp = deepcopy(inc["node"])
                del inc["node"]
                inc.update(tmp)
        for key in inc:
            if isinstance(inc[key], (list, dict)):
                inc[key] = walk(inc[key])
    return inc
