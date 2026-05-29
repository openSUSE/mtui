"""Tests for the `approve` command (BaseApiCall dispatch)."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock, patch

import pytest

from mtui.commands.apicall import Approve
from mtui.messages import TestReportNotLoadedError
from mtui.types import RequestKind


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.metadata.__bool__ = lambda self: True
    # MAINTENANCE -> _is_gitea_workflow returns False -> osc path
    p.metadata.rrid = MagicMock()
    p.metadata.rrid.kind = RequestKind.MAINTENANCE
    p.metadata.rrid.maintenance_id = "12345"
    p.metadata.rrid.review_id = "67890"
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_approve_osc_branch_calls_osc_approve(mock_config):
    prompt = _prompt()
    args = Namespace(group=["qam-sle"], user="")

    with patch("mtui.commands.apicall.OSC") as osc_cls:
        Approve(args, mock_config, MagicMock(), prompt)()

    osc_cls.assert_called_once_with(mock_config, prompt.metadata.rrid)
    osc_cls.return_value.approve.assert_called_once_with(["qam-sle"])


def test_approve_without_metadata_raises(mock_config):
    prompt = _prompt()
    prompt.metadata.__bool__ = lambda self: False
    args = Namespace(group=None, user="")

    with pytest.raises(TestReportNotLoadedError):
        Approve(args, mock_config, MagicMock(), prompt)()
