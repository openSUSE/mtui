"""Tests for the native oscrc credential reader (mtui.data_sources.obs.oscrc)."""

from dataclasses import fields
from pathlib import Path

import pytest

from mtui.data_sources.obs import oscrc
from mtui.support.exceptions import ObsConfigError

API = "https://api.suse.de"


@pytest.fixture(autouse=True)
def _isolate_oscrc_discovery(monkeypatch, tmp_path):
    """Keep every test off the real oscrc locations.

    ``$OSC_CONFIG`` is cleared and both discovery locations
    (``$XDG_CONFIG_HOME/osc/oscrc`` via the module-level ``xdg_config_home``
    constant, and ``~/.oscrc`` via ``$HOME``) are redirected under
    ``tmp_path`` so the reader never touches the developer's files.
    """
    monkeypatch.delenv("OSC_CONFIG", raising=False)
    monkeypatch.setattr(oscrc, "xdg_config_home", str(tmp_path / "xdg"))
    monkeypatch.setenv("HOME", str(tmp_path / "home"))


def _write(
    tmp_path: Path, body: str, keyfile: Path | None = None, monkeypatch=None
) -> Path:
    """Write an oscrc file and point ``$OSC_CONFIG`` at it.

    Creates a dummy key file when referenced. When ``monkeypatch`` is given,
    ``$OSC_CONFIG`` is set so :func:`read_credentials` discovers this file.
    """
    if keyfile is not None:
        keyfile.write_text("dummy-key")
    path = tmp_path / "oscrc"
    path.write_text(body)
    if monkeypatch is not None:
        monkeypatch.setenv("OSC_CONFIG", str(path))
    return path


def test_reads_user_and_sshkey(tmp_path, monkeypatch):
    key = tmp_path / "id_ed25519"
    conf = _write(
        tmp_path,
        f"[general]\napiurl = {API}\n\n[{API}]\nuser = bob\nsshkey = {key}\n",
        keyfile=key,
        monkeypatch=monkeypatch,
    )
    creds = oscrc.read_credentials(API)
    assert creds.user == "bob"
    assert creds.sshkey_path == key
    assert creds.apiurl == API
    assert creds.source == str(conf)


def test_password_is_never_read_for_signature_target(tmp_path, monkeypatch):
    """`pass`/`passx` are ignored (no password ever enters memory)."""
    key = tmp_path / "id_ed25519"
    _write(
        tmp_path,
        f"[{API}]\nuser = bob\npass = s3cret\npassx = AAAA==\nsshkey = {key}\n",
        keyfile=key,
        monkeypatch=monkeypatch,
    )
    creds = oscrc.read_credentials(API)
    assert creds.user == "bob"
    # The dataclass structurally cannot carry a password.
    assert "pass" not in {f.name for f in fields(creds)}
    assert "s3cret" not in repr(creds)


def test_missing_conffile_raises(tmp_path, monkeypatch):
    monkeypatch.setenv("OSC_CONFIG", str(tmp_path / "nope"))
    with pytest.raises(ObsConfigError, match="not found"):
        oscrc.read_credentials(API)


def test_missing_section_raises(tmp_path, monkeypatch):
    _write(
        tmp_path,
        "[https://api.opensuse.org]\nuser = bob\n",
        monkeypatch=monkeypatch,
    )
    with pytest.raises(ObsConfigError, match="no \\[https://api.suse.de\\] section"):
        oscrc.read_credentials(API)


def test_missing_user_raises(tmp_path, monkeypatch):
    key = tmp_path / "k"
    _write(tmp_path, f"[{API}]\nsshkey = {key}\n", keyfile=key, monkeypatch=monkeypatch)
    with pytest.raises(ObsConfigError, match="no 'user'"):
        oscrc.read_credentials(API)


def test_missing_sshkey_raises(tmp_path, monkeypatch):
    _write(tmp_path, f"[{API}]\nuser = bob\n", monkeypatch=monkeypatch)
    with pytest.raises(ObsConfigError, match="no 'sshkey'"):
        oscrc.read_credentials(API)


def test_unsupported_credentials_manager_raises(tmp_path, monkeypatch):
    key = tmp_path / "k"
    _write(
        tmp_path,
        f"[{API}]\nuser = bob\nsshkey = {key}\n"
        "credentials_mgr_class = osc.credentials.KeyringCredentialsManager\n",
        keyfile=key,
        monkeypatch=monkeypatch,
    )
    with pytest.raises(ObsConfigError, match="credentials_mgr_class"):
        oscrc.read_credentials(API)


def test_agent_fingerprint_sshkey_is_accepted(tmp_path, monkeypatch):
    """A SHA256: fingerprint yields an agent-key credential (no file)."""
    _write(
        tmp_path,
        f"[{API}]\nuser = bob\nsshkey = SHA256:abc123\n",
        monkeypatch=monkeypatch,
    )
    creds = oscrc.read_credentials(API)
    assert creds.sshkey_fingerprint == "SHA256:abc123"
    assert creds.sshkey_path is None
    assert creds.user == "bob"


def test_pub_only_key_on_disk_is_accepted(tmp_path, monkeypatch):
    """A key present only as <name>.pub is accepted (agent holds the private)."""
    priv = tmp_path / "id_ed25519"
    (tmp_path / "id_ed25519.pub").write_text("ssh-ed25519 AAAA comment\n")
    _write(tmp_path, f"[{API}]\nuser = bob\nsshkey = {priv}\n", monkeypatch=monkeypatch)
    creds = oscrc.read_credentials(API)
    assert creds.sshkey_path == priv
    assert creds.sshkey_fingerprint is None


def test_missing_key_file_raises(tmp_path, monkeypatch):
    _write(
        tmp_path,
        f"[{API}]\nuser = bob\nsshkey = {tmp_path / 'absent'}\n",
        monkeypatch=monkeypatch,
    )
    with pytest.raises(ObsConfigError, match="does not exist"):
        oscrc.read_credentials(API)


def test_unparsable_oscrc_raises(tmp_path, monkeypatch):
    _write(tmp_path, "not = ini = at = all\n[unclosed\n", monkeypatch=monkeypatch)
    with pytest.raises(ObsConfigError, match="could not parse"):
        oscrc.read_credentials(API)


def test_parse_error_does_not_leak_secret(tmp_path, monkeypatch):
    """A malformed oscrc's source line (e.g. a password) is not surfaced."""
    # value before any section
    _write(tmp_path, "pass = SUPERSECRET\n[general]\n", monkeypatch=monkeypatch)
    with pytest.raises(ObsConfigError) as ei:
        oscrc.read_credentials(API)
    assert "SUPERSECRET" not in str(ei.value)


def test_sshkey_inherited_from_general(tmp_path, monkeypatch):
    """A key set only in [general] is inherited (osc FromParent parity)."""
    key = tmp_path / "id_ed25519"
    key.write_text("dummy-key")
    _write(
        tmp_path,
        f"[general]\nsshkey = {key}\n\n[{API}]\nuser = bob\n",
        monkeypatch=monkeypatch,
    )
    creds = oscrc.read_credentials(API)
    assert creds.sshkey_path == key
    assert creds.user == "bob"


def test_credentials_manager_inherited_from_general(tmp_path, monkeypatch):
    """A global keyring manager in [general] still fails closed."""
    key = tmp_path / "k"
    _write(
        tmp_path,
        f"[general]\ncredentials_mgr_class = osc.credentials.KeyringCredentialsManager\n"
        f"\n[{API}]\nuser = bob\nsshkey = {key}\n",
        keyfile=key,
        monkeypatch=monkeypatch,
    )
    with pytest.raises(ObsConfigError, match="credentials_mgr_class"):
        oscrc.read_credentials(API)


def test_trailing_slash_section_header_matches(tmp_path, monkeypatch):
    """A [https://api.suse.de/] header matches the api.suse.de apiurl."""
    key = tmp_path / "k"
    _write(
        tmp_path,
        f"[{API}/]\nuser = bob\nsshkey = {key}\n",
        keyfile=key,
        monkeypatch=monkeypatch,
    )
    creds = oscrc.read_credentials(API)
    assert creds.user == "bob"


def test_loose_permissions_warn(tmp_path, monkeypatch, caplog):
    key = tmp_path / "k"
    conf = _write(
        tmp_path,
        f"[{API}]\nuser = bob\nsshkey = {key}\n",
        keyfile=key,
        monkeypatch=monkeypatch,
    )
    conf.chmod(0o644)
    with caplog.at_level("WARNING"):
        oscrc.read_credentials(API)
    assert any("group/world-accessible" in r.message for r in caplog.records)


@pytest.mark.parametrize(
    ("value", "expected_template"),
    [
        ("id_ed25519", "~/.ssh/id_ed25519"),
        ("/etc/keys/obs", "/etc/keys/obs"),
        ("~/keys/obs", "~/keys/obs"),
    ],
)
def test_resolve_sshkey_paths(value, expected_template):
    # expand at call time so the ``$HOME`` isolation fixture is honoured
    expected = Path(expected_template).expanduser()
    assert oscrc._resolve_sshkey(value) == (expected, None)


def test_resolve_sshkey_fingerprint():
    assert oscrc._resolve_sshkey("SHA256:abc123") == (None, "SHA256:abc123")


def test_resolve_sshkey_empty_raises():
    with pytest.raises(ObsConfigError, match="empty"):
        oscrc._resolve_sshkey("   ")


def test_tight_permissions_do_not_warn(tmp_path, monkeypatch, caplog):
    key = tmp_path / "k"
    conf = _write(
        tmp_path,
        f"[{API}]\nuser = bob\nsshkey = {key}\n",
        keyfile=key,
        monkeypatch=monkeypatch,
    )
    conf.chmod(0o600)
    with caplog.at_level("WARNING"):
        oscrc.read_credentials(API)
    assert not any("group/world-accessible" in r.message for r in caplog.records)


# --- oscrc discovery (osc identify_conf parity) --------------------------


def _xdg_oscrc(tmp_path: Path) -> Path:
    """Path to the XDG oscrc as redirected by the isolation fixture."""
    return tmp_path / "xdg" / "osc" / "oscrc"


def _home_oscrc(tmp_path: Path) -> Path:
    """Path to ~/.oscrc as redirected by the isolation fixture."""
    return tmp_path / "home" / ".oscrc"


def test_discovery_prefers_osc_config_env(tmp_path, monkeypatch):
    """$OSC_CONFIG wins over both XDG and ~/.oscrc, even when they exist."""
    override = tmp_path / "custom-oscrc"
    override.write_text("")
    xdg = _xdg_oscrc(tmp_path)
    xdg.parent.mkdir(parents=True)
    xdg.write_text("")
    home = _home_oscrc(tmp_path)
    home.parent.mkdir(parents=True)
    home.write_text("")
    monkeypatch.setenv("OSC_CONFIG", str(override))
    assert oscrc._default_conffile() == override


def test_discovery_prefers_xdg_when_it_exists(tmp_path):
    """The XDG oscrc is used when present (with no ~/.oscrc)."""
    xdg = _xdg_oscrc(tmp_path)
    xdg.parent.mkdir(parents=True)
    xdg.write_text("")
    assert oscrc._default_conffile() == xdg


def test_discovery_falls_back_to_home_oscrc(tmp_path):
    """~/.oscrc is used when only it exists (no XDG file)."""
    home = _home_oscrc(tmp_path)
    home.parent.mkdir(parents=True)
    home.write_text("")
    assert oscrc._default_conffile() == home


def test_discovery_default_is_xdg_when_nothing_exists(tmp_path):
    """With neither file present, the XDG path is returned as the default."""
    assert oscrc._default_conffile() == _xdg_oscrc(tmp_path)


def test_discovery_warns_when_both_locations_exist(tmp_path, caplog):
    """Both XDG and ~/.oscrc present: XDG wins and a warning is logged."""
    xdg = _xdg_oscrc(tmp_path)
    xdg.parent.mkdir(parents=True)
    xdg.write_text("")
    home = _home_oscrc(tmp_path)
    home.parent.mkdir(parents=True)
    home.write_text("")
    with caplog.at_level("WARNING"):
        result = oscrc._default_conffile()
    assert result == xdg
    assert any("multiple oscrc files detected" in r.message for r in caplog.records)
