import fcntl
import os
import re
import readline
import struct
import subprocess
import tempfile
import termios
import time
from collections.abc import Callable, Collection
from contextlib import contextmanager
from copy import deepcopy
from functools import wraps
from itertools import chain
from pathlib import Path
from shutil import move
from tempfile import mkstemp
from typing import Any

from .exceptions import ComponentParseError, InternalParseError, MissingComponent
from .messages import TestReportNotLoadedError


def edit_text(text: str) -> str:
    editor = os.getenv("EDITOR", "vim")
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

    def green(xs: str) -> str:
        return "\033[1;32m{!s}\033[1;m\033[0m".format(xs)

    def red(xs: str) -> str:
        return "\033[1;31m{!s}\033[1;m\033[0m".format(xs)

    def yellow(xs: str) -> str:
        return "\033[1;33m{!s}\033[1;m\033[0m".format(xs)

    def blue(xs: str) -> str:
        return "\033[1;34m{!s}\033[1;m\033[0m".format(xs)

else:
    green = red = yellow = blue = lambda xs: str(xs)


def prompt_user(text: str, options: Collection[str], interactive: bool = True) -> bool:
    result = False
    response = ""

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


def termsize() -> tuple[int, int]:
    try:
        x = fcntl.ioctl(0, termios.TIOCGWINSZ, b"1234")
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

    return int(width), int(height)


def filter_ansi(text: str) -> str:
    text = re.sub(chr(27), "", text)
    text = re.sub(r"\[[0-9;]*[mA]", "", text)
    text = re.sub(r"\[K", "", text)

    return text


def page(text: list[str], interactive: bool = True) -> None:
    if not interactive:
        return None

    prompt = "Press Enter to continue... (q to quit)"

    width, height = termsize()

    text.reverse()

    try:
        line = filter_ansi(text.pop().rstrip("\r\n"))
    except IndexError:
        return None

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
                    return None

        if prompt_user(prompt, ("q",)):
            return None


def requires_update(fn: Callable) -> Callable:
    @wraps(fn)
    def wrap(self, *a, **kw) -> Any:
        if not self.metadata:
            raise TestReportNotLoadedError()
        return fn(self, *a, **kw)

    return wrap


class DictWithInjections(dict):
    def __init__(self, *args, **kw) -> None:
        self.key_error = kw.pop("key_error", KeyError)

        super().__init__(*args, **kw)

    def __getitem__(self, x):
        try:
            return super().__getitem__(x)
        except KeyError:
            raise self.key_error(x)


class SUTParse:
    def __init__(self, args: str) -> None:
        suts = args.split(",")
        targets = ["-t {!s}".format(i) for i in suts]
        self.args = " ".join(targets)

    def print_args(self) -> str:
        return self.args


def complete_choices(
    synonyms: list[tuple[str, ...]],
    line: str,
    text: str,
    hostnames: list[str] | None = None,
) -> list[str]:
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
    _ = ls.pop(0)

    for line in ls:
        if len(line) >= 2 and line[0] == "-" and line[1] != "-":
            if len(line) > 2:
                for c in list(line[1:]):
                    ls.append("-" + c)

                continue

        for s in synonyms:
            if line in s:
                choices = choices - set(s)

    endchoices: list[str] = []
    for c in choices:
        if text == c:
            return [c]
        if text == c[0 : len(text)]:
            endchoices.append(c)

    return endchoices


def complete_choices_filelist(
    synonyms: list[tuple[str, ...]],
    line: str,
    text: str,
    hostnames: list[str] | None = None,
) -> list[str]:
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


def timestamp() -> str:
    # remove fractional part
    return str(int(time.time()))


class check_eq:
    """
    Usage: check_eq(x)(y)
    :return: y for y if (x == y) is True otherwise raises
    :raises: ValueError
    """

    def __init__(self, *x) -> None:
        self.x: tuple[Any, ...] = x

    def __call__(self, y: Any) -> Any:
        if y not in self.x:
            raise ValueError(f"Expected: {self.x!r}, got: {y!r}")
        return y

    def __repr__(self) -> str:
        return f"<{self.__class__.__module__}.{self.__class__.__name__} {self.x!r}>"

    def __str__(self) -> str:
        return f"{self.x!r}"


class check_type:
    """
    Usage: check_type(x)(y)
    :return: y for y if x(y) otherwise raises
    :raises: ValueError
    """

    def __init__(self, *x) -> None:
        self.x: tuple[Any, ...] = x

    def __call__(self, y: Any) -> Any:
        for f in self.x:
            try:
                return f(y)
            except ValueError:
                err = True
                pass
        if err:
            raise ValueError(f"Expected {self.x!r}, got: {y!r}")

    def __repr__(self) -> str:
        return f"<{self.__class__.__module__}.{self.__class__.__name__} {self.x!r}>"

    def __str__(self) -> str:
        return f"convertible to {self.x!r}"


@contextmanager
def chdir(newpath: Path):
    """Context manager for changing the current working directory"""
    storedpath = Path().cwd()
    os.chdir(newpath)
    yield
    os.chdir(storedpath)


def ensure_dir_exists(*path, **kwargs) -> Path:
    """
    :returns: path with dirs created as needed.
    :type path: Path

    :type filepath: bool
    :param filepath: path is treated as directory if False, otherwise as
        file and last component is not created as directory.
    :param on_create: Callable operation on created dir
    """

    def empty(*args, **kwds) -> None:
        pass

    on_create: Callable = kwargs.get("on_create", empty)
    filepath: bool = kwargs.get("filepath", False)

    pt = Path().joinpath(*path)
    dirn = pt.parent if filepath else pt

    dirn.absolute().mkdir(parents=True, exist_ok=True)

    on_create(path=dirn)

    return pt


def atomic_write_file(data: bytes | str, path: Path) -> None:
    if isinstance(data, bytes):
        data = data.decode("utf-8")
    fd, fname = mkstemp(dir=path.parent)

    with os.fdopen(fd, "w") as f:
        f.write(data)

    move(fname, path)


def walk(inc: Collection) -> Collection:
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
            if isinstance(inc[key], list | dict):
                inc[key] = walk(inc[key])
    return inc


def apply_parser(f, x, cnt):
    if not f or not cnt:
        raise InternalParseError(f, cnt)

    if not x:
        raise MissingComponent(cnt, f)

    try:
        return f(x)
    except Exception as e:
        new = ComponentParseError(cnt, f, x)
        new.__cause__ = e
        raise new
