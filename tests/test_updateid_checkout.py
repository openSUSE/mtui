"""Tests for ``UpdateID._checkout`` error mapping."""

from __future__ import annotations

from errno import ENOENT
from unittest.mock import MagicMock

import pytest

from mtui.support.messages import (
    TemplateDirNotUsableError,
    TestReportNotLoadedError,
)
from mtui.test_reports.svn_io import TemplateIOError
from mtui.types import RequestReviewID
from mtui.types.updateid import UpdateID


class _StubUpdateID(UpdateID):
    """Concrete UpdateID: only _checkout is under test."""

    def make_testreport(self, *a, **kw):  # pragma: no cover - unused
        raise NotImplementedError


def test_checkout_unusable_template_dir_maps_to_not_loaded(mock_config, tmp_path):
    """An unusable template_dir surfaces as TestReportNotLoadedError.

    The svn checkout raises TemplateDirNotUsableError (e.g. a plain file in
    the way of the configured directory); _checkout must log it and map it to
    the same clean error as any other failed checkout instead of letting it
    escape as a raw traceback.
    """
    report = MagicMock()
    report.read.side_effect = TemplateIOError(ENOENT, "no template on disk")
    factory = MagicMock(return_value=report)
    vcs = MagicMock(
        side_effect=TemplateDirNotUsableError(tmp_path / "templates", "in the way")
    )
    mock_config.template_dir = tmp_path

    uid = _StubUpdateID(
        RequestReviewID("SUSE:Maintenance:1:1"),
        testreport_factory=factory,
        testreport_svn_checkout=vcs,
    )

    with pytest.raises(TestReportNotLoadedError):
        uid._checkout(mock_config, interactive=False)

    vcs.assert_called_once()
