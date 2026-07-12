"""Mutation-killing pins for ``mtui.update_workflow.export.downloader``.

The pre-existing tests assert only that a filename fragment appears
somewhere in the dispatched URL, leaving the URL construction, the local
filename format, and the errormode/verify forwarding unobserved. These
tests pin the exact values, plus the multi-host accumulation and the
unknown-job fallback in ``download_logs``.
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest
import requests

from mtui.support.messages import ResultsMissingError
from mtui.update_workflow.export.downloader import (
    _installlog,
    _resultlog,
    download_logs,
    downloader,
)

# ---------------------------------------------------------------------------
# _resultlog / _installlog: exact URL, local path, errormode and verify
# ---------------------------------------------------------------------------


def test_resultlog_builds_exact_url_and_local_path() -> None:
    test = {"test_id": "1", "arch": "x", "name": "ltp"}
    with patch("mtui.update_workflow.export.downloader._subdl") as subdl:
        _resultlog("http://h", test, "/res", None, "tolerant")

    subdl.assert_called_once_with(
        "http://h/tests/1/file/result_array.json",
        "/res/h-x-ltp.json",
        test,
        "tolerant",
        True,
    )


def test_resultlog_forwards_verify_policy() -> None:
    test = {"test_id": "1", "arch": "x", "name": "ltp"}
    with patch("mtui.update_workflow.export.downloader._subdl") as subdl:
        _resultlog("http://h", test, "/res", None, "full", verify=False)

    assert subdl.call_args.args[3] == "full"
    assert subdl.call_args.args[4] is False


def test_installlog_builds_exact_url_and_local_path() -> None:
    test = {"test_id": "1", "arch": "x", "name": "install_kernel"}
    with patch("mtui.update_workflow.export.downloader._subdl") as subdl:
        _installlog("http://h", test, None, "/installs", "tolerant")

    subdl.assert_called_once_with(
        "http://h/tests/1/file/update_kernel-zypper.log",
        "/installs/h-zypper-x.log",
        test,
        "tolerant",
        True,
    )


# ---------------------------------------------------------------------------
# download_logs: ltp jobs route end-to-end through _resultlog
# ---------------------------------------------------------------------------


def _oqa_host(host: str, *results: tuple[str, str, str]) -> MagicMock:
    mock = MagicMock()
    mock.host = host
    mock.results = []
    for name, test_id, arch in results:
        result = MagicMock()
        result.name = name
        result.test_id = test_id
        result.arch = arch
        mock.results.append(result)
    return mock


def test_download_logs_ltp_full_mode_raises_results_missing() -> None:
    """An 'ltp' job dispatches through the real _resultlog; a failed
    download under errormode='full' surfaces as ResultsMissingError."""
    host = _oqa_host("http://h", ("ltp_syscalls", "7", "x86_64"))
    with (
        patch(
            "mtui.update_workflow.export.downloader.get_bytes",
            side_effect=requests.exceptions.HTTPError("404"),
        ) as get_bytes,
        pytest.raises(ResultsMissingError),
    ):
        download_logs([host], "/res", "/inst", "full")

    get_bytes.assert_called_once_with(
        "http://h/tests/7/file/result_array.json", verify=True
    )


# ---------------------------------------------------------------------------
# download_logs: results from every host are accumulated
# ---------------------------------------------------------------------------


def test_download_logs_downloads_results_of_all_hosts() -> None:
    h1 = _oqa_host("http://h1", ("install_x", "1", "a1"))
    h2 = _oqa_host("http://h2", ("install_x", "2", "a2"))
    install = MagicMock()

    with patch.dict(downloader, {"install": install}):
        download_logs([h1, h2], "/res", "/inst", "tolerant")

    assert install.call_count == 2
    assert {c.args[0] for c in install.call_args_list} == {
        "http://h1",
        "http://h2",
    }


# ---------------------------------------------------------------------------
# download_logs: unknown job names fall back to the quiet _emptylog
# ---------------------------------------------------------------------------


def test_download_logs_unknown_job_name_is_skipped_via_emptylog() -> None:
    host = _oqa_host("http://h", ("boot_x", "9", "z"))

    with patch("mtui.update_workflow.export.downloader._emptylog") as emptylog:
        download_logs([host], "/res", "/inst", "tolerant")

    emptylog.assert_called_once_with(
        "http://h",
        {"name": "boot_x", "test_id": "9", "arch": "z"},
        "/res",
        "/inst",
        "tolerant",
        True,
    )
