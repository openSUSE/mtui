"""Golden-vector test for the SSHSIG signer (mtui.data_sources.obs.sshsig).

The signer must reproduce ``ssh-keygen -Y sign`` byte-for-byte, since the
base64 SSHSIG blob goes straight into the OBS ``Authorization: Signature``
header. The key is derived from a FIXED 32-byte seed (a test fixture, not a
credential) so the vector is deterministic (Ed25519 signing is
deterministic), and ``GOLDEN`` below was cross-checked once against
``ssh-keygen -Y sign`` for exactly this seed/namespace/created.
"""

import base64
import hashlib
from struct import pack

import paramiko
import pytest
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from paramiko.message import Message

from mtui.data_sources.obs import sshsig

SEED = base64.b64decode("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=")
NAMESPACE = "Use your developer account"
CREATED = 1700000000


def _read_str(raw: bytes, off: int) -> tuple[bytes, int]:
    n = int.from_bytes(raw[off : off + 4], "big")
    off += 4
    return raw[off : off + n], off + n


def _decode_sshsig(blob_b64: str) -> dict[str, bytes]:
    """Unpack the outer SSHSIG blob into its fields (for verification)."""
    raw = base64.b64decode(blob_b64)
    assert raw[:6] == b"SSHSIG"
    off = 10  # 6 magic + 4 version
    pub, off = _read_str(raw, off)
    namespace, off = _read_str(raw, off)
    _reserved, off = _read_str(raw, off)
    hashalg, off = _read_str(raw, off)
    signature, off = _read_str(raw, off)
    return {
        "pub": pub,
        "namespace": namespace,
        "hashalg": hashalg,
        "signature": signature,
    }


def _to_sign(namespace: str, created: int) -> bytes:
    hashed = hashlib.sha512(f"(created): {created}".encode()).digest()

    def w(b: bytes) -> bytes:
        return pack(">I", len(b)) + b

    return b"SSHSIG" + w(namespace.encode()) + w(b"") + w(b"sha512") + w(hashed)


def _assert_verifies(key, blob_b64: str) -> dict[str, bytes]:
    """The SSHSIG blob carries the key's pubkey and a valid signature."""
    fields = _decode_sshsig(blob_b64)
    assert fields["pub"] == key.asbytes()
    assert fields["namespace"] == NAMESPACE.encode()
    assert fields["hashalg"] == b"sha512"
    assert key.verify_ssh_sig(
        _to_sign(NAMESPACE, CREATED), Message(fields["signature"])
    )
    return fields


GOLDEN = (
    "U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgA6EHv/POEL4dcN0Y50vAmWfk1jCb"
    "pQ1fHdyGZBJVMbgAAAAaVXNlIHlvdXIgZGV2ZWxvcGVyIGFjY291bnQAAAAAAAAABnNoYTUx"
    "MgAAAFMAAAALc3NoLWVkMjU1MTkAAABAiD/Mxc0SAyrM6wVmT1T9BF7dNbv0dUKD4i3Tmxwq"
    "iXyrstfeqgcwEx6QkzfDwCjMNPD97uiIcGJjfR2yLhtBCw=="
)


@pytest.fixture
def key(tmp_path):
    """A paramiko Ed25519 key derived from the fixed test seed."""
    priv = Ed25519PrivateKey.from_private_bytes(SEED)
    pem = priv.private_bytes(
        serialization.Encoding.PEM,
        serialization.PrivateFormat.OpenSSH,
        serialization.NoEncryption(),
    )
    path = tmp_path / "id_ed25519"
    path.write_bytes(pem)
    return paramiko.Ed25519Key.from_private_key_file(str(path))


def test_sign_created_matches_golden_vector(key):
    """The base64 SSHSIG equals the ssh-keygen-verified golden vector."""
    assert sshsig.sign_created(key, NAMESPACE, CREATED) == GOLDEN


def test_sign_created_is_deterministic(key):
    """Ed25519 signing is deterministic — same inputs, same blob."""
    a = sshsig.sign_created(key, NAMESPACE, CREATED)
    b = sshsig.sign_created(key, NAMESPACE, CREATED)
    assert a == b


def test_created_changes_signature(key):
    """A different created timestamp yields a different signature."""
    assert sshsig.sign_created(key, NAMESPACE, CREATED) != sshsig.sign_created(
        key, NAMESPACE, CREATED + 1
    )


def test_namespace_changes_signature(key):
    """A different namespace yields a different signature."""
    assert sshsig.sign_created(key, NAMESPACE, CREATED) != sshsig.sign_created(
        key, "other namespace", CREATED
    )


def test_blob_is_valid_sshsig_base64(key):
    """The output is base64 that decodes to a SSHSIG-magic blob."""
    raw = base64.b64decode(sshsig.sign_created(key, NAMESPACE, CREATED))
    assert raw.startswith(b"SSHSIG")


def test_wstr_encodes_ssh_string():
    assert sshsig._wstr(b"") == b"\x00\x00\x00\x00"
    assert sshsig._wstr(b"abc") == b"\x00\x00\x00\x03abc"


def test_rsa_key_signs_with_rsa_sha2_512():
    """RSA is signed with rsa-sha2-512 (not paramiko's legacy ssh-rsa/SHA-1)."""
    key = paramiko.RSAKey.generate(2048)
    fields = _assert_verifies(key, sshsig.sign_created(key, NAMESPACE, CREATED))
    # The inner signature's algorithm field must be rsa-sha2-512.
    assert Message(fields["signature"]).get_text() == "rsa-sha2-512"


def test_ecdsa_key_signs_and_verifies():
    """An ECDSA key produces a valid (randomised) SSHSIG signature."""
    key = paramiko.ECDSAKey.generate()
    fields = _assert_verifies(key, sshsig.sign_created(key, NAMESPACE, CREATED))
    assert Message(fields["signature"]).get_text() == "ecdsa-sha2-nistp256"


def test_sig_algorithm_only_overrides_rsa():
    """Only RSA keys request an explicit signature algorithm."""
    assert sshsig._sig_algorithm(paramiko.RSAKey.generate(2048)) == "rsa-sha2-512"
    assert sshsig._sig_algorithm(paramiko.ECDSAKey.generate()) is None
