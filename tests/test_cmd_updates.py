"""Tests for the ``updates`` command (TeReGen-backed update queue)."""

from __future__ import annotations

import io
from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.cli.argparse import ArgsParseFailureError, ArgumentParser
from mtui.commands.updates import Updates


def _sysmock() -> MagicMock:
    s = MagicMock()
    s.stdout = io.StringIO()
    return s


def _args(**kw) -> Namespace:
    base = {
        "review_group": None,
        "status": "testing",
        "limit": 0,
        "assignee": None,
        "mine": False,
        "all_assignees": False,
    }
    base.update(kw)
    return Namespace(**base)


def _parser() -> ArgumentParser:
    parser = ArgumentParser(prog="updates", sys_=_sysmock())
    Updates._add_arguments(parser)
    return parser


def test_updates_lists_queue(mock_config):
    sysmock = _sysmock()
    with patch("mtui.commands.updates.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.updates.return_value = [
            {
                "id": "SUSE:Maintenance:43000:405000",
                "kind": "Maintenance",
                "priority": 900,
                "status": "testing",
                "deadline": "2026-05-01T00:00:00Z",
            },
            {
                "id": "SUSE:SLFO:1.2:5444",
                "kind": "SLFO",
                "priority": 653,
                "status": "new",
                "deadline": "2026-07-10T05:50:21Z",
            },
        ]
        Updates(_args(review_group="qam-sle"), mock_config, sysmock, MagicMock())()

    teregen.updates.assert_called_once_with(
        review_group="qam-sle",
        status="testing",
        assignee=None,
        unassigned=True,
        with_assignment=True,
    )
    out = sysmock.stdout.getvalue()
    assert "SUSE:SLFO:1.2:5444" in out
    assert "653" in out
    # kind and deadline (date) are surfaced
    assert "Maintenance" in out
    assert "SLFO" in out
    assert "2026-05-01" in out


def test_updates_limit(mock_config):
    sysmock = _sysmock()
    with patch("mtui.commands.updates.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.updates.return_value = [{"id": "a"}, {"id": "b"}, {"id": "c"}]
        Updates(_args(limit=1), mock_config, sysmock, MagicMock())()

    out = sysmock.stdout.getvalue()
    assert "Update queue (1)" in out


def test_updates_all_assignees_renders_assignee(mock_config):
    sysmock = _sysmock()
    with patch("mtui.commands.updates.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.updates.return_value = [
            {"id": "SUSE:SLFO:1.2:1", "assignee": "alice"},
            {"id": "SUSE:SLFO:1.2:2", "assignee": None},
        ]
        Updates(_args(all_assignees=True), mock_config, sysmock, MagicMock())()

    teregen.updates.assert_called_once_with(
        review_group=None,
        status="testing",
        assignee=None,
        unassigned=False,
        with_assignment=True,
    )
    out = sysmock.stdout.getvalue()
    assert "assignee=alice" in out
    assert "assignee=unassigned" in out


def test_updates_mine_maps_to_session_user(mock_config):
    sysmock = _sysmock()
    with patch("mtui.commands.updates.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.updates.return_value = []
        Updates(_args(mine=True), mock_config, sysmock, MagicMock())()

    teregen.updates.assert_called_once_with(
        review_group=None,
        status="testing",
        assignee="testuser",
        unassigned=False,
        with_assignment=True,
    )


def test_updates_all_assignees_drops_unassigned_filter(mock_config):
    sysmock = _sysmock()
    with patch("mtui.commands.updates.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.updates.return_value = []
        Updates(_args(all_assignees=True), mock_config, sysmock, MagicMock())()

    teregen.updates.assert_called_once_with(
        review_group=None,
        status="testing",
        assignee=None,
        unassigned=False,
        with_assignment=True,
    )


def test_updates_default_is_unassigned_testing(mock_config):
    sysmock = _sysmock()
    with patch("mtui.commands.updates.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.updates.return_value = []
        Updates(_args(), mock_config, sysmock, MagicMock())()

    teregen.updates.assert_called_once_with(
        review_group=None,
        status="testing",
        assignee=None,
        unassigned=True,
        with_assignment=True,
    )


def test_updates_status_all_widens_to_full_queue(mock_config):
    sysmock = _sysmock()
    with patch("mtui.commands.updates.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.updates.return_value = []
        Updates(_args(status="all"), mock_config, sysmock, MagicMock())()

    teregen.updates.assert_called_once_with(
        review_group=None,
        status=None,
        assignee=None,
        unassigned=False,
        with_assignment=False,
    )


def test_updates_assignee_drops_unassigned_default(mock_config):
    sysmock = _sysmock()
    with patch("mtui.commands.updates.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.updates.return_value = []
        Updates(_args(assignee="bob"), mock_config, sysmock, MagicMock())()

    teregen.updates.assert_called_once_with(
        review_group=None,
        status="testing",
        assignee="bob",
        unassigned=False,
        with_assignment=True,
    )


def test_updates_mine_and_assignee_are_mutually_exclusive():
    with pytest.raises(ArgsParseFailureError):
        _parser().parse_args(["--mine", "--assignee", "bob"])


def test_updates_all_assignees_with_assignee_errors():
    with pytest.raises(ArgsParseFailureError):
        _parser().parse_args(["--all-assignees", "--assignee", "bob"])


def test_updates_mine_and_all_assignees_are_mutually_exclusive():
    with pytest.raises(ArgsParseFailureError):
        _parser().parse_args(["--mine", "--all-assignees"])
