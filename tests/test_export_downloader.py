"""Tests for ``mtui.export.downloader``."""

from __future__ import annotations

import urllib.error
from unittest.mock import MagicMock, patch

import pytest

from mtui.export.downloader import (
    _emptylog,
    _installlog,
    _resultlog,
    _subdl,
    download_logs,
    downloader,
)
from mtui.support.messages import ResultsMissingError

# ---------------------------------------------------------------------------
# _subdl
# ---------------------------------------------------------------------------


def test_subdl_success() -> None:
    with patch("mtui.export.downloader.urlretrieve") as urlretrieve:
        _subdl("http://h/file", "/tmp/x", {"name": "t", "arch": "x"}, "tolerant")
    urlretrieve.assert_called_once_with("http://h/file", "/tmp/x")


def test_subdl_http_error_tolerant_logs_no_raise(caplog) -> None:
    err = urllib.error.HTTPError("http://h", 404, "no", {}, None)  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    with (
        patch("mtui.export.downloader.urlretrieve", side_effect=err),
        caplog.at_level("ERROR", logger="mtui.export.downloader"),
    ):
        _subdl("http://h/file", "/tmp/x", {"name": "t", "arch": "x"}, "tolerant")
    assert any("Download from" in r.message for r in caplog.records)


def test_subdl_http_error_full_raises_results_missing() -> None:
    err = urllib.error.HTTPError("http://h", 404, "no", {}, None)  # type: ignore[arg-type]  # ty: ignore[invalid-argument-type]
    with (
        patch("mtui.export.downloader.urlretrieve", side_effect=err),
        pytest.raises(ResultsMissingError),
    ):
        _subdl("http://h/file", "/tmp/x", {"name": "t", "arch": "x"}, "full")


# ---------------------------------------------------------------------------
# _emptylog
# ---------------------------------------------------------------------------


def test_emptylog_logs_only(caplog) -> None:
    with caplog.at_level("DEBUG", logger="mtui.export.downloader"):
        _emptylog("hostA", {"name": "t", "arch": "x86"})
    assert any("No log to download" in r.message for r in caplog.records)


# ---------------------------------------------------------------------------
# _resultlog / _installlog
# ---------------------------------------------------------------------------


def test_resultlog_dispatches_to_subdl() -> None:
    with patch("mtui.export.downloader._subdl") as subdl:
        _resultlog(
            "http://h",
            {"test_id": "1", "arch": "x", "name": "ltp"},
            "/res",
            None,
            "tolerant",
        )
    subdl.assert_called_once()
    args, _ = subdl.call_args
    assert "result_array.json" in args[0]


def test_installlog_dispatches_to_subdl() -> None:
    with patch("mtui.export.downloader._subdl") as subdl:
        _installlog(
            "http://h",
            {"test_id": "1", "arch": "x", "name": "install_kernel"},
            None,
            "/installs",
            "tolerant",
        )
    subdl.assert_called_once()
    args, _ = subdl.call_args
    assert "update_kernel-zypper.log" in args[0]


# ---------------------------------------------------------------------------
# downloader dispatch dict
# ---------------------------------------------------------------------------


def test_downloader_dispatch_dict_keys() -> None:
    assert downloader["install"] is _installlog
    assert downloader["ltp"] is _resultlog


# ---------------------------------------------------------------------------
# download_logs
# ---------------------------------------------------------------------------


def test_download_logs_no_results_is_noop() -> None:
    download_logs([], "/r", "/i", "tolerant")  # must not raise


def test_download_logs_dispatches_via_thread_pool() -> None:
    host = MagicMock()
    host.host = "http://h"
    result = MagicMock()
    result.name = "install_x"
    result.test_id = "1"
    result.arch = "x"
    host.results = [result]

    fake_executor = MagicMock()
    fake_executor.__enter__.return_value = fake_executor
    fake_executor.__exit__.return_value = False
    with patch(
        "mtui.export.downloader.concurrent.futures.ThreadPoolExecutor",
        return_value=fake_executor,
    ):
        download_logs([host], "/r", "/i", "tolerant")
    fake_executor.submit.assert_called_once()
