"""Tests for the `downgrade` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.downgrade import Downgrade
from mtui.support.messages import NoRefhostsDefinedError, TestReportNotLoadedError
from mtui.target.hostgroup import HostsGroup


def _target(hostname="h1", state="enabled"):
    t = MagicMock()
    t.hostname = hostname
    t.state = state
    t.packages = {}
    return t


def _prompt(targets) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.display = MagicMock()
    p.targets = targets
    return p


def test_downgrade_happy_calls_perform_downgrade(mock_config):
    t1 = _target("h1")
    hg = HostsGroup([t1])
    prompt = _prompt(hg)
    args = Namespace(hosts=None)

    Downgrade(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.perform_downgrade.assert_called_once()


def test_downgrade_empty_targets_raises(mock_config):
    prompt = _prompt(HostsGroup([]))
    args = Namespace(hosts=None)

    with pytest.raises(NoRefhostsDefinedError):
        Downgrade(args, mock_config, MagicMock(), prompt)()


def test_downgrade_without_metadata_raises(mock_config):
    prompt = _prompt(HostsGroup([]))
    prompt.metadata.__bool__ = lambda self: False
    with pytest.raises(TestReportNotLoadedError):
        Downgrade(Namespace(hosts=None), mock_config, MagicMock(), prompt)()
