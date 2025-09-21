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

from .messages import TestReportNotLoadedError


def edit_text(text: str) -> str:
    """Opens the user's default editor to edit the given text.

    Args:
        text: The initial text to be edited.

    Returns:
        The edited text.
    """
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
        """Wraps a string in ANSI escape codes to make it green.

        Args:
            xs: The string to color.

        Returns:
            The colorized string.
        """
        return "\033[1;32m{!s}\033[1;m\033[0m".format(xs)

    def red(xs: str) -> str:
        """Wraps a string in ANSI escape codes to make it red.

        Args:
            xs: The string to color.

        Returns:
            The colorized string.
        """
        return "\033[1;31m{!s}\033[1;m\033[0m".format(xs)

    def yellow(xs: str) -> str:
        """Wraps a string in ANSI escape codes to make it yellow.

        Args:
            xs: The string to color.

        Returns:
            The colorized string.
        """
        return "\033[1;33m{!s}\033[1;m\033[0m".format(xs)

    def blue(xs: str) -> str:
        """Wraps a string in ANSI escape codes to make it blue.

        Args:
            xs: The string to color.

        Returns:
            The colorized string.
        """
        return "\033[1;34m{!s}\033[1;m\033[0m".format(xs)

else:
    green = red = yellow = blue = lambda xs: str(xs)


def prompt_user(text: str, options: Collection[str], interactive: bool = True) -> bool:
    """Prompts the user with a question and waits for a response.

    Args:
        text: The prompt to display to the user.
        options: A collection of strings that are considered "yes" answers.
        interactive: If False, the prompt is printed but no input is requested.

    Returns:
        True if the user's response is in `options`, False otherwise.
    """
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
    """Gets the size of the terminal.

    Returns:
        A tuple containing the width and height of the terminal.
    """
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
    """Removes ANSI escape codes from a string.

    Args:
        text: The string to filter.

    Returns:
        The string with ANSI escape codes removed.
    """
    text = re.sub(chr(27), "", text)
    text = re.sub(r"\[[0-9;]*[mA]", "", text)
    text = re.sub(r"\[K", "", text)

    return text


def page(text: list[str], interactive: bool = True) -> None:
    """Displays long text in a pager-like fashion.

    Args:
        text: A list of strings to display.
        interactive: If False, the function does nothing.
    """
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
    """A decorator that checks if a test report is loaded before executing.

    Args:
        fn: The function to decorate.

    Returns:
        The decorated function.
    """

    @wraps(fn)
    def wrap(self, *a, **kw) -> Any:
        if not self.metadata:
            raise TestReportNotLoadedError()
        return fn(self, *a, **kw)

    return wrap


class DictWithInjections(dict):
    """A dictionary that allows for a custom error on key lookup failure."""

    def __init__(self, *args, **kw) -> None:
        """Initializes the dictionary.

        Args:
            *args: Arguments to pass to the dict constructor.
            **kw: Keyword arguments to pass to the dict constructor.
                'key_error' is a special keyword argument that specifies
                the exception to raise on a key error.
        """
        self.key_error = kw.pop("key_error", KeyError)

        super().__init__(*args, **kw)

    def __getitem__(self, x):
        try:
            return super().__getitem__(x)
        except KeyError:
            raise self.key_error(x)


class SUTParse:
    """Parses a comma-separated string of SUTs into a formatted string."""

    def __init__(self, args: str) -> None:
        """Initializes the parser.

        Args:
            args: A comma-separated string of SUTs.
        """
        suts = args.split(",")
        targets = ["-t {!s}".format(i) for i in suts]
        self.args = " ".join(targets)

    def print_args(self) -> str:
        """Returns the formatted string of SUTs.

        Returns:
            The formatted string.
        """
        return self.args


def complete_choices(
    synonyms: list[tuple[str, ...]],
    line: str,
    text: str,
    hostnames: list[str] | None = None,
) -> list[str]:
    """Provides command-line completion for choices.

    Args:
        synonyms: A list of tuples, where each tuple contains
            synonymous arguments (e.g., `("-a", "--all")`).
        line: The current command line string.
        text: The text being completed.
        hostnames: A list of hostnames to include in the completion choices.

    Returns:
        A list of possible completion strings.
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
    """Provides command-line completion for file paths.

    Args:
        synonyms: A list of tuples, where each tuple contains
            synonymous arguments.
        line: The current command line string.
        text: The text being completed.
        hostnames: A list of hostnames to include in the completion choices.

    Returns:
        A list of possible completion strings, including file and
        directory names.
    """
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
    """Gets the current time as a Unix timestamp string.

    Returns:
        The current time as a string.
    """
    # remove fractional part
    return str(int(time.time()))


@contextmanager
def chdir(newpath: Path):
    """A context manager for changing the current working directory.

    Args:
        newpath: The path to change to.
    """
    storedpath = Path().cwd()
    os.chdir(newpath)
    yield
    os.chdir(storedpath)


def ensure_dir_exists(*path, **kwargs) -> Path:
    """Ensures that a directory exists, creating it if necessary.

    Args:
        *path: The path components to join to form the directory path.
        **kwargs:
            filepath: If True, the last component of the path is treated
                as a filename, and only its parent directory is created.
            on_create: A callable to be executed on the created directory.

    Returns:
        The Path object for the created directory.
    """

    def empty(*args, **kwds) -> None:  # type: ignore
        pass

    on_create: Callable = kwargs.get("on_create", empty)
    filepath: bool = kwargs.get("filepath", False)

    pt = Path().joinpath(*path)
    dirn = pt.parent if filepath else pt

    dirn.absolute().mkdir(parents=True, exist_ok=True)

    on_create(path=dirn)

    return pt


def atomic_write_file(data: bytes | str, path: Path) -> None:
    """Atomically writes data to a file.

    Args:
        data: The data to write, as bytes or a string.
        path: The path to the file.
    """
    if isinstance(data, bytes):
        data = data.decode("utf-8")
    fd, fname = mkstemp(dir=path.parent)

    with os.fdopen(fd, "w") as f:
        f.write(data)

    move(fname, path)


def walk(inc: Collection) -> Collection:
    """Recursively walks through a nested data structure.

    This function is designed to simplify nested data structures,
    particularly those that resemble GraphQL responses with 'edges'
    and 'node' keys.

    Args:
        inc: The collection (list or dict) to walk through.

    Returns:
        The modified collection.
    """
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
