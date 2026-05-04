"""Smoke tests for the mtui command-line entrypoint.

Exercises ``mtui --help`` and ``mtui -V`` end-to-end via subprocess
to catch import-time, argparse, and entrypoint regressions that unit
tests typically miss. Runs first under ``python -m mtui``; falls back
to the ``mtui`` console script on PATH; skips when neither is usable
(e.g. uninstalled checkout in some CI environments).
"""

from __future__ import annotations

import shutil
import subprocess
import sys

import pytest


def _invoke_args() -> list[str] | None:
    """Return argv prefix for invoking mtui, or ``None`` if not runnable."""
    probe = subprocess.run(
        [sys.executable, "-m", "mtui", "--help"],
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    if probe.returncode == 0:
        return [sys.executable, "-m", "mtui"]

    on_path = shutil.which("mtui")
    if on_path:
        return [on_path]

    return None


@pytest.fixture(scope="module")
def mtui_argv() -> list[str]:
    argv = _invoke_args()
    if argv is None:
        pytest.skip("mtui is not invokable in this environment")
    return argv


@pytest.mark.integration
def test_cli_help_exits_zero(mtui_argv):
    """``mtui --help`` exits 0 and prints argparse usage."""
    result = subprocess.run(
        [*mtui_argv, "--help"],
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert "usage:" in result.stdout.lower()


@pytest.mark.integration
def test_cli_version_exits_zero(mtui_argv):
    """``mtui -V`` exits 0 and prints mtui + interpreter + dep versions."""
    result = subprocess.run(
        [*mtui_argv, "-V"],
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    out = result.stdout
    assert out.strip(), "version output should be non-empty"
    # All four expected lines must be present, in order.
    lines = out.strip().splitlines()
    assert len(lines) >= 4, out
    assert lines[0].startswith("mtui ")
    assert lines[1].startswith("Python ")
    assert lines[2].startswith("paramiko ")
    assert lines[3].startswith("openqa-client ")
