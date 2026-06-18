"""Tests for the SMELT query commands."""

from __future__ import annotations

import io
from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.smelt import SmeltUpdate, SmeltUpdates
from mtui.types import RequestKind


def _prompt_slfo() -> MagicMock:
    p = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.metadata.rrid = MagicMock()
    p.metadata.rrid.kind = RequestKind.SLFO
    p.metadata.rrid.review_id = "5426"
    p.metadata.giteapr = "https://src.suse.de/products/SLFO/pulls/5426"
    return p


def _sysmock():
    s = MagicMock()
    s.stdout = io.StringIO()
    return s


def test_smelt_update_slfo_prints_detail(mock_config):
    prompt = _prompt_slfo()
    sysmock = _sysmock()
    with patch("mtui.commands.smelt.Smelt") as cls:
        smelt = cls.return_value
        smelt.configured = True
        smelt.update.return_value = {
            "human_readable_id": "products/SLFO #5426",
            "status": "testing",
            "priority": 637,
            "deadline": "2026-07-06",
            "rating": {"name": "important"},
            "packages": [{"name": "python-aiohttp"}],
        }
        SmeltUpdate(Namespace(), mock_config, sysmock, prompt)()
    out = sysmock.stdout.getvalue()
    assert "priority : 637" in out
    assert "python-aiohttp" in out
    smelt.update.assert_called_once_with("src.suse.de:products:SLFO:5426")


def test_smelt_update_not_configured(mock_config):
    prompt = _prompt_slfo()
    sysmock = _sysmock()
    with patch("mtui.commands.smelt.Smelt") as cls:
        cls.return_value.configured = False
        SmeltUpdate(Namespace(), mock_config, sysmock, prompt)()
    assert "not configured" in sysmock.stdout.getvalue()


def test_smelt_updates_pending_filter(mock_config):
    sysmock = _sysmock()
    args = Namespace(
        status="testing", review_group=None, pending="qam-sle-review", limit=0
    )
    items = [
        {  # kept: testing + qam-sle-review not approved
            "human_readable_id": "products/SLFO #1",
            "status": "testing",
            "priority": 900,
            "packages": [{"name": "systemd"}],
            "reviews": [{"name": "qam-sle-review", "state": "REQUEST_REVIEW"}],
        },
        {  # dropped: qam-sle-review already approved
            "human_readable_id": "products/SLFO #2",
            "status": "testing",
            "priority": 800,
            "reviews": [{"name": "qam-sle-review", "state": "APPROVED"}],
        },
        {  # dropped: wrong status
            "human_readable_id": "products/SLFO #3",
            "status": "tested",
            "priority": 700,
            "reviews": [{"name": "qam-sle-review", "state": "REQUEST_REVIEW"}],
        },
    ]
    with patch("mtui.commands.smelt.Smelt") as cls:
        smelt = cls.return_value
        smelt.configured = True
        smelt.unreleased.return_value = items
        SmeltUpdates(args, mock_config, sysmock, MagicMock())()
    out = sysmock.stdout.getvalue()
    assert "products/SLFO #1" in out
    assert "products/SLFO #2" not in out
    assert "products/SLFO #3" not in out
    assert "1 update(s)" in out


def test_smelt_requests_pending_filter(mock_config):
    from mtui.commands.smelt import SmeltRequests

    sysmock = _sysmock()
    args = Namespace(group="qam-sle", pending=True, status=None, limit=0)
    nodes = [
        {  # kept: qam-sle review still 'new'
            "requestId": 1,
            "kind": "RR",
            "incident": {
                "incidentId": 100,
                "priority": 637,
                "packages": {"edges": [{"node": {"name": "libarchive"}}]},
            },
            "reviewSet": {
                "edges": [
                    {
                        "node": {
                            "status": {"name": "new"},
                            "assignedByGroup": {"name": "qam-sle"},
                            "assignedTo": None,
                        }
                    }
                ]
            },
        },
        {  # dropped: qam-sle review accepted
            "requestId": 2,
            "kind": "RR",
            "incident": {"incidentId": 101, "priority": 500, "packages": {"edges": []}},
            "reviewSet": {
                "edges": [
                    {
                        "node": {
                            "status": {"name": "accepted"},
                            "assignedByGroup": {"name": "qam-sle"},
                            "assignedTo": {"username": "mdonis"},
                        }
                    }
                ]
            },
        },
    ]
    with patch("mtui.commands.smelt.Smelt") as cls:
        smelt = cls.return_value
        smelt.configured = True
        smelt.review_requests.return_value = nodes
        SmeltRequests(args, mock_config, sysmock, MagicMock())()
    out = sysmock.stdout.getvalue()
    assert "req 1" in out
    assert "libarchive" in out
    assert "req 2" not in out
    assert "1 request(s)" in out
