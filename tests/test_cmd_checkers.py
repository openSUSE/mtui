"""Tests for the ``checkers`` command (TeReGen-backed build-check results)."""

from __future__ import annotations

import io
from argparse import Namespace
from unittest.mock import MagicMock, patch

from mtui.commands.checkers import Checkers


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    p.metadata.rrid = "SUSE:SLFO:1.2:5444"
    return p


def _sysmock() -> MagicMock:
    s = MagicMock()
    s.stdout = io.StringIO()
    return s


def _args() -> Namespace:
    return Namespace(template=None, all_templates=False)


def test_checkers_lists_results(mock_config):
    prompt = _prompt()
    sysmock = _sysmock()

    with patch("mtui.commands.checkers.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.checkers.return_value = [
            {"name": "source_validator", "status": "passed"},
            {"name": "rpmlint", "status": "failed"},
        ]
        Checkers(_args(), mock_config, sysmock, prompt)()

    teregen.checkers.assert_called_once()
    out = sysmock.stdout.getvalue()
    assert "source_validator" in out
    assert "rpmlint" in out


def test_checkers_reports_none(mock_config):
    prompt = _prompt()
    sysmock = _sysmock()

    with patch("mtui.commands.checkers.TeReGen") as teregen_cls:
        teregen = teregen_cls.return_value
        teregen.checkers.return_value = None
        Checkers(_args(), mock_config, sysmock, prompt)()

    assert "No checker results" in sysmock.stdout.getvalue()
