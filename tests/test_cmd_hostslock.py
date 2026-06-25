"""Tests for the `lock` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

from mtui.commands.hostslock import HostLock


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    targets = MagicMock()
    selected = MagicMock()
    targets.select.return_value = selected
    p.targets = targets
    return p


def test_lock_happy_passes_joined_comment(mock_config):
    prompt = _prompt()
    args = Namespace(hosts=None, comment=[["my", "lock"]])
    HostLock(args, mock_config, MagicMock(), prompt)()
    prompt.targets.select.return_value.lock.assert_called_once_with("my lock")


def test_lock_without_comment_passes_empty_string(mock_config):
    prompt = _prompt()
    args = Namespace(hosts=None, comment=None)
    HostLock(args, mock_config, MagicMock(), prompt)()
    prompt.targets.select.return_value.lock.assert_called_once_with("")


def test_lock_is_fanout():
    assert HostLock.scope == "fanout"


def test_lock_accepts_template_flag():
    ns = HostLock.parse_args("-T SUSE:Maintenance:1:1", MagicMock())
    assert ns.template == "SUSE:Maintenance:1:1"
    assert ns.all_templates is False


def test_lock_accepts_all_templates_flag():
    ns = HostLock.parse_args("--all-templates", MagicMock())
    assert ns.all_templates is True
    assert ns.template is None
