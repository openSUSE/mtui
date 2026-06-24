"""Readline tab-completion helpers.

These two helpers back the ``complete_<name>`` methods registered by
each :class:`mtui.commands._command.Command` on the interactive prompt.
"""

import os
from collections.abc import Mapping, Sequence
from itertools import chain
from typing import Any


def template_completion(state: Mapping[str, Any]) -> list[tuple[str, ...]]:
    """Builds completion synonyms for the ``-T``/``--template`` flags.

    Fan-out commands accept ``-T``/``--template RRID`` (scope to one loaded
    template) and ``--all-templates`` (force fan-out). This helper returns the
    flag tokens plus each loaded RRID as its own synonym group so a user can
    tab-complete both the flag and the template to act on, mirroring the
    ``switch`` / ``unload`` commands.

    Args:
        state: The prompt state dict passed to ``complete``; ``state["templates"]``
            is the :class:`~mtui.template_registry.TemplateRegistry`.

    Returns:
        Synonym groups suitable for appending to the list passed to
        :func:`complete_choices` / :func:`complete_choices_filelist`.

    """
    templates = state.get("templates")
    groups: list[tuple[str, ...]] = [("-T", "--template"), ("--all-templates",)]
    if templates:
        groups += [(rrid,) for rrid in templates.rrids()]
    return groups


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

    # Tab-completion fires on every keystroke, including transient
    # typos like ``,/`` (missed shift on ``./``). A missing directory,
    # a non-directory in the path, or an unreadable directory must not
    # tear down the completer — just yield no file suggestions.
    try:
        entries = os.listdir(dirname)
    except (FileNotFoundError, NotADirectoryError, PermissionError):
        entries = []

    synonyms += [(dirname + i,) for i in entries if i.startswith(filename)]

    return complete_choices(synonyms, line, text, hostnames)
