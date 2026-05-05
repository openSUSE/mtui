from argparse import Namespace
from pathlib import Path

import pytest

from mtui import config
from mtui.messages import InvalidLocationError
from mtui.refhost import RefhostsResolveFailedError


class MockRefhosts:
    def __init__(self, config):
        pass

    def check_location_sanity(self, location):
        pass

    def __call__(self, config):
        return self


class RaisingRefhosts:
    """Refhosts double whose ``check_location_sanity`` raises on demand."""

    exc: Exception | None = None

    def __init__(self, config):
        pass

    def check_location_sanity(self, location):
        if self.exc is not None:
            raise self.exc

    def __call__(self, config):
        return self


def test_default_config(tmpdir):
    """Test default config."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("")
    cfg = config.Config(config_file, refhosts=MockRefhosts)
    assert cfg.location == "default"
    assert cfg.connection_timeout == 300


def test_override_default_config(tmpdir):
    """Test override config."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text(
        "[mtui]\nlocation = test_location\nconnection_timeout = 600\n"
    )
    cfg = config.Config(config_file, refhosts=MockRefhosts)
    assert cfg.location == "test_location"
    assert cfg.connection_timeout == 600


def test_merge_args(tmpdir):
    """Test merge_args."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("")
    cfg = config.Config(config_file, refhosts=MockRefhosts)
    args = Namespace(
        location="cmd_location",
        template_dir="/cmd/template_dir",
        connection_timeout=1200,
        gitea_token="cmd_gitea_token",
    )
    cfg.merge_args(args)
    assert cfg.location == "cmd_location"
    assert cfg.template_dir == "/cmd/template_dir"
    assert cfg.connection_timeout == 1200
    assert cfg.qem_dashboard_api == "http://dashboard.qam.suse.de/api"
    assert cfg.gitea_token == "cmd_gitea_token"


def test_ssh_strict_host_key_checking_default(tmpdir):
    """Default value preserves backward-compatible auto-add behaviour."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("")
    cfg = config.Config(config_file, refhosts=MockRefhosts)
    assert cfg.ssh_strict_host_key_checking == "auto_add"


def test_ssh_strict_host_key_checking_override(tmpdir):
    """[connection] section in INI overrides the default."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("[connection]\nssh_strict_host_key_checking = reject\n")
    cfg = config.Config(config_file, refhosts=MockRefhosts)
    assert cfg.ssh_strict_host_key_checking == "reject"


def test_mtui_conf_env_var_selects_configfile(tmpdir, monkeypatch):
    """When ``path`` is ``None`` and ``MTUI_CONF`` is set, that file is read."""
    cfg_file = Path(tmpdir.join("via_env.cfg"))
    cfg_file.write_text("[mtui]\nconnection_timeout = 777\n")
    monkeypatch.setenv("MTUI_CONF", str(cfg_file))
    cfg = config.Config(None, refhosts=MockRefhosts)
    assert cfg.configfiles == [cfg_file]
    assert cfg.connection_timeout == 777


def test_default_configfiles_when_no_path_no_env(monkeypatch):
    """No ``path`` and no ``MTUI_CONF`` falls back to the canonical pair."""
    monkeypatch.delenv("MTUI_CONF", raising=False)
    cfg = config.Config(None, refhosts=MockRefhosts)
    assert cfg.configfiles == [
        Path("/etc/mtui.cfg"),
        Path("~/.mtuirc").expanduser(),
    ]


def test_read_logs_and_swallows_configparser_error(tmpdir, caplog):
    """A malformed INI is logged at error and does not crash construction."""
    cfg_file = Path(tmpdir.join("broken.cfg"))
    # MissingSectionHeaderError: option line before any [section].
    cfg_file.write_text("connection_timeout = 42\n")
    with caplog.at_level("ERROR", logger="mtui.config"):
        cfg = config.Config(cfg_file, refhosts=MockRefhosts)
    assert any(
        "MissingSectionHeader" in r.message or "section" in r.message.lower()
        for r in caplog.records
    )
    # Defaults still applied; broken file did not poison subsequent steps.
    assert cfg.connection_timeout == 300


def test_location_setter_invalid_location_keeps_previous(tmpdir, caplog):
    """``InvalidLocationError`` is logged and the location is unchanged."""
    cfg_file = Path(tmpdir.join("loc.cfg"))
    cfg_file.write_text("")
    refhosts = RaisingRefhosts
    cfg = config.Config(cfg_file, refhosts=refhosts)
    assert cfg.location == "default"
    refhosts.exc = InvalidLocationError("nowhere", ["here", "there"])
    with caplog.at_level("ERROR", logger="mtui.config"):
        cfg.location = "nowhere"
    assert cfg.location == "default"
    assert any("nowhere" in r.message for r in caplog.records)
    refhosts.exc = None  # reset shared class state


def test_location_setter_resolve_failed_keeps_previous(tmpdir, caplog):
    """``RefhostsResolveFailedError`` is logged and the location is unchanged."""
    cfg_file = Path(tmpdir.join("loc.cfg"))
    cfg_file.write_text("")
    refhosts = RaisingRefhosts
    cfg = config.Config(cfg_file, refhosts=refhosts)
    refhosts.exc = RefhostsResolveFailedError()
    with caplog.at_level("ERROR", logger="mtui.config"):
        cfg.location = "anywhere"
    assert cfg.location == "default"
    assert any("refhosts.yml" in r.message for r in caplog.records)
    refhosts.exc = None


@pytest.mark.parametrize(
    ("ini_section", "ini_key", "ini_value"),
    [
        # ``connection_timeout`` reads via ``config.get`` then ``int`` as fixup.
        # The ``ValueError`` from ``int("abc")`` escapes ``_parse_config`` and
        # crashes ``Config.__init__``.
        ("mtui", "connection_timeout", "abc"),
    ],
)
def test_fixup_failure_propagates_and_aborts_construction(
    tmpdir, ini_section, ini_key, ini_value
):
    """Captures current behaviour: a bad ``int`` fixup crashes ``Config()``.

    FIXME (Phase 5b/C10): ``_parse_config`` only handles failures that
    originate inside ``_get_option``; ``fixup(val)`` is invoked outside the
    ``try`` so its exceptions are not converted into the documented
    "use default" behaviour. Replace with explicit per-option parse handling.
    """
    cfg_file = Path(tmpdir.join("typed.cfg"))
    cfg_file.write_text(f"[{ini_section}]\n{ini_key} = {ini_value}\n")
    with pytest.raises(ValueError, match="invalid literal for int"):
        config.Config(cfg_file, refhosts=MockRefhosts)


@pytest.mark.parametrize(
    ("ini_section", "ini_key", "ini_value", "attr", "expected_default"),
    [
        # ``getint`` raises inside ``_get_option`` → caught by outer
        # ``except Exception`` → default applied.
        ("refhosts", "https_expiration", "xyz", "refhosts_https_expiration", 3600 * 12),
        ("template", "smelt_threshold", "nope", "threshold", 10),
        # ``getboolean`` raises inside ``_get_option`` → same path.
        ("mtui", "chdir_to_template_dir", "maybe", "chdir_to_template_dir", False),
        ("mtui", "use_keyring", "perhaps", "use_keyring", False),
    ],
)
def test_typed_getter_failure_falls_back_to_default(
    tmpdir, ini_section, ini_key, ini_value, attr, expected_default
):
    """Captures current behaviour: typed-getter failures silently use the default.

    FIXME (Phase 5b/C10): ``_get_option`` lines 298-301 are intended to log an
    error before re-raising, but the format-string call
    ``msg.format((*secopt, self.configfiles))`` passes a single tuple to a
    template that expects three positional args, so the ``logger.error`` call
    itself raises and the user gets no diagnostic. Either way, the outer
    ``except Exception`` in ``_parse_config`` swallows everything and applies
    the default. The "log on failure" intent should be restored.
    """
    cfg_file = Path(tmpdir.join("typed.cfg"))
    cfg_file.write_text(f"[{ini_section}]\n{ini_key} = {ini_value}\n")
    cfg = config.Config(cfg_file, refhosts=MockRefhosts)
    assert getattr(cfg, attr) == expected_default
