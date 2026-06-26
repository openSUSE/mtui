"""Tests for the ``updates`` command (TeReGen-backed update queue)."""

from __future__ import annotations

import io
from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.updates import Updates


def _sysmock() -> MagicMock:
    s = MagicMock()
    s.stdout = io.StringIO()
    return s


def _args(**kw) -> Namespace:
    base = {"review_group": None, "status": None, "limit": 0}
    base.update(kw)
    return Namespace(**base)


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

    teregen.updates.assert_called_once_with(review_group="qam-sle", status=None)
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
