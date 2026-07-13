"""Tests for the native oscrc credential reader (mtui.data_sources.obs.oscrc)."""

from dataclasses import fields
from pathlib import Path

import pytest

from mtui.data_sources.obs import oscrc
from mtui.support.exceptions import ObsConfigError

API = "https://api.suse.de"


def _write(tmp_path: Path, body: str, keyfile: Path | None = None) -> Path:
    """Write an oscrc file, creating a dummy key file when referenced."""
    if keyfile is not None:
        keyfile.write_text("dummy-key")
    path = tmp_path / "oscrc"
    path.write_text(body)
    return path


def test_reads_user_and_sshkey(tmp_path):
    key = tmp_path / "id_ed25519"
    conf = _write(
        tmp_path,
        f"[general]\napiurl = {API}\n\n[{API}]\nuser = bob\nsshkey = {key}\n",
        keyfile=key,
    )
    creds = oscrc.read_credentials(API, conffile=str(conf))
    assert creds.user == "bob"
    assert creds.sshkey_path == key
    assert creds.apiurl == API
    assert creds.source == str(conf)


def test_password_is_never_read_for_signature_target(tmp_path):
    """`pass`/`passx` are ignored (no password ever enters memory)."""
    key = tmp_path / "id_ed25519"
    conf = _write(
        tmp_path,
        f"[{API}]\nuser = bob\npass = s3cret\npassx = AAAA==\nsshkey = {key}\n",
        keyfile=key,
    )
    creds = oscrc.read_credentials(API, conffile=str(conf))
    assert creds.user == "bob"
    # The dataclass structurally cannot carry a password.
    assert "pass" not in {f.name for f in fields(creds)}
    assert "s3cret" not in repr(creds)


def test_missing_conffile_raises(tmp_path):
    with pytest.raises(ObsConfigError, match="not found"):
        oscrc.read_credentials(API, conffile=str(tmp_path / "nope"))


def test_missing_section_raises(tmp_path):
    conf = _write(tmp_path, "[https://api.opensuse.org]\nuser = bob\n")
    with pytest.raises(ObsConfigError, match="no \\[https://api.suse.de\\] section"):
        oscrc.read_credentials(API, conffile=str(conf))


def test_missing_user_raises(tmp_path):
    key = tmp_path / "k"
    conf = _write(tmp_path, f"[{API}]\nsshkey = {key}\n", keyfile=key)
    with pytest.raises(ObsConfigError, match="no 'user'"):
        oscrc.read_credentials(API, conffile=str(conf))


def test_missing_sshkey_raises(tmp_path):
    conf = _write(tmp_path, f"[{API}]\nuser = bob\n")
    with pytest.raises(ObsConfigError, match="no 'sshkey'"):
        oscrc.read_credentials(API, conffile=str(conf))


def test_unsupported_credentials_manager_raises(tmp_path):
    key = tmp_path / "k"
    conf = _write(
        tmp_path,
        f"[{API}]\nuser = bob\nsshkey = {key}\n"
        "credentials_mgr_class = osc.credentials.KeyringCredentialsManager\n",
        keyfile=key,
    )
    with pytest.raises(ObsConfigError, match="credentials_mgr_class"):
        oscrc.read_credentials(API, conffile=str(conf))


def test_agent_fingerprint_sshkey_is_accepted(tmp_path):
    """A SHA256: fingerprint yields an agent-key credential (no file)."""
    conf = _write(tmp_path, f"[{API}]\nuser = bob\nsshkey = SHA256:abc123\n")
    creds = oscrc.read_credentials(API, conffile=str(conf))
    assert creds.sshkey_fingerprint == "SHA256:abc123"
    assert creds.sshkey_path is None
    assert creds.user == "bob"


def test_pub_only_key_on_disk_is_accepted(tmp_path):
    """A key present only as <name>.pub is accepted (agent holds the private)."""
    priv = tmp_path / "id_ed25519"
    (tmp_path / "id_ed25519.pub").write_text("ssh-ed25519 AAAA comment\n")
    conf = _write(tmp_path, f"[{API}]\nuser = bob\nsshkey = {priv}\n")
    creds = oscrc.read_credentials(API, conffile=str(conf))
    assert creds.sshkey_path == priv
    assert creds.sshkey_fingerprint is None


def test_missing_key_file_raises(tmp_path):
    conf = _write(tmp_path, f"[{API}]\nuser = bob\nsshkey = {tmp_path / 'absent'}\n")
    with pytest.raises(ObsConfigError, match="does not exist"):
        oscrc.read_credentials(API, conffile=str(conf))


def test_unparsable_oscrc_raises(tmp_path):
    conf = tmp_path / "oscrc"
    conf.write_text("not = ini = at = all\n[unclosed\n")
    with pytest.raises(ObsConfigError, match="could not parse"):
        oscrc.read_credentials(API, conffile=str(conf))


def test_parse_error_does_not_leak_secret(tmp_path):
    """A malformed oscrc's source line (e.g. a password) is not surfaced."""
    conf = tmp_path / "oscrc"
    conf.write_text("pass = SUPERSECRET\n[general]\n")  # value before any section
    with pytest.raises(ObsConfigError) as ei:
        oscrc.read_credentials(API, conffile=str(conf))
    assert "SUPERSECRET" not in str(ei.value)


def test_sshkey_inherited_from_general(tmp_path):
    """A key set only in [general] is inherited (osc FromParent parity)."""
    key = tmp_path / "id_ed25519"
    key.write_text("dummy-key")
    conf = _write(
        tmp_path,
        f"[general]\nsshkey = {key}\n\n[{API}]\nuser = bob\n",
    )
    creds = oscrc.read_credentials(API, conffile=str(conf))
    assert creds.sshkey_path == key
    assert creds.user == "bob"


def test_credentials_manager_inherited_from_general(tmp_path):
    """A global keyring manager in [general] still fails closed."""
    key = tmp_path / "k"
    conf = _write(
        tmp_path,
        f"[general]\ncredentials_mgr_class = osc.credentials.KeyringCredentialsManager\n"
        f"\n[{API}]\nuser = bob\nsshkey = {key}\n",
        keyfile=key,
    )
    with pytest.raises(ObsConfigError, match="credentials_mgr_class"):
        oscrc.read_credentials(API, conffile=str(conf))


def test_trailing_slash_section_header_matches(tmp_path):
    """A [https://api.suse.de/] header matches the api.suse.de apiurl."""
    key = tmp_path / "k"
    conf = _write(tmp_path, f"[{API}/]\nuser = bob\nsshkey = {key}\n", keyfile=key)
    creds = oscrc.read_credentials(API, conffile=str(conf))
    assert creds.user == "bob"


def test_loose_permissions_warn(tmp_path, caplog):
    key = tmp_path / "k"
    conf = _write(tmp_path, f"[{API}]\nuser = bob\nsshkey = {key}\n", keyfile=key)
    conf.chmod(0o644)
    with caplog.at_level("WARNING"):
        oscrc.read_credentials(API, conffile=str(conf))
    assert any("group/world-accessible" in r.message for r in caplog.records)


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        ("id_ed25519", Path("~/.ssh/id_ed25519").expanduser()),
        ("/etc/keys/obs", Path("/etc/keys/obs")),
        ("~/keys/obs", Path("~/keys/obs").expanduser()),
    ],
)
def test_resolve_sshkey_paths(value, expected):
    assert oscrc._resolve_sshkey(value) == (expected, None)


def test_resolve_sshkey_fingerprint():
    assert oscrc._resolve_sshkey("SHA256:abc123") == (None, "SHA256:abc123")


def test_resolve_sshkey_empty_raises():
    with pytest.raises(ObsConfigError, match="empty"):
        oscrc._resolve_sshkey("   ")


def test_default_conffile_is_oscrc():
    assert oscrc._default_conffile() == Path("~/.oscrc").expanduser()


def test_tight_permissions_do_not_warn(tmp_path, caplog):
    key = tmp_path / "k"
    conf = _write(tmp_path, f"[{API}]\nuser = bob\nsshkey = {key}\n", keyfile=key)
    conf.chmod(0o600)
    with caplog.at_level("WARNING"):
        oscrc.read_credentials(API, conffile=str(conf))
    assert not any("group/world-accessible" in r.message for r in caplog.records)


def test_default_conffile_used_when_conffile_empty(tmp_path, monkeypatch):
    """An empty conffile falls back to ~/.oscrc (here redirected via HOME)."""
    key = tmp_path / "k"
    key.write_text("dummy-key")
    (tmp_path / ".oscrc").write_text(f"[{API}]\nuser = bob\nsshkey = {key}\n")
    monkeypatch.setattr(oscrc, "_default_conffile", lambda: tmp_path / ".oscrc")
    creds = oscrc.read_credentials(API, conffile="")
    assert creds.user == "bob"
