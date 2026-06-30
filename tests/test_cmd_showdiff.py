"""Tests for the `show_diff` and `analyze_diff` commands."""

from __future__ import annotations

import logging
from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.showdiff import (
    AnalyzeDiff,
    ShowDiff,
    _apply_info,
    _defined_patches,
    _split_sections,
)
from mtui.support.messages import TestReportNotLoadedError

FIXTURES = Path(__file__).parent / "fixtures" / "diffs"


def _prompt(report_wd) -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.metadata.report_wd.return_value = report_wd
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def _load_fixture(tmp_path: Path, name: str) -> Path:
    """Copy a fixture diff into ``tmp_path`` as ``source.diff``."""
    text = (FIXTURES / name).read_text()
    (tmp_path / "source.diff").write_text(text)
    return tmp_path


def _printed(prompt: MagicMock) -> str:
    """Join everything passed to ``prompt.println`` into one string."""
    return "\n".join(
        str(call.args[0]) if call.args else "" for call in prompt.println.call_args_list
    )


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


# --- pure parser unit tests -------------------------------------------------


def test_split_sections_handles_repeating_headers():
    text = (FIXTURES / "openjdk_autosetup.diff").read_text()
    sections = _split_sections(text)
    headers = [h for h, _ in sections if h]
    assert headers[:4] == ["changes files", "old", "new", "spec files"]
    # The real diff repeats ``other changes:`` many times.
    assert headers.count("other changes") > 1


def test_defined_patches_splits_added_and_removed():
    spec = [
        "+Patch1:         fix-a.patch",
        "+Patch2:         fix-b.patch",
        "-Patch13:        old-fix.patch",
    ]
    added, removed = _defined_patches(spec)
    assert added == [("1", "fix-a.patch"), ("2", "fix-b.patch")]
    assert removed == ["old-fix.patch"]


def test_apply_info_detects_autosetup_and_missing_patch_lines():
    assert _apply_info(["+%autosetup -p1"]) == (set(), True)
    # No %patch lines at all -> treated as implicit application.
    assert _apply_info(["+Patch1: a.patch"]) == (set(), True)
    applied, auto = _apply_info(["+%patch1 -p1", "+%patch2 -p0"])
    assert applied == {"1", "2"}
    assert auto is False


# --- command integration tests against real-shaped fixtures -----------------


def test_analyze_diff_openjdk_reports_review_aid(mock_config, tmp_path, caplog):
    _load_fixture(tmp_path, "openjdk_autosetup.diff")
    prompt = _prompt(tmp_path)
    with caplog.at_level(logging.WARNING):
        AnalyzeDiff(Namespace(), mock_config, MagicMock(), prompt)()

    out = _printed(prompt)
    # New tarball surfaced.
    assert "icedtea-3.39.0.tar.xz" in out
    # Removed patch surfaced.
    assert "fix-build-with-gcc14.patch" in out
    # autosetup detected -> apply check skipped, no mismatch warning.
    assert "%autosetup" in out
    assert "do not match" not in caplog.text
    # CVE / bsc references harvested from the changelog.
    assert "CVE-2026-22007" in out or "bsc#1267355" in out


def test_analyze_diff_classic_patch_no_warning(mock_config, tmp_path, caplog):
    _load_fixture(tmp_path, "classic_patch.diff")
    prompt = _prompt(tmp_path)
    with caplog.at_level(logging.WARNING):
        AnalyzeDiff(Namespace(), mock_config, MagicMock(), prompt)()

    out = _printed(prompt)
    assert "fix-overflow.patch" in out
    assert "fix-typo.patch" in out
    # Defined patches all applied -> no mismatch warning.
    assert "do not match" not in caplog.text
    # Both patches mentioned in changelog -> no "isn't mentioned" warning.
    assert "isn't mentioned" not in caplog.text


def test_analyze_diff_mismatch_warns(mock_config, tmp_path, caplog):
    _load_fixture(tmp_path, "classic_patch_mismatch.diff")
    prompt = _prompt(tmp_path)
    with caplog.at_level(logging.WARNING):
        AnalyzeDiff(Namespace(), mock_config, MagicMock(), prompt)()

    # Patch2 defined but not applied -> mismatch warning fires.
    assert "do not match" in caplog.text


def test_analyze_diff_changelog_match_is_literal(mock_config, tmp_path, caplog):
    """A '.' in a patch name must not behave as a regex wildcard."""
    (tmp_path / "source.diff").write_text(
        "changes files:\n"
        "--------------\n"
        "+- added fixXpatch placeholder\n"
        "\n"
        "spec files:\n"
        "-----------\n"
        "+Patch1:    fix.patch\n"
        "+%patch1 -p1\n"
    )
    prompt = _prompt(tmp_path)
    with caplog.at_level(logging.WARNING):
        AnalyzeDiff(Namespace(), mock_config, MagicMock(), prompt)()

    # "fixXpatch" would match the regex "fix.patch" but not a literal search.
    assert "isn't mentioned" in caplog.text
    out = _printed(prompt)
    assert "(not in changelog)" in out
