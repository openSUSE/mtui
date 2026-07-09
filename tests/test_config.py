from argparse import Namespace
from pathlib import Path

import pytest

from mtui.support import config


def test_default_config(tmpdir):
    """Test default config."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("")
    cfg = config.Config(config_file)
    assert cfg.connection_timeout == 300


def test_override_default_config(tmpdir):
    """Test override config."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("[mtui]\nconnection_timeout = 600\n")
    cfg = config.Config(config_file)
    assert cfg.connection_timeout == 600


def test_connection_timeout_from_connection_section(tmpdir):
    """connection_timeout is read from the [connection] section."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("[connection]\nconnection_timeout = 45\n")
    cfg = config.Config(config_file)
    assert cfg.connection_timeout == 45


def test_connection_timeout_connection_section_wins(tmpdir):
    """[connection] takes precedence over the legacy [mtui] section."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text(
        "[mtui]\nconnection_timeout = 600\n[connection]\nconnection_timeout = 45\n"
    )
    cfg = config.Config(config_file)
    assert cfg.connection_timeout == 45


def test_path_options_expand_tilde(tmpdir):
    """``~``-prefixed path options expand to the user's home directory."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text(
        "[refhosts]\npath = ~/qam/refhosts.yml\n[target]\ntempdir = ~/scratch\n"
    )
    cfg = config.Config(config_file)
    assert cfg.refhosts_path == Path.home() / "qam/refhosts.yml"
    assert cfg.target_tempdir == Path.home() / "scratch"
    # An absolute path is passed through unchanged.
    abs_cfg = Path(tmpdir.join("abs.cfg"))
    abs_cfg.write_text("[refhosts]\npath = /usr/share/refhosts.yml\n")
    assert config.Config(abs_cfg).refhosts_path == Path("/usr/share/refhosts.yml")


def test_merge_args(tmpdir):
    """Test merge_args."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("")
    cfg = config.Config(config_file)
    args = Namespace(
        template_dir="/cmd/template_dir",
        connection_timeout=1200,
        gitea_token="cmd_gitea_token",
    )
    cfg.merge_args(args)
    assert cfg.template_dir == "/cmd/template_dir"
    assert cfg.connection_timeout == 1200
    assert cfg.qem_dashboard_api == "http://dashboard.qam.suse.de/api"
    assert cfg.gitea_token == "cmd_gitea_token"


def test_ssh_strict_host_key_checking_default(tmpdir):
    """Default value preserves backward-compatible auto-add behaviour."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("")
    cfg = config.Config(config_file)
    assert cfg.ssh_strict_host_key_checking == "auto_add"


def test_ssh_strict_host_key_checking_override(tmpdir):
    """[connection] section in INI overrides the default."""
    config_file = Path(tmpdir.join("test.cfg"))
    config_file.write_text("[connection]\nssh_strict_host_key_checking = reject\n")
    cfg = config.Config(config_file)
    assert cfg.ssh_strict_host_key_checking == "reject"


def test_mtui_conf_env_var_selects_configfile(tmpdir, monkeypatch):
    """When ``path`` is ``None`` and ``MTUI_CONF`` is set, that file is read."""
    cfg_file = Path(tmpdir.join("via_env.cfg"))
    cfg_file.write_text("[mtui]\nconnection_timeout = 777\n")
    monkeypatch.setenv("MTUI_CONF", str(cfg_file))
    cfg = config.Config(None)
    assert cfg.configfiles == [cfg_file]
    assert cfg.connection_timeout == 777


def test_default_configfiles_when_no_path_no_env(monkeypatch):
    """No ``path`` and no ``MTUI_CONF`` falls back to the canonical pair."""
    monkeypatch.delenv("MTUI_CONF", raising=False)
    cfg = config.Config(None)
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
        cfg = config.Config(cfg_file)
    assert any(
        "MissingSectionHeader" in r.message or "section" in r.message.lower()
        for r in caplog.records
    )
    # Defaults still applied; broken file did not poison subsequent steps.
    assert cfg.connection_timeout == 300


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
        cfg = config.Config(cfg_file)
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
        cfg = config.Config(cfg_file)
    assert getattr(cfg, attr) == expected_default
    assert any(attr in r.message for r in caplog.records), (
        f"expected an ERROR log line mentioning {attr!r}; "
        f"got: {[r.message for r in caplog.records]}"
    )


# --- [mtui] ssl_verify: parse-time validation + system-bundle default ---


def test_ssl_verify_typo_logs_one_clean_error_and_falls_back(
    tmpdir, caplog, monkeypatch
):
    """The reproduced field bug: ``ssl_verify = false1``.

    Previously the string flowed verbatim into ``requests`` and died at the
    first HTTPS call with ``OSError: ... invalid path: false1``. Now it is
    rejected at parse time with ONE clean ERROR line (no traceback) naming
    the accepted forms, and verification falls back to the secure default.
    """
    monkeypatch.setattr(config, "system_ca_bundle", lambda: None)
    cfg_file = Path(tmpdir.join("ssl.cfg"))
    cfg_file.write_text("[mtui]\nssl_verify = false1\n")
    with caplog.at_level("ERROR", logger="mtui.config"):
        cfg = config.Config(cfg_file)
    assert cfg.ssl_verify is True
    errors = [r for r in caplog.records if "ssl_verify" in r.message]
    assert len(errors) == 1
    assert "false1" in errors[0].message
    assert "true/yes/on/1" in errors[0].message  # accepted forms are named
    assert errors[0].exc_info is None  # one line, no traceback


def test_ssl_verify_unset_prefers_system_bundle(tmpdir, monkeypatch):
    """Unset, the policy is the distribution CA bundle when one exists.

    requests validates against its bundled certifi CAs, not the system
    trust store, so without this a system-installed internal CA (e.g. the
    SUSE root) is invisible when mtui runs from a git checkout.
    """
    monkeypatch.setattr(config, "system_ca_bundle", lambda: "/etc/ssl/ca-bundle.pem")
    cfg_file = Path(tmpdir.join("ssl.cfg"))
    cfg_file.write_text("")
    assert config.Config(cfg_file).ssl_verify == "/etc/ssl/ca-bundle.pem"


def test_ssl_verify_unset_defaults_true_without_system_bundle(tmpdir, monkeypatch):
    monkeypatch.setattr(config, "system_ca_bundle", lambda: None)
    cfg_file = Path(tmpdir.join("ssl.cfg"))
    cfg_file.write_text("")
    assert config.Config(cfg_file).ssl_verify is True


def test_ssl_verify_explicit_existing_bundle_is_kept(tmpdir):
    ca = Path(tmpdir.join("ca.pem"))
    ca.write_text("dummy")
    cfg_file = Path(tmpdir.join("ssl.cfg"))
    cfg_file.write_text(f"[mtui]\nssl_verify = {ca}\n")
    assert config.Config(cfg_file).ssl_verify == str(ca)


def test_ssl_verify_false_still_disables(tmpdir):
    cfg_file = Path(tmpdir.join("ssl.cfg"))
    cfg_file.write_text("[mtui]\nssl_verify = false\n")
    assert config.Config(cfg_file).ssl_verify is False


def test_ssl_verify_explicit_true_equals_unset_default(tmpdir, monkeypatch):
    """Writing out the documented default must not change behaviour.

    With a system bundle present, both an unset option and an explicit
    ``true`` prefer it — otherwise users who wrote ``ssl_verify = true``
    would keep the internal-CA verification failure the default avoids.
    """
    monkeypatch.setattr(config, "system_ca_bundle", lambda: "/etc/ssl/ca-bundle.pem")
    cfg_file = Path(tmpdir.join("ssl.cfg"))
    cfg_file.write_text("[mtui]\nssl_verify = true\n")
    assert config.Config(cfg_file).ssl_verify == "/etc/ssl/ca-bundle.pem"


def test_ssl_verify_blank_treated_as_unset_stays_secure(tmpdir, caplog, monkeypatch):
    """A blank value must NOT disable TLS verification.

    ``ssl_verify =`` is almost always an unfinished edit, not intent.
    Historically the blank passed through as a falsy ``verify`` and turned
    certificate verification off for every HTTPS call (requests treats
    ""/False as CERT_NONE). It now falls back to the secure default, with
    a warning; only an explicit false spelling disables verification.
    """
    monkeypatch.setattr(config, "system_ca_bundle", lambda: None)
    cfg_file = Path(tmpdir.join("ssl.cfg"))
    cfg_file.write_text("[mtui]\nssl_verify =\n")
    with caplog.at_level("WARNING", logger="mtui.config"):
        cfg = config.Config(cfg_file)
    assert cfg.ssl_verify is True
    assert any("blank ssl_verify" in r.message for r in caplog.records)


def test_ssl_verify_blank_prefers_system_bundle(tmpdir, caplog, monkeypatch):
    """The blank fallback is the same default as an unset option."""
    monkeypatch.setattr(config, "system_ca_bundle", lambda: "/etc/ssl/ca-bundle.pem")
    cfg_file = Path(tmpdir.join("ssl.cfg"))
    cfg_file.write_text("[mtui]\nssl_verify =  \n")
    with caplog.at_level("WARNING", logger="mtui.config"):
        cfg = config.Config(cfg_file)
    assert cfg.ssl_verify == "/etc/ssl/ca-bundle.pem"


def test_unexpected_fixup_error_keeps_traceback(tmpdir, caplog):
    """Non-ValueError fixup failures are parser bugs: the stack is kept.

    Only validation rejections (ValueError) get the clean one-line
    treatment; anything else logs with the full traceback so a bug report
    from a normal run contains the stack.
    """
    cfg_file = Path(tmpdir.join("boom.cfg"))
    cfg_file.write_text("[mtui]\nconnection_timeout = 5\n")
    cfg = config.Config(cfg_file)

    def _boom(_raw):
        raise RuntimeError("parser bug")

    cfg.data = [
        config.ConfigOption(
            "connection_timeout",
            ("mtui", "connection_timeout"),
            300,
            _boom,
            cfg.config.get,
        )
    ]
    with caplog.at_level("ERROR", logger="mtui.config"):
        cfg._parse_config()
    assert cfg.connection_timeout == 300
    errors = [r for r in caplog.records if "connection_timeout" in r.message]
    assert len(errors) == 1
    assert errors[0].exc_info is not None  # traceback preserved


# --- Endpoint URLs / positive ints / install_logs: parse-time validation ---


@pytest.mark.parametrize(
    ("ini_section", "ini_key", "ini_value", "attr", "expected_default"),
    [
        # URL-typed endpoints. The non-numeric port is the reproduced field
        # bug: it used to survive startup and crash the first query deep in
        # requests as an unhandled InvalidURL.
        (
            "openqa",
            "openqa",
            "https://openqa.suse.de:44e3",
            "openqa_instance",
            "https://openqa.suse.de",
        ),
        (
            "openqa",
            "baremetal",
            "openqa.qam.suse.cz",  # missing scheme
            "openqa_instance_baremetal",
            "http://openqa.qam.suse.cz",
        ),
        (
            "qem_dashboard",
            "api",
            "ftp://dashboard.qam.suse.de/api",  # unsupported scheme
            "qem_dashboard_api",
            "http://dashboard.qam.suse.de/api",
        ),
        (
            "teregen",
            "api",
            "https://",  # empty host
            "teregen_api",
            "https://qam.suse.de/api/v1",
        ),
        (
            "refhosts",
            "https_uri",
            "qam.suse.de/refhosts/refhosts.yml",  # missing scheme
            "refhosts_https_uri",
            "https://qam.suse.de/refhosts/refhosts.yml",
        ),
        # Positive-int options: zero/negative pass int() but break downstream
        # opaquely (a negative connection_timeout reaches paramiko and shows
        # up as a bogus 'Error reading SSH protocol banner' per host).
        ("connection", "connection_timeout", "-5", "connection_timeout", 300),
        ("connection", "connection_timeout", "0", "connection_timeout", 300),
        ("lock", "wait_poll", "0", "lock_wait_poll", 15),
        ("mcp", "session_cap", "-1", "mcp_session_cap", 32),
        ("mcp", "session_idle_timeout", "0", "mcp_session_idle_timeout", 1800),
        ("refhosts", "https_expiration", "-3600", "refhosts_https_expiration", 43200),
        # install_logs is joined as template_dir / <rrid> / install_logs and
        # created with mkdir(parents=False): a nested value crashed AFTER a
        # successful svn checkout, an absolute one replaced the whole base.
        ("mtui", "install_logs", "logs/zypper", "install_logs", Path("install_logs")),
        ("mtui", "install_logs", "/srv/logs", "install_logs", Path("install_logs")),
    ],
)
def test_invalid_value_logs_one_error_and_falls_back(
    tmpdir, caplog, ini_section, ini_key, ini_value, attr, expected_default
):
    """Each hardened option rejects bad values with ONE clean ERROR line."""
    cfg_file = Path(tmpdir.join("validated.cfg"))
    cfg_file.write_text(f"[{ini_section}]\n{ini_key} = {ini_value}\n")
    with caplog.at_level("ERROR", logger="mtui.config"):
        cfg = config.Config(cfg_file)
    assert getattr(cfg, attr) == expected_default
    errors = [r for r in caplog.records if attr in r.message]
    assert len(errors) == 1, (
        f"expected exactly one ERROR naming {attr!r}; "
        f"got: {[r.message for r in caplog.records]}"
    )
    assert ini_value in errors[0].message  # the offending value is shown
    assert errors[0].exc_info is None  # one line, no traceback


@pytest.mark.parametrize(
    ("ini_section", "ini_key", "ini_value", "attr", "expected"),
    [
        (
            "openqa",
            "openqa",
            "https://openqa.opensuse.org",
            "openqa_instance",
            "https://openqa.opensuse.org",
        ),
        (
            "openqa",
            "baremetal",
            "http://openqa.example.com:9526",  # numeric port is fine
            "openqa_instance_baremetal",
            "http://openqa.example.com:9526",
        ),
        (
            "teregen",
            "api",
            "https://qam.example.com/api/v1",
            "teregen_api",
            "https://qam.example.com/api/v1",
        ),
        ("connection", "connection_timeout", "45", "connection_timeout", 45),
        ("lock", "wait_poll", "30", "lock_wait_poll", 30),
        ("refhosts", "https_expiration", "600", "refhosts_https_expiration", 600),
        ("mtui", "install_logs", "zypper_logs", "install_logs", Path("zypper_logs")),
    ],
)
def test_valid_value_is_kept(
    tmpdir, caplog, ini_section, ini_key, ini_value, attr, expected
):
    """Good values pass the new validations unchanged (no ERROR logged)."""
    cfg_file = Path(tmpdir.join("validated.cfg"))
    cfg_file.write_text(f"[{ini_section}]\n{ini_key} = {ini_value}\n")
    with caplog.at_level("ERROR", logger="mtui.config"):
        cfg = config.Config(cfg_file)
    assert getattr(cfg, attr) == expected
    assert not [r for r in caplog.records if attr in r.message]


def test_documented_zero_semantics_are_untouched(tmpdir):
    """``lock.wait`` (fail-fast) and ``lock.stale_age`` (disable) keep 0."""
    cfg_file = Path(tmpdir.join("locks.cfg"))
    cfg_file.write_text("[lock]\nwait = 0\nstale_age = 0\n")
    cfg = config.Config(cfg_file)
    assert cfg.lock_wait == 0
    assert cfg.lock_stale_age == 0


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("https://openqa.suse.de", "https://openqa.suse.de"),
        ("http://dashboard.qam.suse.de/api", "http://dashboard.qam.suse.de/api"),
        # Surrounding whitespace is stripped.
        ("  https://qam.suse.de/api/v1  ", "https://qam.suse.de/api/v1"),
        # Numeric ports and userinfo are accepted.
        ("https://openqa.suse.de:443", "https://openqa.suse.de:443"),
        (
            "http://user:pw@openqa.example.com:80/x",
            "http://user:pw@openqa.example.com:80/x",
        ),
    ],
)
def test_parse_base_url_accepts(raw, expected):
    assert config._parse_base_url(raw) == expected


@pytest.mark.parametrize(
    "raw",
    [
        "https://openqa.suse.de:44e3",  # non-numeric port
        "https://openqa.suse.de:-1",  # out-of-range port
        "openqa.suse.de",  # no scheme
        "ftp://openqa.suse.de",  # unsupported scheme
        "https://",  # empty host
        "https:///api",  # empty host, path only
        "http://[::1",  # unparsable IPv6 netloc
        "",
        "   ",
    ],
)
def test_parse_base_url_rejects(raw):
    with pytest.raises(ValueError, match="http"):
        config._parse_base_url(raw)


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

    cfg = config.Config(fixture)

    # String values across multiple sections.
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
