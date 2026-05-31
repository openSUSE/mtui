"""Tests for the `approve` command (BaseApiCall dispatch)."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.apicall import Approve, Assign, Reject, Unassign
from mtui.messages import TestReportNotLoadedError
from mtui.types import RequestKind, RequestReviewID


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    # MAINTENANCE -> _is_gitea_workflow returns False -> osc path
    p.metadata.rrid = MagicMock()
    p.metadata.rrid.kind = RequestKind.MAINTENANCE
    p.metadata.rrid.maintenance_id = "12345"
    p.metadata.rrid.review_id = "67890"
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_approve_osc_branch_calls_osc_approve(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="")

    with patch("mtui.commands.apicall.OSC") as osc_cls:
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.assert_called_once_with(mock_config, prompt.metadata.rrid)
    osc_cls.return_value.approve.assert_called_once_with(["qam-sle"])


def test_approve_without_metadata_raises(mock_config):
    prompt = _prompt()
    prompt.metadata.__bool__ = lambda self: False
    args = Namespace(group=None, user="")

    with pytest.raises(TestReportNotLoadedError):
        Approve(args, mock_config, MagicMock(), prompt)()


# ---------------------------------------------------------------------------
# PI auto-lock around assign / end-of-testing
# ---------------------------------------------------------------------------


def _pi_prompt() -> MagicMock:
    p = _prompt()
    # A real RRID so kind is PI and str(rrid) renders the lock comment.
    p.metadata.rrid = RequestReviewID("SUSE:PI:34556:1")
    return p


def test_assign_pi_locks_refhosts(mock_config):
    mock_config.lock_pi_autolock = True
    prompt = _pi_prompt()
    args = Namespace(group=["qam-sle"], user="", force=False)

    with patch("mtui.commands.apicall.OSC") as osc_cls:
        Assign(args, mock_config, MagicMock(), prompt)()

    osc_cls.return_value.assign.assert_called_once_with(["qam-sle"])
    prompt.targets.lock.assert_called_once_with("testing of SUSE:PI:34556:1")
    assert prompt.metadata.lock_comment == "testing of SUSE:PI:34556:1"


def test_assign_pi_autolock_disabled(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _pi_prompt()
    args = Namespace(group=["qam-sle"], user="", force=False)

    with patch("mtui.commands.apicall.OSC"):
        Assign(args, mock_config, MagicMock(), prompt)()

    prompt.targets.lock.assert_not_called()


def test_assign_non_pi_does_not_lock(mock_config):
    mock_config.lock_pi_autolock = True
    prompt = _prompt()  # MAINTENANCE kind
    args = Namespace(group=["qam-sle"], user="", force=False)

    with patch("mtui.commands.apicall.OSC"):
        Assign(args, mock_config, MagicMock(), prompt)()

    prompt.targets.lock.assert_not_called()


@pytest.mark.parametrize(
    ("cls", "extra"),
    [
        (Approve, {}),
        (Unassign, {}),
        (Reject, {"reason": "admin", "message": ["nope"]}),
    ],
)
def test_end_of_testing_pi_unlocks(mock_config, cls, extra):
    mock_config.lock_pi_autolock = True
    prompt = _pi_prompt()
    prompt.metadata.lock_comment = "testing of SUSE:PI:34556:1"
    args = Namespace(group=["qam-sle"], user="", **extra)

    with patch("mtui.commands.apicall.OSC"):
        cls(args, mock_config, MagicMock(), prompt)()

    prompt.targets.unlock.assert_called_once_with()
    assert prompt.metadata.lock_comment == ""
