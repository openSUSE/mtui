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
        A list of possible completion strings. A candidate that equals
        ``text`` exactly is included alongside any longer candidate
        that shares the same prefix (e.g. typing ``Doc`` fully still
        offers a sibling ``Documents``) rather than short-circuiting
        to just the exact match.

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

    # Every candidate sharing ``text`` as a prefix is a match — this
    # already includes an exact match, since ``c[0:len(text)] == text``
    # trivially holds when ``c == text``. Do not special-case (and
    # short-circuit on) an exact match: iterating a set has no defined
    # order, so returning early the moment one is found would silently
    # drop other, longer candidates depending on iteration order (e.g.
    # a directory named "Doc" sitting next to "Documents").
    return [c for c in choices if text == c[0 : len(text)]]


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
        directory names. A leading ``~``/``~user`` in ``text`` is
        expanded first (:func:`os.path.expanduser` semantics), so the
        file candidates for a tilde path come back as absolute paths.
        Directory candidates are suffixed with ``/`` (shell
        convention) so a following TAB descends into their contents.
        A tilde path that already names an existing directory (a bare
        ``~``/``~user``, or ``~/some/existing/dir`` typed with no
        trailing slash) behaves as if the trailing slash had already
        been typed -- descending straight into it -- *unless* a
        sibling entry shares the same prefix (e.g. a directory ``Doc``
        next to ``Documents``), in which case forcing the descent
        would silently hide that sibling, so the parent's entries are
        listed instead.

    """
    is_tilde = text.startswith("~")
    if is_tilde:
        text = os.path.expanduser(text)

    # List the directory portion of the typed path (everything up to and
    # including the last ``/``; the current directory when there is none).
    # The typed basename prefix is matched by complete_choices() against
    # the rebuilt ``dirname + entry`` candidates.
    dirname = text[: text.rindex("/") + 1] if "/" in text else "./"

    # Tab-completion fires on every keystroke, including transient
    # typos like ``,/`` (missed shift on ``./``). A missing directory,
    # a non-directory in the path, or an unreadable directory must not
    # tear down the completer — just yield no file suggestions.
    try:
        entries = os.listdir(dirname)
    except (FileNotFoundError, NotADirectoryError, PermissionError):
        entries = []

    if is_tilde and not text.endswith("/") and os.path.isdir(text):
        # The tilde-expanded text itself names an existing directory
        # (bare "~"/"~user", or e.g. "~/Documents" typed in full). Its
        # "parent" (the directory `dirname` currently points at, e.g.
        # the home directory's own parent for a bare "~") is never
        # what the user means to browse. Force the descent only when
        # unambiguous: no sibling of the parent shares this basename
        # as a prefix (that sibling would otherwise vanish once we
        # switch to listing the directory itself).
        basename = text[len(dirname) :]
        if sum(1 for e in entries if e.startswith(basename)) <= 1:
            text += "/"
            dirname = text
            try:
                entries = os.listdir(dirname)
            except (FileNotFoundError, NotADirectoryError, PermissionError):
                entries = []

    synonyms += [
        (dirname + i + ("/" if os.path.isdir(dirname + i) else ""),) for i in entries
    ]

    return complete_choices(synonyms, line, text, hostnames)
