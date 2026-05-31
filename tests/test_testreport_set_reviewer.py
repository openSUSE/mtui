"""Tests for ``TestReport.set_reviewer`` template rewriting."""

from __future__ import annotations

from pathlib import Path

import pytest

from mtui.template import TemplateFormatError
from mtui.template.testreport import TestReport


class _ConcreteReport(TestReport):
    """Minimal instantiable TestReport for exercising set_reviewer."""

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
    tr.reviewer = ""
    return tr


TEMPLATE = """\
METADATA:
=========
Packager: slemke@suse.com
Bugs: 12345
{reviewer_line}
Repository: http://example/
"""


def _write(tmp_path: Path, reviewer_line: str) -> Path:
    p = tmp_path / "log"
    p.write_text(TEMPLATE.format(reviewer_line=reviewer_line))
    return p


def test_replaces_existing_reviewer_value(mock_config, tmp_path):
    p = _write(tmp_path, "Test Plan Reviewer: #maintenance")
    tr = _report(mock_config, p)

    tr.set_reviewer("alice")

    text = p.read_text()
    assert "Test Plan Reviewer: alice" in text
    assert "#maintenance" not in text
    assert tr.reviewer == "alice"


def test_replaces_empty_reviewer_value(mock_config, tmp_path):
    p = _write(tmp_path, "Test Plan Reviewer:")
    tr = _report(mock_config, p)

    tr.set_reviewer("alice")

    assert "Test Plan Reviewer: alice" in p.read_text()


def test_normalizes_whitespace_and_old_suggested_phrasing(mock_config, tmp_path):
    p = _write(tmp_path, "Suggested Test Plan Reviewers:    old.user@example.com")
    tr = _report(mock_config, p)

    tr.set_reviewer("alice")

    text = p.read_text()
    assert "Test Plan Reviewer: alice" in text
    assert "Suggested" not in text
    assert "old.user@example.com" not in text


def test_strips_surrounding_whitespace_from_name(mock_config, tmp_path):
    p = _write(tmp_path, "Test Plan Reviewer: x")
    tr = _report(mock_config, p)

    tr.set_reviewer("  alice  ")

    assert "Test Plan Reviewer: alice\n" in p.read_text()
    assert tr.reviewer == "alice"


def test_only_other_lines_unchanged(mock_config, tmp_path):
    p = _write(tmp_path, "Test Plan Reviewer: old")
    original = p.read_text()
    tr = _report(mock_config, p)

    tr.set_reviewer("alice")

    new = p.read_text()
    # Exactly one line differs.
    diff = [
        (a, b)
        for a, b in zip(original.splitlines(), new.splitlines(), strict=True)
        if a != b
    ]
    assert diff == [("Test Plan Reviewer: old", "Test Plan Reviewer: alice")]


def test_missing_line_raises(mock_config, tmp_path):
    p = tmp_path / "log"
    p.write_text("METADATA:\nPackager: x\nRepository: y\n")
    original = p.read_text()
    tr = _report(mock_config, p)

    with pytest.raises(TemplateFormatError):
        tr.set_reviewer("alice")

    assert p.read_text() == original
    assert tr.reviewer == ""


def test_empty_name_raises_and_leaves_file(mock_config, tmp_path):
    p = _write(tmp_path, "Test Plan Reviewer: old")
    original = p.read_text()
    tr = _report(mock_config, p)

    with pytest.raises(ValueError, match="non-empty"):
        tr.set_reviewer("   ")

    assert p.read_text() == original
    assert tr.reviewer == ""


def test_no_path_raises(mock_config):
    tr = _report(mock_config, None)

    with pytest.raises(RuntimeError):
        tr.set_reviewer("alice")
