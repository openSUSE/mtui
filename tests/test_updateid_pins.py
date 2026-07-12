"""Mutation-killing pinning tests for ``mtui.types.updateid``.

A full mutmut run left survivors in ``UpdateID._checkout`` because the
existing tests patch ``testreport_factory``, ``_vcs_checkout``,
``prompt_user`` and ``_regenerate`` as MagicMocks without asserting the
call arguments, so argument-drop/reorder mutants pass unnoticed. These
tests pin:

- the exact arguments of ``testreport_factory``, ``tr.read``,
  ``_vcs_checkout`` and ``_regenerate``;
- the real ``prompt_user`` accept lists (``["yes", "y"]``) by patching
  the low-level ``_read_line`` instead of ``prompt_user`` itself;
- that non-interactive mode never reads input and honors the delete
  prompt's ``default=True``;
- ``shutil.rmtree(..., ignore_errors=True)`` on the accepted delete;
- ``UpdateID.tr_factory``'s kind-to-class dispatch.
"""

from __future__ import annotations

from errno import ENOENT
from unittest.mock import MagicMock, call, patch

import pytest

from mtui.support.exceptions import InvalidGiteaHashError
from mtui.support.messages import TestReportNotLoadedError
from mtui.test_reports.obs_report import OBSTestReport
from mtui.test_reports.pi_report import PITestReport
from mtui.test_reports.sl_report import SLTestReport
from mtui.test_reports.svn_io import TemplateIOError
from mtui.types import RequestReviewID
from mtui.types.updateid import AutoOBSUpdateID, UpdateID

RRID = "SUSE:Maintenance:12358:199773"


def _make_updateid(read_side_effect=None) -> tuple[AutoOBSUpdateID, MagicMock]:
    """AutoOBSUpdateID with a mocked factory; ``read`` behavior injectable."""
    uid = AutoOBSUpdateID(RRID)
    tr_mock = MagicMock(name="testreport")
    if read_side_effect is not None:
        tr_mock.read.side_effect = read_side_effect
    uid.testreport_factory = MagicMock(return_value=tr_mock)
    return uid, tr_mock


def _hash_mismatch(uid: AutoOBSUpdateID) -> InvalidGiteaHashError:
    return InvalidGiteaHashError(uid.id, "oldhash", "newhash")


# --- call-argument pins ---------------------------------------------------------


def test_checkout_happy_path_forwards_config_prompter_and_trpath(mock_config, tmp_path):
    """factory gets (config, prompter=...); read gets template_dir/<id>/log."""
    mock_config.template_dir = tmp_path
    uid, tr_mock = _make_updateid()
    factory = MagicMock(return_value=tr_mock)
    uid.testreport_factory = factory
    prompter = MagicMock(name="prompter")

    tr = uid._checkout(mock_config, interactive=False, prompter=prompter)

    assert tr is tr_mock
    factory.assert_called_once_with(mock_config, prompter=prompter)
    tr_mock.read.assert_called_once_with(tmp_path / str(uid.id) / "log")


def test_checkout_svn_checkout_gets_config_svnpath_and_id(mock_config, tmp_path):
    """The ENOENT retry checks out (config, config.svn_path, id) then re-reads."""
    mock_config.template_dir = tmp_path
    enoent = TemplateIOError(ENOENT, "No such file or directory", "log")
    uid, tr_mock = _make_updateid(read_side_effect=[enoent, None])
    vcs_mock = MagicMock(name="vcs_checkout")
    uid._vcs_checkout = vcs_mock

    tr = uid._checkout(mock_config, interactive=False)

    assert tr is tr_mock
    vcs_mock.assert_called_once_with(mock_config, mock_config.svn_path, uid.id)
    trpath = tmp_path / str(uid.id) / "log"
    assert tr_mock.read.call_args_list == [call(trpath), call(trpath)]


def test_checkout_regenerate_gets_config_teregen_paths_and_prompter(
    mock_config, tmp_path
):
    """Accepting the offer calls _regenerate(config, teregen, trdir, trpath, p)."""
    mock_config.template_dir = tmp_path
    uid, _tr_mock = _make_updateid()
    _tr_mock.read.side_effect = _hash_mismatch(uid)
    prompter = MagicMock(name="prompter")
    fresh = MagicMock(name="fresh_testreport")

    with (
        patch("mtui.types.updateid.TeReGen") as teregen_cls,
        patch("mtui.types.updateid.prompt_user", return_value=True),
        patch.object(uid, "_regenerate", return_value=fresh) as regen_mock,
    ):
        result = uid._checkout(mock_config, interactive=True, prompter=prompter)

    assert result is fresh
    teregen_cls.assert_called_once_with(mock_config)
    trdir = tmp_path / str(uid.id)
    regen_mock.assert_called_once_with(
        mock_config, teregen_cls.return_value, trdir, trdir / "log", prompter
    )


# --- real prompt_user accept lists (patch _read_line, not prompt_user) -----------


def test_checkout_regenerate_offer_accepts_literal_yes(mock_config, tmp_path):
    """Typing 'yes' at the regenerate offer takes the regeneration path."""
    mock_config.template_dir = tmp_path
    uid, tr_mock = _make_updateid()
    tr_mock.read.side_effect = _hash_mismatch(uid)
    fresh = MagicMock(name="fresh_testreport")

    with (
        patch("mtui.types.updateid.TeReGen"),
        patch("mtui.cli.term._read_line", return_value="yes") as read_mock,
        patch.object(uid, "_regenerate", return_value=fresh),
    ):
        result = uid._checkout(mock_config, interactive=True)

    assert result is fresh
    read_mock.assert_called_once()


def test_checkout_force_continue_accepts_literal_yes(mock_config, tmp_path):
    """Declining the offer, then typing 'y' at force-continue loads anyway.

    Uses the short literal deliberately: the accept list is ["yes", "y"]
    and the sibling tests already cover "yes", so this pins the "y"
    element against accept-list mutants.
    """
    mock_config.template_dir = tmp_path
    uid, tr_mock = _make_updateid()
    tr_mock.read.side_effect = _hash_mismatch(uid)

    with (
        patch("mtui.types.updateid.TeReGen"),
        patch("mtui.cli.term._read_line", side_effect=["no", "y"]) as read_mock,
    ):
        result = uid._checkout(mock_config, interactive=True)

    assert result is tr_mock
    assert read_mock.call_count == 2


def test_checkout_delete_prompt_accepts_literal_yes_and_rmtree_ignores_errors(
    mock_config, tmp_path
):
    """Declining offer+force, then 'yes' at delete removes trdir tolerantly."""
    mock_config.template_dir = tmp_path
    uid, tr_mock = _make_updateid()
    tr_mock.read.side_effect = _hash_mismatch(uid)
    trdir = tmp_path / str(uid.id)
    trdir.mkdir(parents=True)

    with (
        patch("mtui.types.updateid.TeReGen"),
        patch("mtui.cli.term._read_line", side_effect=["no", "no", "yes"]),
        patch("mtui.types.updateid.shutil.rmtree") as rmtree_mock,
        pytest.raises(TestReportNotLoadedError),
    ):
        uid._checkout(mock_config, interactive=True)

    rmtree_mock.assert_called_once_with(trdir, ignore_errors=True)


def test_checkout_non_interactive_never_reads_input(mock_config, tmp_path):
    """interactive=False reaches every prompt without touching the terminal,
    and the delete prompt's default=True removes the stale checkout."""
    mock_config.template_dir = tmp_path
    uid, tr_mock = _make_updateid()
    tr_mock.read.side_effect = _hash_mismatch(uid)
    trdir = tmp_path / str(uid.id)
    trdir.mkdir(parents=True)

    with (
        patch("mtui.types.updateid.TeReGen"),
        patch("mtui.cli.term._read_line") as read_mock,
        pytest.raises(TestReportNotLoadedError),
    ):
        uid._checkout(mock_config, interactive=False)

    read_mock.assert_not_called()
    # Offer and force-continue default to False; delete defaults to True.
    assert not trdir.exists()


# --- tr_factory dispatch ----------------------------------------------------------


@pytest.mark.parametrize(
    ("rrid", "expected_cls"),
    [
        ("SUSE:SLFO:1.1:20", SLTestReport),
        ("SUSE:PI:34556:1", PITestReport),
        ("SUSE:Maintenance:1:1", OBSTestReport),
    ],
)
def test_tr_factory_dispatches_kind_to_report_class(rrid, expected_cls):
    assert UpdateID.tr_factory(RequestReviewID(rrid)) is expected_cls
