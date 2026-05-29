"""Tests for the `remove_host` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.removehost import RemoveHost
from mtui.messages import HostIsNotConnectedError
from mtui.target.hostgroup import HostsGroup


def _target(hostname):
    t = MagicMock()
    t.hostname = hostname
    t.state = "enabled"
    return t


def _prompt(hg, systems) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.systems = systems
    p.display = MagicMock()
    p.targets = hg
    return p


def test_remove_host_happy_closes_and_pops(mock_config):
    t = _target("h1")
    hg = HostsGroup([t])
    systems = {"h1": MagicMock()}
    prompt = _prompt(hg, systems)
    args = Namespace(hosts=None)

    # Run executor.submit callables inline so the test does not depend on threads.
    def fake_submit(fn, *a, **kw):
        fn(*a, **kw)
        return MagicMock()

    with (
        patch("mtui.commands.removehost.concurrent.futures.ThreadPoolExecutor") as tpe,
        patch("mtui.commands.removehost.concurrent.futures.wait"),
    ):
        executor = MagicMock()
        executor.submit.side_effect = fake_submit
        tpe.return_value.__enter__.return_value = executor

        RemoveHost(args, mock_config, MagicMock(), prompt)()

    t.close.assert_called_once_with()
    assert "h1" not in hg
    assert "h1" not in systems


def test_remove_host_unknown_propagates(mock_config):
    bad_targets = MagicMock()
    bad_targets.select.side_effect = HostIsNotConnectedError("ghost")
    prompt = _prompt(bad_targets, {})
    args = Namespace(hosts=["ghost"])
    with pytest.raises(HostIsNotConnectedError):
        RemoveHost(args, mock_config, MagicMock(), prompt)()
