"""Tests for OBS SSH-signature auth (mtui.data_sources.obs.auth)."""

import base64
from types import SimpleNamespace
from typing import Any

import paramiko
import pytest
import requests
import responses
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from urllib3._collections import HTTPHeaderDict

from mtui.data_sources.obs.auth import ObsSignatureAuth, _challenge_params
from mtui.support.exceptions import ObsConfigError

SEED = base64.b64decode("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=")
REALM = "Use your developer account"
URL = "https://api.suse.de/request/1"


@pytest.fixture
def key_path(tmp_path):
    """Write the deterministic Ed25519 test key to disk and return its path."""
    priv = Ed25519PrivateKey.from_private_bytes(SEED)
    pem = priv.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.OpenSSH,
        serialization.NoEncryption(),
    )
    path = tmp_path / "id_ed25519"
    path.write_bytes(pem)
    return path


def _fake_response(*www_authenticate: str) -> Any:
    """A response whose urllib3 raw headers carry multi-value WWW-Authenticate."""
    raw_headers = HTTPHeaderDict()
    for value in www_authenticate:
        raw_headers.add("WWW-Authenticate", value)
    return SimpleNamespace(raw=SimpleNamespace(headers=raw_headers), headers={})


def test_challenge_params_parses_dual_scheme():
    """A dual Signature+Basic challenge is split from the raw header list."""
    schemes = _challenge_params(
        _fake_response(
            'Signature realm="Use your developer account"',
            'Basic realm="Open Build Service"',
        )
    )
    assert set(schemes) == {"signature", "basic"}
    assert schemes["signature"]["realm"] == "Use your developer account"
    assert schemes["basic"]["realm"] == "Open Build Service"


def test_challenge_params_handles_empty_and_paramless_and_malformed():
    """Blank lines are skipped; param-less and malformed schemes parse safely."""
    schemes = _challenge_params(
        _fake_response(
            "",  # blank -> skipped
            "Negotiate",  # scheme with no params
            "Signature realm",  # malformed (no =value) -> ValueError -> {}
        )
    )
    assert "negotiate" in schemes
    assert schemes["negotiate"] == {}
    assert schemes["signature"] == {}


def test_challenge_params_fallback_to_merged_header():
    """When urllib3 raw headers are absent, fall back to the merged header."""
    fake: Any = SimpleNamespace(
        raw=None, headers={"WWW-Authenticate": 'Signature realm="X"'}
    )
    schemes = _challenge_params(fake)
    assert "signature" in schemes


@responses.activate
def test_signs_and_retries_once_on_401(key_path):
    """A 401 Signature challenge triggers exactly one signed retry."""
    responses.add(
        responses.GET,
        URL,
        status=401,
        headers={"WWW-Authenticate": f'Signature realm="{REALM}"'},
    )
    responses.add(responses.GET, URL, status=200, body="<ok/>")

    auth = ObsSignatureAuth("qamuser", key_path)
    resp = requests.get(URL, auth=auth)

    assert resp.status_code == 200
    assert len(responses.calls) == 2
    # First request unauthenticated, second carries the Signature header.
    assert "Authorization" not in responses.calls[0].request.headers
    authz = str(responses.calls[1].request.headers["Authorization"])
    assert authz.startswith('Signature keyId="qamuser",algorithm="ssh"')
    assert 'headers="(created)"' in authz
    assert "created=" in authz
    assert 'signature="' in authz


@responses.activate
def test_no_signature_scheme_does_not_retry(key_path, caplog):
    """A 401 that does not offer Signature is returned as-is (no retry)."""
    responses.add(
        responses.GET,
        URL,
        status=401,
        headers={"WWW-Authenticate": 'Basic realm="x"'},
    )
    auth = ObsSignatureAuth("qamuser", key_path)
    with caplog.at_level("ERROR"):
        resp = requests.get(URL, auth=auth)
    assert resp.status_code == 401
    assert len(responses.calls) == 1
    assert any("did not offer Signature" in r.message for r in caplog.records)


@responses.activate
def test_non_401_passes_through(key_path):
    """A non-401 first response is returned untouched."""
    responses.add(responses.GET, URL, status=200, body="<ok/>")
    resp = requests.get(URL, auth=ObsSignatureAuth("qamuser", key_path))
    assert resp.status_code == 200
    assert len(responses.calls) == 1


def _encrypted_ed25519(path):
    """Write a passphrase-protected Ed25519 key (no usable passphrase)."""
    pem = Ed25519PrivateKey.from_private_bytes(SEED).private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.OpenSSH,
        serialization.BestAvailableEncryption(b"hunter2"),
    )
    path.write_bytes(pem)


def _fake_agent(monkeypatch, *keys):
    """Point paramiko.Agent at a fixed key set (no real agent required)."""
    monkeypatch.setattr(
        paramiko, "Agent", lambda: SimpleNamespace(get_keys=lambda: tuple(keys))
    )


def _real_agent_key(signer):
    """A genuine paramiko ``AgentKey`` backed by an in-process ``signer``.

    Real ssh-agent keys return raw *bytes* from ``sign_ssh_data`` (via the
    agent wire protocol), unlike file keys which return a ``Message``. This
    fixture reproduces that so the agent signing path is exercised for real —
    an in-process ``PKey`` stand-in would mask the bytes/Message mismatch.
    """
    from paramiko.agent import SSH2_AGENT_SIGN_RESPONSE

    flag_to_algo = {0: None, 2: "rsa-sha2-256", 4: "rsa-sha2-512"}

    class _Conn:
        def _send_message(self, msg):
            m = paramiko.Message(msg.asbytes())
            m.get_byte()  # SSH2_AGENTC_SIGN_REQUEST opcode
            m.get_string()  # key blob
            data = m.get_string()  # the data to sign
            algo = flag_to_algo[m.get_int()]
            resp = paramiko.Message()
            resp.add_string(signer.sign_ssh_data(data, algo).asbytes())
            resp.rewind()
            return SSH2_AGENT_SIGN_RESPONSE, resp

    # _Conn duck-types the Agent surface AgentKey.sign_ssh_data actually uses.
    return paramiko.AgentKey(agent=_Conn(), blob=signer.asbytes())  # ty: ignore[invalid-argument-type]


def _assert_authz_verifies(signer, authz):
    """The Authorization header's SSHSIG verifies against ``signer``'s key."""
    import hashlib
    from struct import pack

    from paramiko.message import Message

    b64 = authz.split('signature="')[1].split('"')[0]
    created = int(authz.split("created=")[1].split(",")[0])
    raw = base64.b64decode(b64)
    off = 10  # 6 magic + 4 version

    def rd(o):
        n = int.from_bytes(raw[o : o + 4], "big")
        return raw[o + 4 : o + 4 + n], o + 4 + n

    pub, off = rd(off)
    _ns, off = rd(off)
    _reserved, off = rd(off)
    halg, off = rd(off)
    sig, off = rd(off)
    assert pub == signer.asbytes()
    assert halg == b"sha512"
    hashed = hashlib.sha512(f"(created): {created}".encode()).digest()

    def w(b):
        return pack(">I", len(b)) + b

    to_sign = b"SSHSIG" + w(REALM.encode()) + w(b"") + w(b"sha512") + w(hashed)
    assert signer.verify_ssh_sig(to_sign, Message(sig))


def test_rsa_key_signs(tmp_path):
    """An RSA private-key file now signs successfully (all key types)."""
    path = tmp_path / "id_rsa"
    paramiko.RSAKey.generate(2048).write_private_key_file(str(path))
    authz = ObsSignatureAuth("qamuser", sshkey_path=path)._authorization(REALM)
    assert 'signature="' in authz


def test_ecdsa_key_signs(tmp_path):
    """An ECDSA private-key file signs successfully."""
    path = tmp_path / "id_ecdsa"
    paramiko.ECDSAKey.generate().write_private_key_file(str(path))
    authz = ObsSignatureAuth("qamuser", sshkey_path=path)._authorization(REALM)
    assert 'signature="' in authz


@pytest.mark.parametrize(
    "signer",
    [paramiko.ECDSAKey.generate(), paramiko.RSAKey.generate(2048)],
    ids=["ecdsa", "rsa"],
)
def test_agent_fingerprint_signs(monkeypatch, signer):
    """A key selected by SHA256 fingerprint is signed via a real AgentKey.

    Uses a genuine paramiko AgentKey (bytes-returning sign_ssh_data), so a
    regression to the file-key .asbytes() path would crash here. The RSA case
    also pins the rsa-sha2-512 agent flag round-trip.
    """
    agent_key = _real_agent_key(signer)
    _fake_agent(monkeypatch, agent_key)
    auth = ObsSignatureAuth("qamuser", sshkey_fingerprint=agent_key.fingerprint)
    authz = auth._authorization(REALM)
    assert 'signature="' in authz
    _assert_authz_verifies(signer, authz)


def test_agent_fingerprint_without_prefix_matches(monkeypatch):
    """A fingerprint given without the SHA256: prefix still matches."""
    signer = paramiko.ECDSAKey.generate()
    agent_key = _real_agent_key(signer)
    _fake_agent(monkeypatch, agent_key)
    bare = agent_key.fingerprint.split(":", 1)[-1]
    auth = ObsSignatureAuth("qamuser", sshkey_fingerprint=bare)
    _assert_authz_verifies(signer, auth._authorization(REALM))


def test_agent_fingerprint_not_found_fails_closed(monkeypatch):
    """A fingerprint absent from the agent fails closed (never prompts)."""
    _fake_agent(monkeypatch, paramiko.ECDSAKey.generate())
    auth = ObsSignatureAuth("qamuser", sshkey_fingerprint="SHA256:missing")
    with pytest.raises(ObsConfigError, match="no key matching fingerprint"):
        auth._authorization(REALM)


def test_agent_query_error_fails_closed(monkeypatch):
    """An ssh-agent protocol error fails closed."""

    def boom():
        raise paramiko.SSHException("no agent socket")

    monkeypatch.setattr(paramiko, "Agent", lambda: SimpleNamespace(get_keys=boom))
    auth = ObsSignatureAuth("qamuser", sshkey_fingerprint="SHA256:x")
    with pytest.raises(ObsConfigError, match="ssh-agent"):
        auth._authorization(REALM)


def test_encrypted_key_falls_back_to_agent(tmp_path, monkeypatch):
    """An encrypted key is signed via the agent that holds its decrypted half."""
    signer = paramiko.RSAKey.generate(2048)
    agent_key = _real_agent_key(signer)
    priv = tmp_path / "id_rsa"
    _encrypted_ed25519(priv)
    # The .pub on disk identifies the agent key by public-blob equality.
    (tmp_path / "id_rsa.pub").write_text(f"ssh-rsa {signer.get_base64()} me\n")
    _fake_agent(monkeypatch, agent_key)
    auth = ObsSignatureAuth("qamuser", sshkey_path=priv)
    _assert_authz_verifies(signer, auth._authorization(REALM))


def test_pub_only_file_uses_agent(tmp_path, monkeypatch):
    """A key present only as <name>.pub is signed via the agent's private half."""
    signer = paramiko.RSAKey.generate(2048)
    agent_key = _real_agent_key(signer)
    priv = tmp_path / "id_rsa"  # the private file does not exist on disk
    (tmp_path / "id_rsa.pub").write_text(f"ssh-rsa {signer.get_base64()} me\n")
    _fake_agent(monkeypatch, agent_key)
    auth = ObsSignatureAuth("qamuser", sshkey_path=priv)
    _assert_authz_verifies(signer, auth._authorization(REALM))


def test_missing_file_no_pub_fails_closed(tmp_path):
    """A missing key file with no .pub cannot resolve to an agent key."""
    auth = ObsSignatureAuth("qamuser", sshkey_path=tmp_path / "absent")
    with pytest.raises(ObsConfigError, match="passphrase-protected"):
        auth._authorization(REALM)


def test_encrypted_key_no_pub_fails_closed(tmp_path):
    """An encrypted key with no .pub cannot be matched to an agent key."""
    priv = tmp_path / "enc"
    _encrypted_ed25519(priv)
    auth = ObsSignatureAuth("qamuser", sshkey_path=priv)
    with pytest.raises(ObsConfigError, match="passphrase-protected"):
        auth._authorization(REALM)


def test_encrypted_key_pub_not_in_agent_fails_closed(tmp_path, monkeypatch):
    """An encrypted key whose .pub is not loaded in the agent fails closed."""
    priv = tmp_path / "id_rsa"
    _encrypted_ed25519(priv)
    wanted = paramiko.RSAKey.generate(2048)
    (tmp_path / "id_rsa.pub").write_text(f"ssh-rsa {wanted.get_base64()} me\n")
    _fake_agent(monkeypatch, paramiko.ECDSAKey.generate())  # a different key
    auth = ObsSignatureAuth("qamuser", sshkey_path=priv)
    with pytest.raises(ObsConfigError, match="passphrase-protected"):
        auth._authorization(REALM)


def test_unreadable_key_fails_closed(tmp_path):
    """A key path that cannot be read (here a directory) fails closed."""
    keydir = tmp_path / "keydir"
    keydir.mkdir()
    auth = ObsSignatureAuth("qamuser", sshkey_path=keydir)
    with pytest.raises(ObsConfigError):
        auth._authorization(REALM)


def test_binary_key_fails_closed(tmp_path):
    """A binary/non-PEM key file raises ObsConfigError, not a bare ValueError.

    paramiko's loader raises a plain ValueError on such input; without the
    ValueError catch it would escape the never-raise facade as a traceback.
    """
    path = tmp_path / "id_bin"
    path.write_bytes(bytes(range(256)) * 8)
    auth = ObsSignatureAuth("qamuser", sshkey_path=path)
    with pytest.raises(ObsConfigError, match="not a usable private key"):
        auth._authorization(REALM)


def test_pubkey_blob_missing_and_malformed(tmp_path):
    """_pubkey_blob returns None for absent, short, or non-base64 .pub files."""
    from mtui.data_sources.obs.auth import _pubkey_blob

    assert _pubkey_blob(tmp_path / "absent") is None
    base = tmp_path / "k"
    (tmp_path / "k.pub").write_text("only-one-field\n")
    assert _pubkey_blob(base) is None
    (tmp_path / "k.pub").write_text("ssh-rsa @@@not-base64@@@ comment\n")
    assert _pubkey_blob(base) is None


def test_authorization_header_never_logged(key_path, caplog):
    """Building the header emits no log line containing the signature."""
    auth = ObsSignatureAuth("qamuser", key_path)
    with caplog.at_level("DEBUG"):
        authz = auth._authorization(REALM)
    signature = authz.split('signature="')[1]
    assert all(signature[:40] not in r.message for r in caplog.records)
