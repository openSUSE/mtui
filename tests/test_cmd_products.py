"""Tests for the `list_products` command."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

import pytest

from mtui.commands.products import ListProducts
from mtui.support.messages import HostIsNotConnectedError


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_list_products_happy_calls_report_products(mock_config):
    prompt = _prompt()
    selected = MagicMock()
    prompt.targets.select.return_value = selected
    args = Namespace(hosts=None)

    ListProducts(args, mock_config, MagicMock(), prompt)()

    selected.report_products.assert_called_once_with(prompt.display.list_products)


def test_list_products_unknown_host_propagates(mock_config):
    prompt = _prompt()
    prompt.targets.select.side_effect = HostIsNotConnectedError("ghost")
    args = Namespace(hosts=["ghost"])
    with pytest.raises(HostIsNotConnectedError):
        ListProducts(args, mock_config, MagicMock(), prompt)()
