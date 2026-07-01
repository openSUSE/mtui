"""Tests for ``TestReport.set_slack_review`` template rewriting."""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

import pytest

from mtui.test_reports.metadata_parsers import ReducedMetadataParser
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


def test_free_text_mention_is_not_a_marker(mock_config, tmp_path):
    # A comment ending in "Slack Review: X/Y" mid-line is free text, not a
    # marker: without a real marker line there is nothing to resume.
    p = _write(
        tmp_path,
        "comment: please see Slack Review: CBAD/000.111\nTest Plan Reviewer: alice",
    )
    assert _report(mock_config, p).get_slack_review() is None


def test_free_text_mention_does_not_shadow_real_marker(mock_config, tmp_path):
    # The template's free-text comment field sits ABOVE the real marker's
    # anchor; text merely containing the phrase must not shadow the marker
    # (which would wedge the approve/reject gate on a dead message).
    p = _write(
        tmp_path,
        "comment: please see Slack Review: CBAD/000.111\n"
        "Test Plan Reviewer: alice\n"
        "Slack Review: CREAL/111.222",
    )
    tr = _report(mock_config, p)
    assert tr.get_slack_review() == ("CREAL", "111.222")

    # And a rewrite replaces the real marker, leaving the comment untouched.
    tr.set_slack_review("CNEW", "333.444")
    text = p.read_text()
    assert "comment: please see Slack Review: CBAD/000.111" in text
    assert "Slack Review: CNEW/333.444" in text
    assert "CREAL" not in text


def test_duplicate_markers_first_wins_and_write_collapses(mock_config, tmp_path):
    # A merge or hand-edit can leave two marker lines; every reader is
    # first-wins, and a write collapses them back to a single marker.
    p = _write(
        tmp_path,
        "Test Plan Reviewer: alice\n"
        "Slack Review: CFIRST/111.222\n"
        "Slack Review: CSECOND/333.444",
    )
    tr = _report(mock_config, p)
    assert tr.get_slack_review() == ("CFIRST", "111.222")

    tr.set_slack_review("CNEW", "555.666")

    text = p.read_text()
    assert text.count("Slack Review:") == 1
    assert "Slack Review: CNEW/555.666" in text
    assert "CSECOND" not in text
    assert tr.get_slack_review() == ("CNEW", "555.666")


def _parse_snapshot(text: str) -> tuple[str, str] | None:
    """Runs the load-time line parser the way ``_parse_json`` feeds it."""
    results = SimpleNamespace(hostnames=set(), jira={}, bugs={}, slack_review=None)
    for line in text.splitlines():
        ReducedMetadataParser.parse(results, line)
    return results.slack_review


def test_load_time_parser_agrees_with_disk_read(mock_config, tmp_path):
    # Same content, two readers: the load-time snapshot parser and the
    # get_slack_review disk read must extract the identical marker — for a
    # plain marker, duplicate markers (first wins), a free-text mention
    # alone, and a free-text mention above a real marker.
    cases = [
        "Test Plan Reviewer: alice\nSlack Review: C1/111.222",
        "Test Plan Reviewer: alice\nSlack Review: C1/111.222\nSlack Review: C2/333.444",
        "comment: see Slack Review: CBAD/000.111\nTest Plan Reviewer: alice",
        "comment: see Slack Review: CBAD/000.111\n"
        "Test Plan Reviewer: alice\n"
        "Slack Review: CREAL/111.222",
    ]
    for lines in cases:
        p = _write(tmp_path, lines)
        disk = _report(mock_config, p).get_slack_review()
        assert _parse_snapshot(p.read_text()) == disk, lines
