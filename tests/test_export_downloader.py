"""Tests for ``mtui.update_workflow.export.downloader``."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest
import requests

from mtui.support.messages import ResultsMissingError
from mtui.update_workflow.export.downloader import (
    _emptylog,
    _installlog,
    _resultlog,
    _subdl,
    download_logs,
    downloader,
)

# ---------------------------------------------------------------------------
# _subdl
# ---------------------------------------------------------------------------


def test_subdl_success() -> None:
    with (
        patch(
            "mtui.update_workflow.export.downloader.get_bytes",
            return_value=b"log-bytes",
        ) as get_bytes,
        patch("mtui.update_workflow.export.downloader.atomic_write_file") as write_file,
    ):
        _subdl("http://h/file", "/tmp/x", {"name": "t", "arch": "x"}, "tolerant")
    get_bytes.assert_called_once_with("http://h/file", verify=True)
    # The downloaded bytes are written to the requested local path.
    args, _ = write_file.call_args
    assert args[0] == b"log-bytes"
    assert str(args[1]) == "/tmp/x"


def test_subdl_passes_verify_policy() -> None:
    with (
        patch(
            "mtui.update_workflow.export.downloader.get_bytes",
            return_value=b"",
        ) as get_bytes,
        patch("mtui.update_workflow.export.downloader.atomic_write_file"),
    ):
        _subdl(
            "http://h/file",
            "/tmp/x",
            {"name": "t", "arch": "x"},
            "tolerant",
            verify=False,
        )
    get_bytes.assert_called_once_with("http://h/file", verify=False)


def test_subdl_http_error_tolerant_logs_no_raise(caplog) -> None:
    err = requests.exceptions.HTTPError("404")
    with (
        patch("mtui.update_workflow.export.downloader.get_bytes", side_effect=err),
        caplog.at_level("ERROR", logger="mtui.export.downloader"),
    ):
        _subdl("http://h/file", "/tmp/x", {"name": "t", "arch": "x"}, "tolerant")
    assert any("Download from" in r.message for r in caplog.records)


def test_subdl_http_error_full_raises_results_missing() -> None:
    err = requests.exceptions.HTTPError("404")
    with (
        patch("mtui.update_workflow.export.downloader.get_bytes", side_effect=err),
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
    with patch("mtui.update_workflow.export.downloader._subdl") as subdl:
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
    with patch("mtui.update_workflow.export.downloader._subdl") as subdl:
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


def _oqa_host(*results: tuple[str, str, str]) -> MagicMock:
    """Builds a fake openQA connector with (name, test_id, arch) results."""
    host = MagicMock()
    host.host = "http://h"
    host.results = []
    for name, test_id, arch in results:
        result = MagicMock()
        result.name = name
        result.test_id = test_id
        result.arch = arch
        host.results.append(result)
    return host


def test_download_logs_no_results_is_noop() -> None:
    download_logs([], "/r", "/i", "tolerant")  # must not raise


def test_download_logs_dispatches_via_thread_pool() -> None:
    host = _oqa_host(("install_x", "1", "x"))
    install = MagicMock()
    with patch.dict(downloader, {"install": install}):
        download_logs([host], "/r", "/i", "tolerant")
    install.assert_called_once_with(
        "http://h",
        {"name": "install_x", "test_id": "1", "arch": "x"},
        "/r",
        "/i",
        "tolerant",
        True,
    )


def test_download_logs_unexpected_error_is_logged_not_dropped(caplog) -> None:
    # A non-RequestException (e.g. a bad TLS CA bundle path raising OSError)
    # bypasses _subdl's error handling and used to vanish with the discarded
    # future. It must be surfaced in the log, without aborting tolerant mode.
    host = _oqa_host(("install_x", "1", "x86_64"))
    with (
        patch(
            "mtui.update_workflow.export.downloader.get_bytes",
            side_effect=OSError("bad CA bundle"),
        ),
        caplog.at_level("WARNING", logger="mtui.export.downloader"),
    ):
        download_logs([host], "/r", "/i", "tolerant")  # must not raise
    errors = [r.getMessage() for r in caplog.records if r.levelname == "ERROR"]
    assert any(
        "install_x" in m and "http://h" in m and "bad CA bundle" in m for m in errors
    )
    warnings = [r.getMessage() for r in caplog.records if r.levelname == "WARNING"]
    assert "1 of 1 openQA log downloads failed" in warnings


def test_download_logs_full_raises_results_missing() -> None:
    host = _oqa_host(("install_x", "1", "x86_64"))
    with (
        patch(
            "mtui.update_workflow.export.downloader.get_bytes",
            side_effect=requests.exceptions.HTTPError("404"),
        ),
        pytest.raises(ResultsMissingError),
    ):
        download_logs([host], "/r", "/i", "full")


def test_download_logs_mixed_batch_downloads_successes(caplog) -> None:
    host = _oqa_host(("install_x", "1", "a1"), ("install_x", "2", "a2"))

    def get_bytes(url, verify=True):
        if "/tests/1/" in url:
            raise OSError("bad CA bundle")
        return b"log-bytes"

    with (
        patch(
            "mtui.update_workflow.export.downloader.get_bytes", side_effect=get_bytes
        ),
        patch("mtui.update_workflow.export.downloader.atomic_write_file") as write_file,
        caplog.at_level("WARNING", logger="mtui.export.downloader"),
    ):
        download_logs([host], "/r", "/i", "tolerant")

    # The successful download is still written...
    write_file.assert_called_once()
    assert write_file.call_args[0][0] == b"log-bytes"
    # ...and the summary reports the failure instead of pretending success.
    warnings = [r.getMessage() for r in caplog.records if r.levelname == "WARNING"]
    assert "1 of 2 openQA log downloads failed" in warnings


def test_download_logs_full_mixed_batch_finishes_before_raising() -> None:
    host = _oqa_host(("install_x", "1", "a1"), ("install_x", "2", "a2"))

    def get_bytes(url, verify=True):
        if "/tests/1/" in url:
            raise requests.exceptions.HTTPError("404")
        return b"log-bytes"

    with (
        patch(
            "mtui.update_workflow.export.downloader.get_bytes", side_effect=get_bytes
        ),
        patch("mtui.update_workflow.export.downloader.atomic_write_file") as write_file,
        pytest.raises(ResultsMissingError),
    ):
        download_logs([host], "/r", "/i", "full")

    # The whole batch runs to completion before the failure propagates.
    write_file.assert_called_once()
    assert write_file.call_args[0][0] == b"log-bytes"
