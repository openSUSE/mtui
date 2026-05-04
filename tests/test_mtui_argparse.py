"""Tests for the custom ArgumentParser and the optional argcomplete hook."""

from __future__ import annotations

import sys
from unittest.mock import MagicMock

import pytest

from mtui import argparse as mtui_argparse
from mtui.argparse import ArgsParseFailureError, ArgumentParser


def _make_parser():
    p = ArgumentParser(sys_=sys)
    p.add_argument("foo")
    return p


def test_parse_args_returns_namespace_without_argcomplete(monkeypatch):
    """parse_args must work normally when argcomplete is not installed."""
    monkeypatch.setattr(mtui_argparse, "_argcomplete", None)
    ns = _make_parser().parse_args(["bar"])
    assert ns.foo == "bar"


def test_parse_args_invokes_argcomplete_when_available(monkeypatch):
    """parse_args must call argcomplete.autocomplete(parser) when present."""
    fake = MagicMock()
    monkeypatch.setattr(mtui_argparse, "_argcomplete", fake)
    parser = _make_parser()
    parser.parse_args(["bar"])
    fake.autocomplete.assert_called_once_with(parser)


def test_exit_raises_args_parse_failure_error():
    """The custom .exit() must surface the status as a typed exception."""
    parser = _make_parser()
    with pytest.raises(ArgsParseFailureError) as info:
        parser.exit(7)
    assert info.value.status == 7
