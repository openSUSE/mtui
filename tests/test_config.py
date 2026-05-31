from argparse import Namespace
from pathlib import Path

import pytest

from mtui.hosts.refhost import RefhostsResolveFailedError
from mtui.support import config
from mtui.support.messages import InvalidLocationError


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
    ("ini_section", "ini_key", "ini_value", "attr", "expected_default"),
    [
        # ``connection_timeout`` reads via ``config.get`` then ``int`` as fixup.
        # The ``ValueError`` from ``int("abc")`` used to escape ``_parse_config``
        # and crash ``Config.__init__``; Phase 5b/C10 routes fixup failures
        # through the same log+default path as getter failures.
        ("mtui", "connection_timeout", "abc", "connection_timeout", 300),
    ],
)
def test_fixup_failure_logs_and_falls_back_to_default(
    tmpdir, caplog, ini_section, ini_key, ini_value, attr, expected_default
):
    """A bad ``int`` fixup is logged at ERROR and the default is applied.

    Phase 5b/C10: ``_parse_config`` now wraps both ``_get_option`` AND
    ``fixup(val)`` in the same ``try`` block, so a malformed
    ``connection_timeout`` no longer crashes startup.
    """
    cfg_file = Path(tmpdir.join("typed.cfg"))
    cfg_file.write_text(f"[{ini_section}]\n{ini_key} = {ini_value}\n")
    with caplog.at_level("ERROR", logger="mtui.config"):
        cfg = config.Config(cfg_file, refhosts=MockRefhosts)
    assert getattr(cfg, attr) == expected_default
    assert any(attr in r.message and ini_value in r.message for r in caplog.records), (
        f"expected an ERROR log line mentioning {attr!r} and the bad value "
        f"{ini_value!r}; got: {[r.message for r in caplog.records]}"
    )


@pytest.mark.parametrize(
    ("ini_section", "ini_key", "ini_value", "attr", "expected_default"),
    [
        # ``getint`` raises inside ``_get_option`` → caught in ``_parse_config``
        # → ERROR logged → default applied.
        ("refhosts", "https_expiration", "xyz", "refhosts_https_expiration", 3600 * 12),
        ("template", "smelt_threshold", "nope", "threshold", 10),
        # ``getboolean`` raises inside ``_get_option`` → same path.
        ("mtui", "chdir_to_template_dir", "maybe", "chdir_to_template_dir", False),
        ("mtui", "use_keyring", "perhaps", "use_keyring", False),
    ],
)
def test_typed_getter_failure_logs_and_falls_back_to_default(
    tmpdir, caplog, ini_section, ini_key, ini_value, attr, expected_default
):
    """Typed-getter failures are logged at ERROR and fall back to the default.

    Phase 5b/C10: the previously-broken ``logger.error`` call in
    ``_get_option`` is replaced by a working ``logger.error`` in
    ``_parse_config`` that names the option and the offending value.
    """
    cfg_file = Path(tmpdir.join("typed.cfg"))
    cfg_file.write_text(f"[{ini_section}]\n{ini_key} = {ini_value}\n")
    with caplog.at_level("ERROR", logger="mtui.config"):
        cfg = config.Config(cfg_file, refhosts=MockRefhosts)
    assert getattr(cfg, attr) == expected_default
    assert any(attr in r.message for r in caplog.records), (
        f"expected an ERROR log line mentioning {attr!r}; "
        f"got: {[r.message for r in caplog.records]}"
    )


# --- Realistic fixture round-trip (Phase 6 / D5) ---


def test_mtuirc_fixture_parses_all_sections():
    """The packaged ``tests/fixtures/mtuirc`` should parse end-to-end.

    Exercises a realistic multi-section INI (``[mtui]``, ``[openqa]``,
    ``[qem_dashboard]``, ``[gitea]``, ``[connection]``, ``[url]``) and
    asserts the parsed attributes match the fixture's values across all
    three value types the parser knows (str, int, bool).
    """
    fixture = Path(__file__).parent / "fixtures" / "mtuirc"
    assert fixture.is_file(), "tests/fixtures/mtuirc fixture missing"
    assert fixture.stat().st_size > 0, "tests/fixtures/mtuirc must be populated"

    cfg = config.Config(fixture, refhosts=MockRefhosts)

    # String values across multiple sections.
    assert cfg.location == "nuremberg"
    assert cfg.session_user == "qauser"
    assert cfg.openqa_instance == "https://openqa.example.com"
    assert cfg.openqa_install_distri == "sle"
    assert cfg.qem_dashboard_api == "https://dashboard.example.com/api"
    assert cfg.gitea_token == "ghp_fixture_token_for_tests"
    assert cfg.ssh_strict_host_key_checking == "warn"
    assert cfg.bugzilla_url == "https://bugzilla.example.com"
    assert cfg.reports_url == "https://qam.example.com/testreports"
    # Integer-typed option.
    assert cfg.connection_timeout == 450
    # Boolean-typed option.
    assert cfg.chdir_to_template_dir is True
