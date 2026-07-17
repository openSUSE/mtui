"""Tests for ``AutoExport._openqa_installog_to_template`` log download.

This is the first coverage for the install-log download path, added
when it migrated from raw ``urllib`` to ``mtui.support.http``.
"""

from __future__ import annotations

import threading
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
    # ssl_verify unset in mock_config resolves to the default: verify=True.
    bs.assert_called_once_with(True)
    assert session.get.call_args.kwargs["timeout"] == HTTP_TIMEOUT


def test_download_sslerror_returns_empty_no_insecure_retry(mock_config, caplog):
    """An SSL error is a hard failure: no silent downgrade to unverified.

    ``ssl_verify`` defaults to ``True``; a certificate failure returns an
    empty result and logs an error instead of retrying without
    verification.
    """
    exporter = _make_exporter(mock_config)
    bad = _fake_session(get_side_effect=requests.exceptions.SSLError("cert"))

    with (
        patch("mtui.update_workflow.export.auto.build_session", return_value=bad) as bs,
        caplog.at_level("ERROR", logger="mtui.export.auto"),
    ):
        out = exporter._openqa_installog_to_template(_url())

    assert out == []
    # Exactly one attempt with the default verify=True -- no insecure retry.
    bs.assert_called_once_with(True)
    assert any("failed to download" in r.message for r in caplog.records)


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


def test_get_logs_creates_install_logs_dir(mock_config, tmp_path):
    """``get_logs`` creates the install_logs directory before writing.

    Regression: SLFO load paths may not pre-create
    ``template_dir/<rrid>/install_logs``; writing a log into a missing
    directory raised ``FileNotFoundError``.
    """
    mock_config.template_dir = tmp_path
    rrid = "SUSE:SLFO:1.2:5295"
    exporter = AutoExport(mock_config, MagicMock(), FileList([]), False, rrid, False)
    exporter.openqa = MagicMock()
    exporter.openqa.auto.results = [_url()]

    target_dir = tmp_path / rrid / mock_config.install_logs
    assert not target_dir.exists()

    with patch.object(
        AutoExport, "_openqa_installog_to_template", return_value=["log line\n"]
    ):
        filenames = exporter.get_logs()

    assert target_dir.is_dir()
    assert len(filenames) == 1
    written = target_dir / "sle_15-SP7_x86_64.log"
    assert written.is_file()


def _multi_results() -> list[URLs]:
    """Three install-log URLs with distinct filenames (varying arch)."""
    return [
        URLs("sle", "x86_64", "15-SP7", "https://oqa/1"),
        URLs("sle", "aarch64", "15-SP7", "https://oqa/2"),
        URLs("sle", "s390x", "15-SP7", "https://oqa/3"),
    ]


def test_get_logs_preserves_order_across_parallel_downloads(mock_config, tmp_path):
    """Parallel downloads still line up with ``auto.results``, in order.

    ``executor.map`` preserves input order, so each log's content must be
    written to the file derived from the *same* result -- proving the
    concurrent downloads did not scramble the pairing -- and the ``_writer``
    calls stay serial and in order.
    """
    mock_config.template_dir = tmp_path
    rrid = "SUSE:Maintenance:1:1"
    exporter = AutoExport(mock_config, MagicMock(), FileList([]), False, rrid, False)
    exporter.openqa = MagicMock()
    exporter.openqa.auto.results = _multi_results()

    with (
        patch.object(
            exporter,
            "_openqa_installog_to_template",
            side_effect=lambda url: [f"content-{url.url}\n"],
        ),
        patch.object(exporter, "_writer") as writer,
    ):
        filenames = exporter.get_logs()

    assert [str(p) for p in filenames] == [
        "sle_15-SP7_x86_64.log",
        "sle_15-SP7_aarch64.log",
        "sle_15-SP7_s390x.log",
    ]
    written = [(c.args[0].name, c.args[1]) for c in writer.call_args_list]
    assert written == [
        ("sle_15-SP7_x86_64.log", ["content-https://oqa/1\n"]),
        ("sle_15-SP7_aarch64.log", ["content-https://oqa/2\n"]),
        ("sle_15-SP7_s390x.log", ["content-https://oqa/3\n"]),
    ]


def test_get_logs_downloads_run_concurrently(mock_config, tmp_path):
    """The per-log downloads run in parallel, not one after another.

    Each download blocks on a Barrier of width N; it trips only if every
    download is in flight at once. A revert to the serial ``map`` makes the
    first download block alone until the Barrier times out, and the raised
    ``BrokenBarrierError`` propagates out of ``get_logs`` -- so this fails on
    that revert.
    """
    mock_config.template_dir = tmp_path
    exporter = AutoExport(
        mock_config, MagicMock(), FileList([]), False, "SUSE:Maintenance:1:1", False
    )
    exporter.openqa = MagicMock()
    results = _multi_results()
    exporter.openqa.auto.results = results
    barrier = threading.Barrier(len(results))

    def _fake_download(url):
        barrier.wait(timeout=10)
        return [f"content-{url.url}\n"]

    with (
        patch.object(
            exporter, "_openqa_installog_to_template", side_effect=_fake_download
        ),
        patch.object(exporter, "_writer"),
    ):
        filenames = exporter.get_logs()

    assert len(filenames) == 3


def test_get_logs_skips_failed_download_and_keeps_pairing(mock_config, tmp_path):
    """A download that returns ``[]`` (failed) is skipped; pairing survives.

    Proves the ``if y:`` skip still works after the concurrent fan-out: the
    empty middle result is dropped, and the two written files pair with the
    two *non-empty* results in order.
    """
    mock_config.template_dir = tmp_path
    exporter = AutoExport(
        mock_config, MagicMock(), FileList([]), False, "SUSE:Maintenance:1:1", False
    )
    exporter.openqa = MagicMock()
    exporter.openqa.auto.results = _multi_results()  # x86_64, aarch64, s390x

    def _fake_download(url):
        # The aarch64 log "fails" and yields no lines.
        return [] if url.url == "https://oqa/2" else [f"content-{url.url}\n"]

    with (
        patch.object(
            exporter, "_openqa_installog_to_template", side_effect=_fake_download
        ),
        patch.object(exporter, "_writer") as writer,
    ):
        filenames = exporter.get_logs()

    assert [str(p) for p in filenames] == [
        "sle_15-SP7_x86_64.log",
        "sle_15-SP7_s390x.log",
    ]
    written = [(c.args[0].name, c.args[1]) for c in writer.call_args_list]
    assert written == [
        ("sle_15-SP7_x86_64.log", ["content-https://oqa/1\n"]),
        ("sle_15-SP7_s390x.log", ["content-https://oqa/3\n"]),
    ]
