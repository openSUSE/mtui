"""Tests for the `add_host` command."""

from __future__ import annotations

import sys
from argparse import Namespace
from io import StringIO
from unittest.mock import MagicMock, patch

from mtui.commands.addhost import AddHost


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.testplatforms = ["base=sles(major=15);arch=[x86_64]"]
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_add_host_argparser_accepts_target_and_keep_mode():
    """The argument parser wires up -t/--target and -k/--keep-mode."""
    ns = AddHost.parse_args("-t h1 -t h2 -k", sys)
    assert ns.target == ["h1", "h2"]
    assert ns.keep_mode is True

    default = AddHost.parse_args("", sys)
    assert default.target is None
    assert default.keep_mode is False


def test_add_host_complete_offers_flags():
    """Tab completion offers -t and the new -k/--keep-mode flag."""
    out = AddHost.complete({"hosts": []}, "", "add_host ", 9, 9)
    assert "--keep-mode" in out
    assert "--target" in out


def test_add_host_with_explicit_targets_submits_per_host(mock_config):
    prompt = _prompt()
    args = Namespace(target=["h1", "h2"], keep_mode=False)

    with (
        patch("mtui.commands.addhost.concurrent.futures.ThreadPoolExecutor") as tpe,
        patch("mtui.commands.addhost.concurrent.futures.wait") as wait,
    ):
        executor = MagicMock()
        tpe.return_value.__enter__.return_value = executor
        AddHost(args, mock_config, MagicMock(), prompt)()
        wait.assert_called_once()

    assert executor.submit.call_count == 2
    submitted_args = [c.args[1] for c in executor.submit.call_args_list]
    assert submitted_args == ["h1", "h2"]
    # the callable passed must be the metadata.add_target bound method
    for c in executor.submit.call_args_list:
        assert c.args[0] is prompt.metadata.add_target


def test_add_host_without_targets_uses_testplatforms(mock_config):
    prompt = _prompt()
    args = Namespace(target=None, keep_mode=False)

    AddHost(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.refhosts_from_tp.assert_called_once_with(
        "base=sles(major=15);arch=[x86_64]"
    )
    prompt.metadata.connect_targets.assert_called_once_with()


def test_add_host_in_automatic_mode_switches_to_manual(mock_config):
    """Running add_host while in automatic mode switches to the manual workflow."""
    mock_config.auto = True
    mock_config.kernel = False
    prompt = _prompt()
    args = Namespace(target=None, keep_mode=False)

    AddHost(args, mock_config, MagicMock(), prompt)()

    assert mock_config.auto is False
    assert mock_config.kernel is False
    # Prompt indicator refreshed (drops the "-auto" marker).
    prompt.set_prompt.assert_called_once_with(prompt.session)
    # The hosts are still added.
    prompt.metadata.connect_targets.assert_called_once_with()


def test_add_host_keep_mode_stays_automatic(mock_config):
    """--keep-mode leaves automatic mode untouched even though a host is added."""
    mock_config.auto = True
    mock_config.kernel = False
    prompt = _prompt()
    args = Namespace(target=None, keep_mode=True)

    AddHost(args, mock_config, MagicMock(), prompt)()

    assert mock_config.auto is True  # still automatic
    prompt.set_prompt.assert_not_called()
    # The hosts are still added.
    prompt.metadata.connect_targets.assert_called_once_with()


def test_add_host_prints_product_warnings_for_new_hosts(mock_config):
    """Product-drift warnings recorded during connect are echoed to stdout so
    MCP clients (which only see command stdout) can see them."""
    prompt = _prompt()
    prompt.metadata.targets = {}  # nothing connected yet
    prompt.metadata.product_warnings = {"h1": ["arch 'x86_64' != 'aarch64' (metadata)"]}
    args = Namespace(target=["h1"], keep_mode=False)

    fake_sys = MagicMock()
    fake_sys.stdout = StringIO()

    def _connect(*_a, **_k):
        # Simulate add_target connecting h1 between the before/after snapshot.
        prompt.metadata.targets["h1"] = MagicMock()

    with (
        patch("mtui.commands.addhost.concurrent.futures.ThreadPoolExecutor"),
        patch("mtui.commands.addhost.concurrent.futures.wait", side_effect=_connect),
    ):
        AddHost(args, mock_config, fake_sys, prompt)()

    output = fake_sys.stdout.getvalue()
    assert "WARNING: h1: arch 'x86_64' != 'aarch64' (metadata)" in output


def test_add_host_no_warnings_prints_nothing(mock_config):
    """A clean connect with no drift prints nothing extra."""
    prompt = _prompt()
    prompt.metadata.targets = {}
    prompt.metadata.product_warnings = {}
    args = Namespace(target=["h1"], keep_mode=False)

    fake_sys = MagicMock()
    fake_sys.stdout = StringIO()

    def _connect(*_a, **_k):
        prompt.metadata.targets["h1"] = MagicMock()

    with (
        patch("mtui.commands.addhost.concurrent.futures.ThreadPoolExecutor"),
        patch("mtui.commands.addhost.concurrent.futures.wait", side_effect=_connect),
    ):
        AddHost(args, mock_config, fake_sys, prompt)()

    assert fake_sys.stdout.getvalue() == ""


def test_add_host_in_manual_mode_does_not_switch(mock_config):
    """In manual mode add_host leaves the workflow untouched."""
    mock_config.auto = False
    prompt = _prompt()
    args = Namespace(target=None, keep_mode=False)

    AddHost(args, mock_config, MagicMock(), prompt)()

    prompt.set_prompt.assert_not_called()
