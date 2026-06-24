"""Tests for the `list_hosts`, `list_locks`, and `list_metadata` commands."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.simplelists import (
    ListBugs,
    ListHosts,
    ListLocks,
    ListMetadata,
    ListUpdateCommands,
    ListVersions,
)
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
    ListLocks(Namespace(pool=False), mock_config, MagicMock(), prompt)()
    prompt.targets.select.assert_called_once_with(enabled=True)
    prompt.targets.select.return_value.report_locks.assert_called_once_with(
        prompt.display.list_locks, pool=False
    )


def test_list_locks_pool_flag_reports_pool(mock_config):
    prompt = _prompt()
    ListLocks(Namespace(pool=True), mock_config, MagicMock(), prompt)()
    prompt.targets.select.return_value.report_locks.assert_called_once_with(
        prompt.display.list_locks, pool=True
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


# --- fan-out scope contract for report-bound list/show commands ---

_REPORT_BOUND = (ListMetadata, ListBugs, ListUpdateCommands, ListVersions)


@pytest.mark.parametrize("cmd_cls", _REPORT_BOUND)
def test_report_bound_command_is_fanout(cmd_cls):
    assert cmd_cls.scope == "fanout"


@pytest.mark.parametrize("cmd_cls", _REPORT_BOUND)
def test_report_bound_command_accepts_template_flag(cmd_cls):
    sys_mock = MagicMock()
    ns = cmd_cls.parse_args("-T SUSE:Maintenance:1:1", sys_mock)
    assert ns.template == "SUSE:Maintenance:1:1"
    assert ns.all_templates is False


@pytest.mark.parametrize("cmd_cls", _REPORT_BOUND)
def test_report_bound_command_accepts_all_templates_flag(cmd_cls):
    sys_mock = MagicMock()
    ns = cmd_cls.parse_args("--all-templates", sys_mock)
    assert ns.all_templates is True
    assert ns.template is None
