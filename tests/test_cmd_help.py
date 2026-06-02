"""Tests for the `help` command.

The ``help`` command restores the listing + per-command help that
:class:`cmd.Cmd` used to provide for free. These tests pin the four
behaviours documented in ``Documentation/iui.rst``:

* No argument lists every registered command.
* A known argument prints that command's argparser ``--help``.
* An unknown argument raises a :class:`UserError` for the REPL loop's
  error handler to surface.
* Tab completion offers registered command names.
"""

from __future__ import annotations

import io
from argparse import Namespace
from typing import Any
from unittest.mock import MagicMock

import pytest

from mtui.commands import registry
from mtui.commands.help import Help, UnknownHelpTopicError


def _prompt(commands: dict[str, Any]) -> MagicMock:
    """Stand-in for :class:`CommandPrompt` exposing the surface ``Help`` reads.

    ``commands`` is typed loosely so tests can pass either the real
    registry (``dict[str, type[Command]]``) or hand-rolled
    ``MagicMock`` doubles for the documented/undocumented split tests.
    """
    p = MagicMock()
    p.commands = commands
    return p


def _sys_with_capture() -> tuple[MagicMock, io.StringIO]:
    """Build a fake ``sys`` whose ``stdout`` is a :class:`StringIO` we can read."""
    buf = io.StringIO()
    sys_mock = MagicMock()
    sys_mock.stdout = buf
    return sys_mock, buf


# --------------------------------------------------------------------------- #
# No-arg listing                                                              #
# --------------------------------------------------------------------------- #


def test_help_no_arg_lists_every_registered_command(mock_config):
    """``help`` with no arg surfaces every name from ``prompt.commands``."""
    prompt = _prompt(dict(registry))  # snapshot so the test stays stable
    sys_mock, buf = _sys_with_capture()

    Help(Namespace(command=None), mock_config, sys_mock, prompt)()

    output = buf.getvalue()
    # Every registered command name appears somewhere in the output.
    for name in registry:
        assert name in output, f"{name!r} missing from help listing"
    # Listing is sorted: scan documented section and ensure first
    # documented name precedes the second.
    documented = sorted(n for n, c in registry.items() if (c.__doc__ or "").strip())
    if len(documented) >= 2:
        assert output.index(documented[0]) < output.index(documented[1])


def test_help_no_arg_splits_documented_and_undocumented(mock_config):
    """Undocumented commands land under the dedicated bucket."""
    documented_cmd = MagicMock(spec=type)
    documented_cmd.__doc__ = "I have docs."
    undocumented_cmd = MagicMock(spec=type)
    undocumented_cmd.__doc__ = None

    prompt = _prompt({"doc_cmd": documented_cmd, "nodoc_cmd": undocumented_cmd})
    sys_mock, buf = _sys_with_capture()

    Help(Namespace(command=None), mock_config, sys_mock, prompt)()

    output = buf.getvalue()
    assert "Documented commands" in output
    assert "Undocumented commands" in output
    # ``doc_cmd`` lands before the Undocumented header; ``nodoc_cmd`` after.
    undoc_header_idx = output.index("Undocumented commands")
    assert output.index("doc_cmd") < undoc_header_idx
    assert output.index("nodoc_cmd") > undoc_header_idx


def test_help_no_arg_omits_undocumented_section_when_all_have_docs(mock_config):
    """Skip the Undocumented header entirely when there is nothing to show."""
    documented_cmd = MagicMock(spec=type)
    documented_cmd.__doc__ = "I have docs."
    prompt = _prompt({"only_cmd": documented_cmd})
    sys_mock, buf = _sys_with_capture()

    Help(Namespace(command=None), mock_config, sys_mock, prompt)()

    output = buf.getvalue()
    assert "Documented commands" in output
    assert "Undocumented commands" not in output


# --------------------------------------------------------------------------- #
# Known argument                                                              #
# --------------------------------------------------------------------------- #


def test_help_known_command_prints_argparser_help(mock_config):
    """``help quit`` runs ``Quit.argparser(sys).print_help()``."""
    prompt = _prompt(dict(registry))
    sys_mock, buf = _sys_with_capture()

    Help(Namespace(command="quit"), mock_config, sys_mock, prompt)()

    output = buf.getvalue()
    # argparse prints "usage: <prog> ..." — ``Quit.command == "quit"`` is
    # the parser ``prog``, so it must appear in the usage line.
    assert "usage:" in output
    assert "quit" in output


# --------------------------------------------------------------------------- #
# Unknown argument                                                            #
# --------------------------------------------------------------------------- #


def test_help_unknown_command_raises_user_error(mock_config):
    """Unknown topics surface as a :class:`UserError` for the loop to log."""
    prompt = _prompt(dict(registry))
    sys_mock, _ = _sys_with_capture()

    with pytest.raises(UnknownHelpTopicError) as exc:
        Help(
            Namespace(command="not_a_real_command_xyz"),
            mock_config,
            sys_mock,
            prompt,
        )()

    assert "not_a_real_command_xyz" in str(exc.value)


# --------------------------------------------------------------------------- #
# Tab completion                                                              #
# --------------------------------------------------------------------------- #


def test_help_complete_offers_registered_command_prefix():
    """``help qu<TAB>`` offers ``quit`` (and any other ``qu``-prefixed command)."""
    completions = Help.complete(
        state={"hosts": MagicMock(), "metadata": MagicMock(), "config": MagicMock()},
        text="qu",
        line="help qu",
        begidx=5,
        endidx=7,
    )

    assert "quit" in completions
    # And the completer returns only prefix matches: every result must
    # start with the typed text.
    assert all(c.startswith("qu") for c in completions)


def test_help_complete_empty_prefix_returns_all_commands():
    """No prefix => completer offers the whole registry."""
    completions = Help.complete(
        state={"hosts": MagicMock(), "metadata": MagicMock(), "config": MagicMock()},
        text="",
        line="help ",
        begidx=5,
        endidx=5,
    )

    # Every registered name shows up.
    for name in registry:
        assert name in completions
