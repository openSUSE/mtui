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


def test_passphrase_key_fails_closed(tmp_path):
    """An encrypted key raises ObsConfigError (never prompts)."""
    priv = Ed25519PrivateKey.from_private_bytes(SEED)
    pem = priv.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.OpenSSH,
        serialization.BestAvailableEncryption(b"hunter2"),
    )
    path = tmp_path / "enc"
    path.write_bytes(pem)
    auth = ObsSignatureAuth("qamuser", path)
    with pytest.raises(ObsConfigError, match="passphrase-protected"):
        auth._authorization(REALM)


def test_non_ed25519_key_fails_closed(tmp_path):
    """A non-Ed25519 key raises ObsConfigError (only Ed25519 supported)."""
    path = tmp_path / "rsa"
    paramiko.RSAKey.generate(2048).write_private_key_file(str(path))
    auth = ObsSignatureAuth("qamuser", path)
    with pytest.raises(ObsConfigError, match="Ed25519"):
        auth._authorization(REALM)


def test_unreadable_key_fails_closed(tmp_path):
    """A key path that cannot be read (here a directory) fails closed."""
    keydir = tmp_path / "keydir"
    keydir.mkdir()
    auth = ObsSignatureAuth("qamuser", keydir)
    with pytest.raises(ObsConfigError):
        auth._authorization(REALM)


def test_authorization_header_never_logged(key_path, caplog):
    """Building the header emits no log line containing the signature."""
    auth = ObsSignatureAuth("qamuser", key_path)
    with caplog.at_level("DEBUG"):
        authz = auth._authorization(REALM)
    signature = authz.split('signature="')[1]
    assert all(signature[:40] not in r.message for r in caplog.records)
