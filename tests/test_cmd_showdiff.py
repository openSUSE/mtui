"""Tests for the `show_diff` and `analyze_diff` commands."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.showdiff import AnalyzeDiff, ShowDiff
from mtui.support.messages import TestReportNotLoadedError


def _prompt(report_wd) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.metadata.report_wd.return_value = report_wd
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_show_diff_happy_pages_lines(mock_config, tmp_path):
    (tmp_path / "source.diff").write_text("line1\nline2\n")
    prompt = _prompt(tmp_path)

    with patch("mtui.commands.showdiff.page") as page:
        ShowDiff(Namespace(), mock_config, MagicMock(), prompt)()

    page.assert_called_once()
    lines = page.call_args.args[0]
    assert "line1" in lines


def test_show_diff_without_metadata_raises(mock_config, tmp_path):
    prompt = _prompt(tmp_path)
    prompt.metadata.__bool__ = lambda self: False
    with pytest.raises(TestReportNotLoadedError):
        ShowDiff(Namespace(), mock_config, MagicMock(), prompt)()


def test_analyze_diff_no_spec_section_prints_message(mock_config, tmp_path):
    (tmp_path / "source.diff").write_text("just a diff\n")
    prompt = _prompt(tmp_path)
    AnalyzeDiff(Namespace(), mock_config, MagicMock(), prompt)()
    prompt.println.assert_any_call("No spec mentioned in source.diff")
