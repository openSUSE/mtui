"""Tests for the `list_hosts`, `list_locks`, and `list_metadata` commands."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.simplelists import ListHosts, ListLocks, ListMetadata
from mtui.support.messages import TestReportNotLoadedError


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_list_hosts_calls_report_self(mock_config):
    prompt = _prompt()
    ListHosts(Namespace(), mock_config, MagicMock(), prompt)()
    prompt.targets.report_self.assert_called_once_with(prompt.display.list_host)


def test_list_locks_filters_enabled_then_reports(mock_config):
    prompt = _prompt()
    ListLocks(Namespace(), mock_config, MagicMock(), prompt)()
    prompt.targets.select.assert_called_once_with(enabled=True)
    prompt.targets.select.return_value.report_locks.assert_called_once_with(
        prompt.display.list_locks
    )


def test_list_metadata_happy(mock_config):
    prompt = _prompt()
    sys_mock = MagicMock()
    ListMetadata(Namespace(), mock_config, sys_mock, prompt)()
    prompt.metadata.show_yourself.assert_called_once_with(sys_mock.stdout)


def test_list_metadata_without_metadata_raises(mock_config):
    prompt = _prompt()
    prompt.metadata.__bool__ = lambda self: False
    with pytest.raises(TestReportNotLoadedError):
        ListMetadata(Namespace(), mock_config, MagicMock(), prompt)()
