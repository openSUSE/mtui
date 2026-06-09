"""Terminal-facing helpers.

Houses the two synchronous prompts (:func:`prompt_user` for yes/no
confirmations, :func:`ask_user` for free-form text), a pager
(:func:`page`), and the two low-level helpers they need
(:func:`termsize`, :func:`filter_ansi`). All four were historically
part of ``mtui.utils``.
"""

import fcntl
import os
import re
import struct
import termios
from collections.abc import Callable, Collection

from prompt_toolkit import PromptSession
from prompt_toolkit.history import InMemoryHistory

from ._history import default_history_path, pop_last_entry


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


def _read_line(text: str) -> str:
    """Read one line from the user through prompt_toolkit.

    Shared by :func:`prompt_user` and :func:`ask_user` so the terminal
    state (raw mode, cursor key bindings, alt-screen) set up by the
    outer REPL ``PromptSession`` is properly saved and restored around
    every mid-command read. A bare ``input()`` here would inherit
    prompt_toolkit's half-configured TTY and either echo literal ``^M``
    or block entirely, depending on the host terminal.

    Uses an ephemeral :class:`InMemoryHistory` so transient answers
    (yes/no confirmations, free-form comments) never reach
    ``~/.mtui_history``. The shared on-disk file is still owned by
    :mod:`mtui.cli._history`.
    """
    return PromptSession(history=InMemoryHistory()).prompt(text)


def prompt_user(
    text: str,
    options: Collection[str],
    interactive: bool = True,
    default: bool = False,
) -> bool:
    """Prompts the user with a question and waits for a response.

    Args:
        text: The prompt to display to the user.
        options: A collection of strings that are considered "yes" answers.
        interactive: If False, the prompt is printed but no input is requested.
        default: Result returned when the user submits an empty response
            (just presses Enter) in interactive mode. Lets a prompt have a
            preselected answer, e.g. ``[Y/n]``. Non-interactive mode always
            returns False so a defaulted prompt never auto-confirms an
            unattended (e.g. destructive) action.

    Returns:
        True if the user's response is in `options` (or empty and
        ``default`` is True), False otherwise.

    """
    result = False
    response = ""

    if not interactive:
        print(text)  # noqa: T201
        return False

    try:
        response = _read_line(text).lower()
        if not response:
            result = default
        elif response in options:
            result = True
    except (KeyboardInterrupt, EOFError):
        pass
    finally:
        if response:
            # Defensive scrub: even with the ephemeral in-memory history
            # used inside ``_read_line``, a stale ``readline``-era entry
            # written by an older mtui (or a hand-edit) would still be
            # popped, matching the previous behaviour the acceptance
            # tests lock in.
            pop_last_entry(default_history_path())

    return result


def ask_user(text: str, interactive: bool = True) -> str:
    """Read a free-form line of input from the user.

    Counterpart to :func:`prompt_user` for prompts that take an
    arbitrary string instead of a yes/no answer (e.g. a comment body).
    Routes through prompt_toolkit so the surrounding REPL's terminal
    state is preserved; a bare ``input()`` would hang or echo literal
    ``^M`` between two ``PromptSession`` calls.

    Args:
        text: The prompt to display to the user.
        interactive: If False, the prompt is printed and an empty string
            is returned. Lets non-interactive callers reach this helper
            without trying to read from a closed stdin.

    Returns:
        The line typed by the user with surrounding whitespace stripped,
        or an empty string when the user cancels with Ctrl-C / Ctrl-D
        or when ``interactive`` is False.

    """
    if not interactive:
        print(text)  # noqa: T201
        return ""

    try:
        return _read_line(text).strip()
    except (KeyboardInterrupt, EOFError):
        return ""


def page(
    text: list[str],
    interactive: bool = True,
    writer: Callable[[str], None] | None = None,
) -> None:
    """Displays long text in a pager-like fashion.

    Args:
        text: A list of strings to display.
        interactive: If False and ``writer`` is None, the function does
            nothing (preserves historical no-op behaviour). If False and
            ``writer`` is provided, each line in ``text`` is forwarded
            to ``writer`` without pagination or width-wrapping — used by
            non-TTY transports (e.g. ``mtui-mcp``) that render their own
            output.
        writer: Optional per-line callback used in non-interactive mode
            to route output into a caller-supplied sink (typically
            ``self.display.println``).

    """
    if not interactive:
        if writer is not None:
            for line in text:
                writer(line.rstrip("\r\n"))
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
