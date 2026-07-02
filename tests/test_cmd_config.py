"""Tests for the `config` command (show/set)."""

from __future__ import annotations

from argparse import Namespace
from pathlib import Path
from unittest.mock import MagicMock

import pytest

from mtui.commands.config import Config
from mtui.support.config import Config as RealConfig
from mtui.support.config import ConfigOption


def _prompt() -> MagicMock:
    p = MagicMock()
    p.metadata = MagicMock()
    p.display = MagicMock()
    p.targets = MagicMock()
    return p


@pytest.fixture
def real_config(tmp_path) -> RealConfig:
    """A real Config (empty INI) so `set` sees the declared ConfigOptions."""
    cfg_file = tmp_path / "mtui.cfg"
    cfg_file.write_text("")
    return RealConfig(cfg_file)


def _run_set(config, attribute: str, value: str) -> MagicMock:
    sys_mock = MagicMock()
    args = Namespace(func="set", attribute=attribute, value=value)
    Config(args, config, sys_mock, _prompt())()
    return sys_mock


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


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        # Regression: the old coercion was ``val == "True"``, so every
        # spelling but the literal "True" -- including "true" -- set False.
        ("true", True),
        ("True", True),
        ("yes", True),
        ("on", True),
        ("1", True),
        ("false", False),
        ("False", False),
        ("no", False),
        ("0", False),
    ],
)
def test_config_set_bool_parses_ini_spellings(real_config, value, expected):
    _run_set(real_config, "use_keyring", value)

    assert real_config.use_keyring is expected


def test_config_set_bool_rejects_garbage(real_config, caplog):
    with caplog.at_level("ERROR", logger="mtui.commands.config"):
        _run_set(real_config, "use_keyring", "maybe")

    assert real_config.use_keyring is False  # default, unchanged
    assert any(
        "use_keyring" in r.message and "maybe" in r.message for r in caplog.records
    )


def test_config_set_int_accepts_integer(real_config):
    _run_set(real_config, "connection_timeout", "600")

    assert real_config.connection_timeout == 600


def test_config_set_int_rejects_non_integer(real_config, caplog):
    """`config set connection_timeout abc` must not store the string 'abc'."""
    with caplog.at_level("ERROR", logger="mtui.commands.config"):
        _run_set(real_config, "connection_timeout", "abc")

    assert real_config.connection_timeout == 300  # default, unchanged
    assert any(
        "connection_timeout" in r.message and "abc" in r.message for r in caplog.records
    )


def test_config_set_getint_option_rejects_non_integer(real_config, caplog):
    """Options declared with the ``getint`` getter reject non-integers too."""
    with caplog.at_level("ERROR", logger="mtui.commands.config"):
        _run_set(real_config, "refhosts_https_expiration", "xyz")

    assert real_config.refhosts_https_expiration == 3600 * 12
    assert any(
        "refhosts_https_expiration" in r.message and "expected an integer" in r.message
        for r in caplog.records
    )


def test_config_set_ssl_verify_false_disables_verification(real_config):
    _run_set(real_config, "ssl_verify", "false")

    assert real_config.ssl_verify is False


def test_config_set_ssl_verify_bogus_value_rejected(real_config, caplog):
    """'false1' is neither a boolean nor a CA bundle: reject, keep the default.

    Previously the string was stored verbatim once ssl_verify held a str,
    making every later HTTP call die with a requests-level OSError about
    an invalid CA path.
    """
    before = real_config.ssl_verify
    with caplog.at_level("ERROR", logger="mtui.commands.config"):
        _run_set(real_config, "ssl_verify", "false1")

    assert real_config.ssl_verify == before  # unchanged (verifying default)
    assert any(
        "ssl_verify" in r.message and "false1" in r.message for r in caplog.records
    )


def test_config_set_ssl_verify_ca_bundle_path(real_config, tmp_path):
    bundle = tmp_path / "ca.pem"
    bundle.write_text("dummy")

    _run_set(real_config, "ssl_verify", str(bundle))

    assert real_config.ssl_verify == str(bundle)


def test_config_set_str_option_stays_verbatim(real_config):
    sys_mock = _run_set(real_config, "session_user", "bob")

    assert real_config.session_user == "bob"
    written = "".join(c.args[0] for c in sys_mock.stdout.write.call_args_list)
    assert "session_user" in written


def test_config_set_path_option_applies_fixup(real_config):
    """Declared path options run their ``expanduser`` fixup, like the INI."""
    _run_set(real_config, "template_dir", "~/templates")

    assert real_config.template_dir == Path.home() / "templates"


def test_config_set_undeclared_attribute_keeps_current_type(real_config):
    """Attributes set externally (no ConfigOption) keep the legacy coercion."""
    real_config.distro = "sle"

    _run_set(real_config, "distro", "opensuse")

    assert real_config.distro == "opensuse"


def test_config_set_fixup_nonvalueerror_is_normalized(real_config, caplog):
    """A fixup raising something other than ValueError is one rejection path.

    Fixups are arbitrary callables; the command must not crash on e.g. a
    RuntimeError but reject the value with the same single error shape and
    leave the attribute unchanged.
    """

    def _boom(_raw):
        raise RuntimeError("parser bug")

    real_config.data = [ConfigOption("session_user", ("mtui", "user"), "alice", _boom)]
    before = real_config.session_user
    with caplog.at_level("ERROR", logger="mtui.commands.config"):
        _run_set(real_config, "session_user", "bob")

    assert real_config.session_user == before
    assert any(
        "session_user" in r.message and "parser bug" in r.message
        for r in caplog.records
    )


def test_config_set_brand_new_attribute_infers_type(real_config):
    """A never-declared, never-set attribute infers bool/int/str from the raw."""
    _run_set(real_config, "brand_new_flag", "True")
    assert real_config.brand_new_flag is True

    _run_set(real_config, "brand_new_count", "42")
    assert real_config.brand_new_count == 42

    _run_set(real_config, "brand_new_name", "hello")
    assert real_config.brand_new_name == "hello"


def test_config_set_undeclared_bool_rejects_garbage(real_config, caplog):
    """An undeclared attribute keeps its current type; bad values are rejected."""
    real_config.custom_flag = True  # not in the declared option table
    with caplog.at_level("ERROR", logger="mtui.commands.config"):
        _run_set(real_config, "custom_flag", "maybe")

    assert real_config.custom_flag is True  # unchanged
    assert any(
        "custom_flag" in r.message and "bool" in r.message for r in caplog.records
    )
