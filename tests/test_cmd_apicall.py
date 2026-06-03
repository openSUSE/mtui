"""Tests for the API-call commands (BaseApiCall dispatch and PI auto-lock)."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.apicall import Assign, Comment, Reject, Unassign
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


# ---------------------------------------------------------------------------
# Comment reads the body through ask_user (not the bare input() that
# would block / echo ^M between two prompt_toolkit sessions)
# ---------------------------------------------------------------------------


def test_comment_osc_reads_body_through_ask_user(mock_config):
    """The OSC ``comment`` path must route the body read through ``ask_user``.

    A regression to the bare ``input()`` call would hang the REPL on a
    real TTY (see the prompt_toolkit ↔ cooked-mode interaction fixed
    alongside ``prompt_user``).
    """
    mock_config.lock_pi_autolock = False
    prompt = _prompt()  # MAINTENANCE kind -> osc() path
    args = Namespace(group=None, user="")

    with (
        patch("mtui.commands.apicall.OSC") as osc_cls,
        patch("mtui.commands.apicall.ask_user", return_value="looks good") as ask,
    ):
        Comment(args, mock_config, MagicMock(), prompt)()

    ask.assert_called_once_with("Comment: ")
    osc_cls.return_value.comment.assert_called_once_with("looks good")


def test_comment_gitea_reads_body_through_ask_user(mock_config):
    """The Gitea ``comment`` path must route the body read through ``ask_user``."""
    mock_config.lock_pi_autolock = False
    prompt = _prompt()
    # Force the Gitea branch: SLFO + non-1.1 maintenance_id.
    prompt.metadata.rrid.kind = RequestKind.SLFO
    prompt.metadata.rrid.maintenance_id = "55.1"
    prompt.metadata.giteaprapi = MagicMock()
    args = Namespace(group=None, user="")

    with (
        patch("mtui.commands.apicall.Gitea") as gitea_cls,
        patch("mtui.commands.apicall.ask_user", return_value="ack") as ask,
    ):
        Comment(args, mock_config, MagicMock(), prompt)()

    ask.assert_called_once_with("Comment: ")
    gitea_cls.return_value.comment.assert_called_once_with("ack")
