"""Tests for the `config` command (show/set)."""

from __future__ import annotations

from argparse import Namespace
from unittest.mock import MagicMock

from mtui.commands.config import Config
from mtui.support.config import ConfigOption


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


def test_config_show_named_attribute(mock_config):
    sys_mock = MagicMock()
    mock_config.data = [ConfigOption("session_user", ("mtui", "user"), "x")]
    mock_config.session_user = "x"
    args = Namespace(func="show", attributes=["session_user"])

    Config(args, mock_config, sys_mock, _prompt())()

    written = "".join(c.args[0] for c in sys_mock.stdout.write.call_args_list)
    assert "session_user" in written
    assert "'x'" in written


def test_config_show_no_attributes_lists_all(mock_config):
    """Regression: `config show` with no attributes must enumerate every
    declared ConfigOption. Previously crashed with
    ``TypeError: 'ConfigOption' object is not subscriptable`` because the
    show() handler treated ``self.config.data`` entries as tuples.
    """
    sys_mock = MagicMock()
    mock_config.data = [
        ConfigOption("session_user", ("mtui", "user"), "alice"),
        ConfigOption("connection_timeout", ("mtui", "connection_timeout"), 300),
    ]
    mock_config.session_user = "alice"
    mock_config.connection_timeout = 300
    args = Namespace(func="show", attributes=[])

    Config(args, mock_config, sys_mock, _prompt())()

    written = "".join(c.args[0] for c in sys_mock.stdout.write.call_args_list)
    assert "session_user" in written
    assert "connection_timeout" in written
    assert "'alice'" in written
    assert "300" in written


def test_config_set_new_attribute_assigns_string(mock_config):
    sys_mock = MagicMock()
    # Force AttributeError on the unknown key by using a fresh object via spec.
    cfg = type("C", (), {})()
    args = Namespace(func="set", attribute="unknown_key", value="hello")

    Config(args, cfg, sys_mock, _prompt())()

    assert getattr(cfg, "unknown_key") == "hello"  # noqa: B009
