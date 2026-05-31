"""Tests for ``mtui.template.testreport_svn_checkout`` error handling."""

from __future__ import annotations

import subprocess
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import pytest

from mtui import template
from mtui.messages import SvnCheckoutFailed
from mtui.types import RequestReviewID


def _cfg(tmp_path: Path) -> SimpleNamespace:
    return SimpleNamespace(
        template_dir=tmp_path,
        fancy_reports_url="https://qam.suse.de/reports",
    )


def test_svn_checkout_missing_raises_clear_error(tmp_path: Path) -> None:
    """A non-existent report yields a clear message pointing to the log URL."""
    cfg = _cfg(tmp_path)
    rrid = RequestReviewID("SUSE:SLFO:1.2:9999")
    completed = subprocess.CompletedProcess(
        args=[], returncode=1, stderr="svn: E170000: URL '...' doesn't exist\n"
    )

    with (
        patch("mtui.template.subprocess.run", return_value=completed) as run,
        pytest.raises(SvnCheckoutFailed) as excinfo,
    ):
        template.testreport_svn_checkout(
            cfg, "svn+ssh://svn@qam.suse.de/testreports", rrid
        )

    # svn's own stderr is captured (suppressed from the terminal).
    assert run.call_args.kwargs.get("stderr") is subprocess.PIPE
    msg = str(excinfo.value)
    assert "SUSE:SLFO:1.2:9999 does not exist" in msg
    assert "https://qam.suse.de/reports/SUSE:SLFO:1.2:9999/log" in msg
    # The cryptic svn error code is not part of the user-facing message.
    assert "E170000" not in msg


def test_svn_checkout_success_does_not_raise(tmp_path: Path) -> None:
    """A successful checkout returns without raising."""
    cfg = _cfg(tmp_path)
    rrid = RequestReviewID("SUSE:Maintenance:1:1")
    completed = subprocess.CompletedProcess(args=[], returncode=0, stderr="")

    with patch("mtui.template.subprocess.run", return_value=completed):
        template.testreport_svn_checkout(cfg, "svn+ssh://svn@example/testreports", rrid)
