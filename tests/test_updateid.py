"""Tests for `UpdateID._checkout` hash-mismatch handling.

Regression coverage for commit cc2147a, which:

- Renames the force-continue prompt label from ``[Y/N]`` to ``[y/N]`` so the
  visible default matches the actual behaviour ("no").
- On the abort branch (user declines force-continue), warns about the
  partially-checked-out template directory that must be removed before
  relaunching MTUI -- but only if the directory actually exists, and only
  when the user did *not* force-continue.
"""

import logging
from unittest.mock import MagicMock, patch

import pytest

from mtui.exceptions import InvalidGiteaHashError
from mtui.messages import TestReportNotLoadedError
from mtui.types.updateid import AutoOBSUpdateID

RRID = "SUSE:Maintenance:12358:199773"
PROMPT_LABEL = "Force continue loading template ? [y/N]: "
CLEANUP_HINT = "Make sure to remove"


def _make_updateid() -> tuple[AutoOBSUpdateID, MagicMock]:
    """Build an `AutoOBSUpdateID` whose testreport read raises hash mismatch.

    The factory is replaced post-construction so the production constructor
    wiring (`tr_factory`, real `testreport_svn_checkout`) still runs, but the
    `_checkout` call path is steered into the `InvalidGiteaHashError` branch
    without touching the network or filesystem-via-svn.

    Returns the update id and the test-report mock so tests can assert
    identity on the returned `TestReport`.
    """
    uid = AutoOBSUpdateID(RRID)
    tr_mock = MagicMock(name="testreport")
    tr_mock.read.side_effect = InvalidGiteaHashError(uid.id, "oldhash", "newhash")
    uid.testreport_factory = MagicMock(return_value=tr_mock)  # ty: ignore[invalid-assignment]
    return uid, tr_mock


def test_checkout_invalid_hash_abort_warns_when_trdir_exists(
    mock_config, tmp_path, caplog
):
    """Abort + existing trdir -> cleanup hint logged, prompt uses [y/N]."""
    mock_config.template_dir = tmp_path
    uid, _tr_mock = _make_updateid()
    trdir = tmp_path / str(uid.id)
    trdir.mkdir(parents=True)

    with (
        patch(
            "mtui.types.updateid.prompt_user", return_value=False
        ) as prompt_user_mock,
        caplog.at_level(logging.WARNING, logger="mtui.types.updateid"),
        pytest.raises(TestReportNotLoadedError),
    ):
        uid._checkout(mock_config, interactive=True)

    # Prompt label change is locked in here.
    prompt_user_mock.assert_called_once()
    assert prompt_user_mock.call_args.args[0] == PROMPT_LABEL

    cleanup_warnings = [r for r in caplog.records if CLEANUP_HINT in r.getMessage()]
    assert len(cleanup_warnings) == 1
    assert str(trdir) in cleanup_warnings[0].getMessage()


def test_checkout_invalid_hash_abort_silent_when_trdir_missing(
    mock_config, tmp_path, caplog
):
    """Abort + missing trdir -> still raises, but no cleanup hint."""
    mock_config.template_dir = tmp_path
    uid, _tr_mock = _make_updateid()
    # Deliberately do not create tmp_path / str(uid.id).
    assert not (tmp_path / str(uid.id)).exists()

    with (
        patch("mtui.types.updateid.prompt_user", return_value=False),
        caplog.at_level(logging.WARNING, logger="mtui.types.updateid"),
        pytest.raises(TestReportNotLoadedError),
    ):
        uid._checkout(mock_config, interactive=True)

    assert not any(CLEANUP_HINT in r.getMessage() for r in caplog.records)


def test_checkout_invalid_hash_force_continue_no_cleanup_warning(
    mock_config, tmp_path, caplog
):
    """Force-continue suppresses the cleanup hint even if trdir exists."""
    mock_config.template_dir = tmp_path
    uid, tr_mock = _make_updateid()
    trdir = tmp_path / str(uid.id)
    trdir.mkdir(parents=True)

    with (
        patch("mtui.types.updateid.prompt_user", return_value=True),
        caplog.at_level(logging.WARNING, logger="mtui.types.updateid"),
    ):
        tr = uid._checkout(mock_config, interactive=True)

    # Returns the same testreport instance produced by the patched factory.
    assert tr is tr_mock
    messages = [r.getMessage() for r in caplog.records]
    assert any("Template is loaded, but hash differs" in m for m in messages)
    assert not any(CLEANUP_HINT in m for m in messages)
