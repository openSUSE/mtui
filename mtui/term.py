"""Terminal-facing helpers.

Houses the synchronous prompt (:func:`prompt_user`), a pager
(:func:`page`), and the two low-level helpers they need
(:func:`termsize`, :func:`filter_ansi`). All four were historically
part of ``mtui.utils``.
"""

import fcntl
import os
import re
import readline
import struct
import termios
from collections.abc import Collection


def termsize() -> tuple[int, int]:
    """Gets the size of the terminal.

    Returns:
        A tuple containing the width and height of the terminal.

    """
    try:
        x = fcntl.ioctl(0, termios.TIOCGWINSZ, b"1234")
        height, width = struct.unpack("hh", x)
    except OSError:
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
        print(text)  # noqa: T201
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


def page(text: list[str], interactive: bool = True) -> None:
    """Displays long text in a pager-like fashion.

    Args:
        text: A list of strings to display.
        interactive: If False, the function does nothing.

    """
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
                print(linelist[i])  # noqa: T201
            linesleft -= lines2print
            linelist = linelist[lines2print:]

            if linelist:
                line = "".join(linelist)
                continue
            try:
                line = filter_ansi(text.pop().rstrip("\r\n"))
            except IndexError:
                return

        if prompt_user(prompt, ("q",)):
            return
