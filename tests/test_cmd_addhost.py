"""Tests for the `add_host` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.addhost import AddHost


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.testplatforms = ["base=sles(major=15);arch=[x86_64]"]
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_add_host_with_explicit_targets_submits_per_host(mock_config):
    prompt = _prompt()
    args = Namespace(target=["h1", "h2"])

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
    args = Namespace(target=None)

    AddHost(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.refhosts_from_tp.assert_called_once_with(
        "base=sles(major=15);arch=[x86_64]"
    )
    prompt.metadata.connect_targets.assert_called_once_with()
