"""Tests for ``AutoExport._openqa_installog_to_template`` log download.

This is the first coverage for the install-log download path, added
when it migrated from raw ``urllib`` to ``mtui.support.http``.
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import requests

from mtui.support.http import HTTP_TIMEOUT
from mtui.types import FileList, URLs
from mtui.update_workflow.export.auto import AutoExport


def _make_exporter(mock_config) -> AutoExport:
    """Build an AutoExport whose openQA results are irrelevant here."""
    return AutoExport(
        mock_config,
        MagicMock(),
        FileList([]),
        False,
        "SUSE:Maintenance:1:1",
        False,
    )


def _url(u: str = "https://openqa.example.com/tests/1/file/log") -> URLs:
    return URLs("sle", "x86_64", "15-SP7", u)


def _fake_session(get_return=None, get_side_effect=None) -> MagicMock:
    session = MagicMock()
    if get_side_effect is not None:
        session.get.side_effect = get_side_effect
    else:
        session.get.return_value = get_return
    return session


def _ok_response(text: str) -> MagicMock:
    resp = MagicMock()
    resp.text = text
    resp.raise_for_status.return_value = None
    return resp


def test_download_success_returns_decoded_lines(mock_config):
    exporter = _make_exporter(mock_config)
    resp = _ok_response("line1\nline2\n")
    session = _fake_session(get_return=resp)

    with patch(
        "mtui.update_workflow.export.auto.build_session", return_value=session
    ) as bs:
        out = exporter._openqa_installog_to_template(_url())

    assert out == ["line1\n", "line2\n"]
    # ssl_verify is None in mock_config -> per-site default True.
    bs.assert_called_once_with(True)
    assert session.get.call_args.kwargs["timeout"] == HTTP_TIMEOUT


def test_download_default_falls_back_to_unverified_on_sslerror(mock_config):
    exporter = _make_exporter(mock_config)
    good = _fake_session(get_return=_ok_response("recovered\n"))
    bad = _fake_session(get_side_effect=requests.exceptions.SSLError("cert"))

    # First build_session() (verify=True) yields the failing session;
    # the fallback build_session(verify=False) yields the good one.
    with patch(
        "mtui.update_workflow.export.auto.build_session",
        side_effect=[bad, good],
    ) as bs:
        out = exporter._openqa_installog_to_template(_url())

    assert out == ["recovered\n"]
    assert bs.call_count == 2
    assert bs.call_args_list[0].args == (True,)
    assert bs.call_args_list[1].kwargs == {"verify": False}


def test_download_ssl_verify_true_does_not_fall_back(mock_config, caplog):
    mock_config.ssl_verify = True
    exporter = _make_exporter(mock_config)
    bad = _fake_session(get_side_effect=requests.exceptions.SSLError("cert"))

    with (
        patch("mtui.update_workflow.export.auto.build_session", return_value=bad) as bs,
        caplog.at_level("ERROR", logger="mtui.export.auto"),
    ):
        out = exporter._openqa_installog_to_template(_url())

    assert out == []
    # Only the single verified attempt -- no insecure retry.
    bs.assert_called_once_with(True)
    assert any("failed to download" in r.message for r in caplog.records)


def test_download_request_exception_returns_empty_and_logs(mock_config, caplog):
    exporter = _make_exporter(mock_config)
    session = _fake_session(
        get_side_effect=requests.exceptions.ConnectionError("refused")
    )

    with (
        patch("mtui.update_workflow.export.auto.build_session", return_value=session),
        caplog.at_level("ERROR", logger="mtui.export.auto"),
    ):
        out = exporter._openqa_installog_to_template(_url())

    assert out == []
    assert any("failed to download" in r.message for r in caplog.records)


def test_download_ssl_verify_false_skips_verification(mock_config):
    mock_config.ssl_verify = False
    exporter = _make_exporter(mock_config)
    session = _fake_session(get_return=_ok_response("x\n"))

    with patch(
        "mtui.update_workflow.export.auto.build_session", return_value=session
    ) as bs:
        out = exporter._openqa_installog_to_template(_url())

    assert out == ["x\n"]
    bs.assert_called_once_with(False)
