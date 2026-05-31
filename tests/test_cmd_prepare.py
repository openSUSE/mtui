"""Tests for the `prepare` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.prepare import Prepare
from mtui.hosts.target.hostgroup import HostsGroup
from mtui.support.messages import NoRefhostsDefinedError


def _prompt(hg) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.display = MagicMock()
    p.targets = hg
    return p


def test_prepare_happy_calls_perform_prepare(mock_config):
    t = MagicMock()
    t.hostname = "h1"
    t.state = "enabled"
    prompt = _prompt(HostsGroup([t]))  # ty: ignore[invalid-argument-type]
    args = Namespace(force=None, installed=None, update=None, hosts=None)

    Prepare(args, mock_config, MagicMock(), prompt)()

    prompt.metadata.perform_prepare.assert_called_once()
    kwargs = prompt.metadata.perform_prepare.call_args.kwargs
    assert kwargs == {"force": False, "installed_only": False, "testing": False}


def test_prepare_empty_targets_raises(mock_config):
    prompt = _prompt(HostsGroup([]))
    args = Namespace(force=None, installed=None, update=None, hosts=None)
    with pytest.raises(NoRefhostsDefinedError):
        Prepare(args, mock_config, MagicMock(), prompt)()
