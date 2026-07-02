"""Tests for ``TestReport.set_slack_review`` template rewriting."""

from __future__ import annotations

from pathlib import Path

import pytest

from mtui.test_reports.svn_io import TemplateFormatError
from mtui.test_reports.testreport import TestReport


class _ConcreteReport(TestReport):
    """Minimal instantiable TestReport for exercising set_slack_review."""

    @property
    def id(self):
        return "test"

    def check_hash(self):
        return True, "", ""

    def _parser(self):
        return {}

    def _update_repos_parser(self):
        return {}

    def list_update_commands(self, targets, display):
        return None


def _report(mock_config, path: Path | None) -> TestReport:
    """Builds a minimal concrete TestReport with ``path`` set."""
    tr = _ConcreteReport.__new__(_ConcreteReport)  # bypass __init__
    tr.config = mock_config
    tr.path = path
    tr.slack_review = None
    return tr


TEMPLATE = """\
METADATA:
=========
Packager: slemke@suse.com
Bugs: 12345
{lines}
Repository: http://example/
"""


def _write(tmp_path: Path, lines: str) -> Path:
    p = tmp_path / "log"
    p.write_text(TEMPLATE.format(lines=lines))
    return p


def test_inserts_marker_after_reviewer_line(mock_config, tmp_path):
    p = _write(tmp_path, "Test Plan Reviewer: alice")
    tr = _report(mock_config, p)

    tr.set_slack_review("C123", "1700000000.000100")

    text = p.read_text()
    assert "Test Plan Reviewer: alice\nSlack Review: C123/1700000000.000100\n" in text
    assert tr.slack_review == ("C123", "1700000000.000100")


def test_replaces_existing_marker(mock_config, tmp_path):
    p = _write(
        tmp_path,
        "Test Plan Reviewer: alice\nSlack Review: COLD/111.222",
    )
    tr = _report(mock_config, p)

    tr.set_slack_review("CNEW", "333.444")

    text = p.read_text()
    assert "Slack Review: CNEW/333.444" in text
    assert "COLD" not in text
    # Replaced in place, not inserted again.
    assert text.count("Slack Review:") == 1
    assert tr.slack_review == ("CNEW", "333.444")


def test_other_lines_unchanged(mock_config, tmp_path):
    p = _write(tmp_path, "Test Plan Reviewer: alice")
    original = p.read_text()
    tr = _report(mock_config, p)

    tr.set_slack_review("C123", "111.222")

    new = p.read_text()
    # Exactly the marker line was added; everything else is untouched.
    assert [ln for ln in new.splitlines() if ln not in original.splitlines()] == [
        "Slack Review: C123/111.222"
    ]


def test_missing_anchor_raises_and_leaves_file(mock_config, tmp_path):
    p = tmp_path / "log"
    p.write_text("METADATA:\nPackager: x\nRepository: y\n")
    original = p.read_text()
    tr = _report(mock_config, p)

    with pytest.raises(TemplateFormatError, match="Test Plan Reviewer"):
        tr.set_slack_review("C123", "111.222")

    assert p.read_text() == original
    assert tr.slack_review is None


def test_no_path_raises(mock_config):
    tr = _report(mock_config, None)

    with pytest.raises(RuntimeError):
        tr.set_slack_review("C123", "111.222")


def test_has_anchor_with_reviewer_line(mock_config, tmp_path):
    p = _write(tmp_path, "Test Plan Reviewer: alice")
    assert _report(mock_config, p).has_slack_review_anchor() is True


def test_has_anchor_with_existing_marker_only(mock_config, tmp_path):
    p = _write(tmp_path, "Slack Review: C1/111.222")
    assert _report(mock_config, p).has_slack_review_anchor() is True


def test_has_anchor_false_without_anchor(mock_config, tmp_path):
    p = tmp_path / "log"
    p.write_text("METADATA:\nPackager: x\nRepository: y\n")
    assert _report(mock_config, p).has_slack_review_anchor() is False


def test_has_anchor_false_without_path(mock_config):
    assert _report(mock_config, None).has_slack_review_anchor() is False


def test_get_slack_review_parses_marker(mock_config, tmp_path):
    p = _write(tmp_path, "Test Plan Reviewer: alice\nSlack Review: C123/111.222")
    assert _report(mock_config, p).get_slack_review() == ("C123", "111.222")


def test_get_slack_review_none_when_absent(mock_config, tmp_path):
    p = _write(tmp_path, "Test Plan Reviewer: alice")
    assert _report(mock_config, p).get_slack_review() is None


def test_get_slack_review_none_without_path(mock_config):
    assert _report(mock_config, None).get_slack_review() is None


def test_missing_file_reads_as_no_marker_and_no_anchor(mock_config, tmp_path):
    # An ``svn up`` can delete a withdrawn report's template underneath the
    # session; both file-backed reads degrade gracefully instead of raising.
    gone = tmp_path / "log"  # never created
    tr = _report(mock_config, gone)
    assert tr.get_slack_review() is None
    assert tr.has_slack_review_anchor() is False


def test_set_then_get_roundtrip(mock_config, tmp_path):
    p = _write(tmp_path, "Test Plan Reviewer: alice")
    tr = _report(mock_config, p)

    tr.set_slack_review("CNEW", "333.444")

    # get_slack_review reads the FILE, so it observes what set wrote.
    assert tr.get_slack_review() == ("CNEW", "333.444")
