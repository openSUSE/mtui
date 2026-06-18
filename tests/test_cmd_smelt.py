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
        status="testing",
        review_group=None,
        pending="qam-sle-review",
        group="qam-sle",
        unassigned=False,
        show_assignment=False,
        limit=0,
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


# --- smelt_updates assignment filtering (--unassigned / --show-assignment) ---


def _uargs(**kw) -> Namespace:
    base = {
        "status": None,
        "review_group": None,
        "pending": None,
        "group": "qam-sle",
        "unassigned": False,
        "show_assignment": False,
        "limit": 0,
    }
    base.update(kw)
    return Namespace(**base)


def _assign_items() -> list[dict]:
    return [
        {
            "human_readable_id": "p #1",
            "status": "testing",
            "priority": 100,
            "external_url": "https://h/o/r/pulls/1",
            "packages": [{"name": "a"}],
        },
        {
            "human_readable_id": "p #2",
            "status": "testing",
            "priority": 90,
            "external_url": "https://h/o/r/pulls/2",
            "packages": [{"name": "b"}],
        },
        {
            "human_readable_id": "p #3",
            "status": "testing",
            "priority": 80,
            "external_url": "https://h/o/r/pulls/3",
            "packages": [{"name": "c"}],
        },
    ]


def _run_updates(cfg, args, assignees=None) -> str:
    """Run SmeltUpdates with Smelt.unreleased mocked; return the printed output.

    ``assignees`` maps external_url -> assignee (None = unassigned); when given,
    ``_assignee`` is stubbed to consult it instead of hitting Gitea.
    """
    sysmock = _sysmock()
    with patch("mtui.commands.smelt.Smelt") as cls:
        smelt = cls.return_value
        smelt.configured = True
        smelt.unreleased.return_value = _assign_items()
        if assignees is None:
            SmeltUpdates(args, cfg, sysmock, MagicMock())()
        else:

            def _fake_assignee(self, item):
                return assignees.get(item["external_url"])

            with patch.object(SmeltUpdates, "_assignee", _fake_assignee):
                SmeltUpdates(args, cfg, sysmock, MagicMock())()
    return sysmock.stdout.getvalue()


def test_plain_listing_has_no_assignee_column(mock_config):
    """Without the flags, no assignment work happens and no column is shown."""
    out = _run_updates(mock_config, _uargs())
    assert "#1" in out
    assert "#2" in out
    assert "#3" in out
    assert "unassigned" not in out
    assert "3 update(s)" in out


def test_unassigned_filters_out_assigned(mock_config):
    assignees = {
        "https://h/o/r/pulls/1": "alice",
        "https://h/o/r/pulls/2": None,
        "https://h/o/r/pulls/3": None,
    }
    out = _run_updates(mock_config, _uargs(unassigned=True), assignees)
    assert "#1" not in out
    assert "#2" in out
    assert "#3" in out
    assert "2 update(s)" in out


def test_unassigned_limit_one_is_lazy(mock_config):
    """--unassigned --limit 1 returns the top unassigned and stops probing."""
    assignees = {
        "https://h/o/r/pulls/1": "alice",
        "https://h/o/r/pulls/2": None,
        "https://h/o/r/pulls/3": None,
    }
    calls: list[str] = []
    sysmock = _sysmock()

    def _fake_assignee(self, item):
        calls.append(item["external_url"])
        return assignees.get(item["external_url"])

    with (
        patch("mtui.commands.smelt.Smelt") as cls,
        patch.object(SmeltUpdates, "_assignee", _fake_assignee),
    ):
        cls.return_value.configured = True
        cls.return_value.unreleased.return_value = _assign_items()
        SmeltUpdates(
            _uargs(unassigned=True, limit=1), mock_config, sysmock, MagicMock()
        )()
    out = sysmock.stdout.getvalue()
    assert "#2" in out
    assert "#1" not in out
    assert "#3" not in out
    assert "1 update(s)" in out
    # Stopped after the first unassigned: #3 was never probed.
    assert calls == ["https://h/o/r/pulls/1", "https://h/o/r/pulls/2"]


def test_show_assignment_adds_column(mock_config):
    assignees = {
        "https://h/o/r/pulls/1": "alice",
        "https://h/o/r/pulls/2": None,
        "https://h/o/r/pulls/3": "bob",
    }
    out = _run_updates(mock_config, _uargs(show_assignment=True), assignees)
    assert "alice" in out
    assert "bob" in out
    assert "unassigned" in out
    assert "3 update(s)" in out


def test_missing_token_skips_assignment(mock_config):
    """Without a Gitea token, --unassigned is ignored with a hint."""
    mock_config.gitea_token = ""
    out = _run_updates(mock_config, _uargs(unassigned=True), assignees={})
    assert "Gitea token" in out
    assert "3 update(s)" in out  # filter ignored, all rows shown


def test_assignee_uses_gitea_client(mock_config):
    """_assignee builds a Gitea client from external_url and returns assignee()."""
    sysmock = _sysmock()
    with (
        patch("mtui.commands.smelt.Smelt") as smelt_cls,
        patch("mtui.commands.smelt.Gitea") as gitea_cls,
    ):
        smelt_cls.return_value.configured = True
        smelt_cls.return_value.unreleased.return_value = _assign_items()
        gitea_cls.return_value.assignee.return_value = "carol"
        SmeltUpdates(
            _uargs(show_assignment=True, limit=1), mock_config, sysmock, MagicMock()
        )()
    out = sysmock.stdout.getvalue()
    assert "carol" in out
    # The client is constructed with the API URL derived from external_url.
    assert gitea_cls.call_args.args[1] == "https://h/api/v1/repos/o/r/pulls/1"
    assert gitea_cls.call_args.kwargs["group"] == "qam-sle"
