"""Readline tab-completion helpers.

These two helpers back the ``complete_<name>`` methods registered by
each :class:`mtui.commands._command.Command` on the interactive prompt.
"""

import os
from collections.abc import Sequence
from itertools import chain


def complete_choices(
    synonyms: Sequence[tuple[str, ...]],
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
        if len(line) >= 2 and line[0] == "-" and line[1] != "-" and len(line) > 2:
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
