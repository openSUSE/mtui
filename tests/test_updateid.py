"""Tests for `UpdateID._checkout` hash-mismatch handling.

Regression coverage for commit cc2147a, which:

- Renames the force-continue prompt label from ``[Y/N]`` to ``[y/N]`` so the
  visible default matches the actual behaviour ("no").
- On the abort branch (user declines force-continue), warns about the
  partially-checked-out template directory that must be removed before
  relaunching MTUI -- but only if the directory actually exists, and only
  when the user did *not* force-continue.

Also covers the post-SVN-checkout retry path: Gitea errors raised by the
*second* ``tr.read()`` (after a successful checkout populates the template
on disk) must reach the same outer except clauses as errors raised by the
first read. Previously, a nested ``try/except Exception: raise`` inside
the SVN-checkout branch made these errors bypass the outer handlers and
crash mtui with an uncaught traceback.
"""

import logging
from errno import ENOENT
from unittest.mock import MagicMock, patch

import pytest

from mtui.support.exceptions import (
    FailedGiteaCallError,
    InvalidGiteaHashError,
    MissingGiteaTokenError,
)
from mtui.support.messages import TestReportNotLoadedError
from mtui.test_reports.svn_io import TemplateIOError
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
    uid.testreport_factory = MagicMock(return_value=tr_mock)
    return uid, tr_mock


def _make_updateid_with_checkout_retry(
    second_read_error: Exception,
) -> tuple[AutoOBSUpdateID, MagicMock, MagicMock]:
    """Build an `AutoOBSUpdateID` that exercises the post-SVN-checkout retry.

    The first ``tr.read()`` raises ``TemplateIOError(ENOENT, ...)`` (simulating
    a not-yet-checked-out template), the mocked ``_vcs_checkout`` succeeds,
    and the second ``tr.read()`` raises ``second_read_error``. This is the
    code path that previously bypassed the outer Gitea except clauses.

    Returns ``(update_id, testreport_mock, vcs_checkout_mock)``.
    """
    uid = AutoOBSUpdateID(RRID)
    tr_mock = MagicMock(name="testreport")
    enoent = TemplateIOError(ENOENT, "No such file or directory", "log")
    tr_mock.read.side_effect = [enoent, second_read_error]
    uid.testreport_factory = MagicMock(return_value=tr_mock)
    vcs_mock = MagicMock(name="vcs_checkout")
    uid._vcs_checkout = vcs_mock  # noqa: SLF001
    return uid, tr_mock, vcs_mock


def test_checkout_invalid_hash_abort_declining_delete_keeps_trdir(
    mock_config, tmp_path
):
    """Abort + decline delete -> trdir kept, no further action."""
    mock_config.template_dir = tmp_path
    uid, _tr_mock = _make_updateid()
    trdir = tmp_path / str(uid.id)
    trdir.mkdir(parents=True)

    with (
        patch(
            "mtui.types.updateid.prompt_user", return_value=False
        ) as prompt_user_mock,
        pytest.raises(TestReportNotLoadedError),
    ):
        uid._checkout(mock_config, interactive=True)

    # Two prompts: force-continue, then (since trdir exists) delete.
    assert prompt_user_mock.call_count == 2
    assert prompt_user_mock.call_args_list[0].args[0] == PROMPT_LABEL
    delete_call = prompt_user_mock.call_args_list[1]
    assert delete_call.args[0] == f"Delete checked out template {trdir}? [Y/n]: "
    assert delete_call.kwargs["default"] is True
    assert trdir.exists()  # declined -> not removed


def test_checkout_invalid_hash_abort_accepting_delete_removes_trdir(
    mock_config, tmp_path, caplog
):
    """Abort + accept delete -> trdir removed, no cleanup hint."""
    mock_config.template_dir = tmp_path
    uid, _tr_mock = _make_updateid()
    trdir = tmp_path / str(uid.id)
    trdir.mkdir(parents=True)

    with (
        # First prompt (force-continue) -> False, second (delete) -> True.
        patch("mtui.types.updateid.prompt_user", side_effect=[False, True]),
        caplog.at_level(logging.INFO, logger="mtui.types.updateid"),
        pytest.raises(TestReportNotLoadedError),
    ):
        uid._checkout(mock_config, interactive=True)

    assert not trdir.exists()  # removed
    messages = [r.getMessage() for r in caplog.records]
    assert any("Removed checked out template" in m for m in messages)
    assert not any(CLEANUP_HINT in m for m in messages)


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


# --- Post-SVN-checkout retry: Gitea errors raised by the second tr.read() ---
#
# These guard the regression where a nested ``try/except Exception: raise``
# inside the SVN-checkout branch let MissingGiteaTokenError, FailedGiteaCallError,
# and InvalidGiteaHashError raised by the post-checkout read bypass the
# outer except clauses and crash mtui with an uncaught traceback.


def test_checkout_missing_token_after_svn_checkout_propagates(
    mock_config, tmp_path, caplog
):
    """Missing GITEA_TOKEN after a successful SVN checkout is a hard failure.

    The error must propagate out of ``_checkout`` (caller decides exit code)
    and a user-facing "token is not configured" message must be logged.
    """
    mock_config.template_dir = tmp_path
    uid, _tr_mock, vcs_mock = _make_updateid_with_checkout_retry(
        MissingGiteaTokenError("Gitea API token is empty, can't access API")
    )

    with (
        caplog.at_level(logging.ERROR, logger="mtui.types.updateid"),
        pytest.raises(MissingGiteaTokenError),
    ):
        uid._checkout(mock_config, interactive=True)  # noqa: SLF001

    vcs_mock.assert_called_once()
    error_messages = [r.getMessage() for r in caplog.records]
    assert any("Gitea API token is not configured" in m for m in error_messages), (
        f"expected configuration-hint error, got {error_messages!r}"
    )


def test_checkout_failed_gitea_call_after_svn_checkout_converts(
    mock_config, tmp_path, caplog
):
    """A Gitea API failure after SVN checkout becomes TestReportNotLoadedError.

    Same soft-fail behaviour as a Gitea failure on the first read: callers
    can convert this into a NullTestReport instead of crashing.
    """
    mock_config.template_dir = tmp_path
    uid, _tr_mock, vcs_mock = _make_updateid_with_checkout_retry(
        FailedGiteaCallError("GET - https://example.invalid returned status 500")
    )

    with (
        caplog.at_level(logging.WARNING, logger="mtui.types.updateid"),
        pytest.raises(TestReportNotLoadedError),
    ):
        uid._checkout(mock_config, interactive=True)  # noqa: SLF001

    vcs_mock.assert_called_once()
    messages = [r.getMessage() for r in caplog.records]
    assert any("TestReport isn't loaded" in m for m in messages)


def test_checkout_invalid_hash_after_svn_checkout_prompts(
    mock_config, tmp_path, caplog
):
    """An InvalidGiteaHashError after SVN checkout still triggers the prompt.

    Locks in that the post-checkout retry path shares the same hash-mismatch
    handling (prompt, cleanup hint, force-continue support) as the first read.
    """
    mock_config.template_dir = tmp_path
    rrid_obj = AutoOBSUpdateID(RRID).id
    uid, _tr_mock, vcs_mock = _make_updateid_with_checkout_retry(
        InvalidGiteaHashError(rrid_obj, "oldhash", "newhash")
    )
    # The post-checkout cleanup hint only fires when trdir exists.
    trdir = tmp_path / str(uid.id)
    trdir.mkdir(parents=True)

    with (
        patch(
            "mtui.types.updateid.prompt_user", return_value=False
        ) as prompt_user_mock,
        caplog.at_level(logging.WARNING, logger="mtui.types.updateid"),
        pytest.raises(TestReportNotLoadedError),
    ):
        uid._checkout(mock_config, interactive=True)  # noqa: SLF001

    vcs_mock.assert_called_once()
    # Two prompts: force-continue, then delete (trdir exists, both declined).
    assert prompt_user_mock.call_count == 2
    assert prompt_user_mock.call_args_list[0].args[0] == PROMPT_LABEL
    assert not any(CLEANUP_HINT in r.getMessage() for r in caplog.records)
