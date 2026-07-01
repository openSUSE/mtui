"""Tests for the API-call commands (BaseApiCall dispatch and PI auto-lock)."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.apicall import Assign, Comment, Reject, Unassign
from mtui.types import RequestKind, RequestReviewID

# The update under test; the reject gate binds the Slack marker to this RRID
# by requiring the review-request parent message to name it verbatim.
_RRID = "SUSE:Maintenance:12345:67890"


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    # MAINTENANCE -> _is_gitea_workflow returns False -> osc path
    p.metadata.rrid = MagicMock()
    p.metadata.rrid.kind = RequestKind.MAINTENANCE
    p.metadata.rrid.maintenance_id = "12345"
    p.metadata.rrid.review_id = "67890"
    p.metadata.rrid.__str__.return_value = _RRID
    # A persisted Slack review so the approve/reject gate re-queries live
    # reactions rather than refusing outright (only Reject consults it here).
    p.metadata.get_slack_review.return_value = ("C123", "111.222")
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


# A 👍 reaction as Slack message objects carry it (message.reactions list).
_THUMBSUP = [{"name": "thumbsup", "count": 1, "users": ["Ureviewer"]}]


def _slack(rrid: str = _RRID):
    """Patch the reject gate's ``SlackClient`` so it sees a live, bound 👍 ack.

    The gate fetches the review-request parent message via
    ``conversations_replies`` and requires its text to name this update's
    RRID before trusting the parent's ``reactions``; pass ``rrid`` when the
    test's prompt carries a different RRID.

    ``require_slack_review`` imports ``SlackClient`` lazily from
    ``mtui.data_sources``, so that is the patch target.
    """
    client = MagicMock()
    client.conversations_replies.return_value = [
        {"text": f"Please review {rrid}: https://qam.suse.de/x", "reactions": _THUMBSUP}
    ]
    return patch("mtui.data_sources.SlackClient", return_value=client)


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

    with patch("mtui.commands.apicall.OSC"), _slack(rrid="SUSE:PI:34556:1"):
        cls(args, mock_config, MagicMock(), prompt)()

    prompt.targets.unlock.assert_called_once_with()
    assert prompt.metadata.lock_comment == ""


# ---------------------------------------------------------------------------
# reject joins its REMAINDER --message into a single string before sending it.
# A bare list reaches shlex.quote() in oscqam.__operation and aborts the reject
# with TypeError("expected string or bytes-like object, got 'list'").
# ---------------------------------------------------------------------------


def test_reject_osc_joins_message_into_string(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _prompt()  # MAINTENANCE -> osc path
    args = Namespace(
        group=["qam-sle"],
        reason="build_problem",
        message=["dependency", "issues", "bsc#1234"],
        user="",
    )

    with patch("mtui.commands.apicall.OSC") as osc_cls, _slack():
        Reject(args, mock_config, MagicMock(), prompt)()

    osc_cls.return_value.reject.assert_called_once_with(
        ["qam-sle"], "build_problem", "dependency issues bsc#1234"
    )


def test_reject_gitea_joins_message_into_string(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _prompt()
    prompt.metadata.rrid.kind = RequestKind.SLFO  # -> gitea path
    args = Namespace(
        group=["qam-sle"],
        reason="build_problem",
        message=["dependency", "issues"],
        user="someone",
    )

    with patch("mtui.commands.apicall.Gitea") as gitea_cls, _slack():
        Reject(args, mock_config, MagicMock(), prompt)()

    gitea_cls.return_value.reject.assert_called_once_with(
        "build_problem", "someone", "dependency issues"
    )


def test_reject_without_message_sends_empty_string(mock_config):
    mock_config.lock_pi_autolock = False
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], reason="admin", message=None, user="")

    with patch("mtui.commands.apicall.OSC") as osc_cls, _slack():
        Reject(args, mock_config, MagicMock(), prompt)()

    osc_cls.return_value.reject.assert_called_once_with(["qam-sle"], "admin", "")


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


# ---------------------------------------------------------------------------
# assign surfaces SMELT priority + deadline
# ---------------------------------------------------------------------------


def _sysmock():
    import io

    s = MagicMock()
    s.stdout = io.StringIO()
    return s


def test_assign_prefers_teregen_priority_deadline(mock_config):
    """TeReGen (the report API) is the source of truth; SMELT isn't consulted."""
    prompt = _prompt()
    prompt.metadata.giteapr = None
    args = Namespace(group=["qam-sle"], user="", force=False)
    sysmock = _sysmock()

    with (
        patch("mtui.commands.apicall.OSC"),
        patch("mtui.commands.apicall.TeReGen") as teregen_cls,
    ):
        teregen = teregen_cls.return_value
        teregen.priority_deadline.return_value = (700, "2026-07-09")
        Assign(args, mock_config, sysmock, prompt)()

    teregen.priority_deadline.assert_called_once()
    out = sysmock.stdout.getvalue()
    assert "TeReGen: priority 700" in out
    assert "2026-07-09" in out


def test_assign_silent_when_teregen_has_nothing(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="", force=False)
    sysmock = _sysmock()

    with (
        patch("mtui.commands.apicall.OSC"),
        patch("mtui.commands.apicall.TeReGen") as teregen_cls,
    ):
        teregen_cls.return_value.priority_deadline.return_value = (None, None)
        Assign(args, mock_config, sysmock, prompt)()

    out = sysmock.stdout.getvalue()
    assert "TeReGen" not in out


def test_unassign_does_not_show_priority_deadline(mock_config):
    """Only assign surfaces priority/deadline; other api-calls don't."""
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="")
    sysmock = _sysmock()

    with (
        patch("mtui.commands.apicall.OSC"),
        patch("mtui.commands.apicall.TeReGen") as teregen_cls,
    ):
        Unassign(args, mock_config, sysmock, prompt)()

    teregen_cls.return_value.priority_deadline.assert_not_called()
