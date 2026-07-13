"""OpenSSH SSHSIG wire-format signer for OBS "Signature" auth.

Reproduces ``ssh-keygen -Y sign`` for any key type (Ed25519, ECDSA, RSA)
using paramiko, so mtui authenticates to the OBS API in-process — no ``osc``
library and no signing subprocess. The format is OpenSSH's SSHSIG
(``PROTOCOL.sshsig``): the message is hashed, wrapped with the namespace,
signed, and the signature plus public key are packed into the outer SSHSIG
blob, returned base64-encoded WITHOUT the PEM armor (that raw base64 is what
the OBS ``Authorization: Signature`` header carries).

RSA keys are signed with ``rsa-sha2-512`` rather than paramiko's default
legacy ``ssh-rsa`` (SHA-1), which modern OpenSSH/OBS reject. Ed25519 and
ECDSA have a single signature algorithm, so no override is needed. The
message-hash algorithm in the SSHSIG blob (``sha512``) is independent of the
signature algorithm and stays fixed, matching ``ssh-keygen -Y sign``.

paramiko ships no ``py.typed``, so ``ty`` cannot check the calls below; the
committed golden vector (Ed25519, byte-for-byte vs ssh-keygen) plus the
RSA/ECDSA verify-round-trip tests are the real guard.
"""

from __future__ import annotations

import base64
import hashlib
from struct import pack
from typing import Protocol

_MAGIC = b"SSHSIG"
_SIG_VERSION = 1
_HASH_ALG = b"sha512"

# paramiko's default RSA signature algorithm is legacy ``ssh-rsa`` (SHA-1),
# which modern OpenSSH and OBS reject; sign RSA keys with SHA-512 instead.
_RSA_KEY_NAME = "ssh-rsa"
_RSA_SIG_ALGORITHM = "rsa-sha2-512"


class _SshKey(Protocol):
    """The narrow paramiko key surface used here (keeps the boundary typed)."""

    def asbytes(self) -> bytes: ...

    def get_name(self) -> str: ...

    def sign_ssh_data(self, data: bytes, algorithm: str | None = ...) -> object: ...


def _wstr(data: bytes) -> bytes:
    """Encode ``data`` as an SSH string: 4-byte big-endian length + bytes."""
    return pack(">I", len(data)) + data


def _sig_algorithm(key: _SshKey) -> str | None:
    """The signature algorithm to request; only RSA needs a non-default one."""
    return _RSA_SIG_ALGORITHM if key.get_name() == _RSA_KEY_NAME else None


def _sig_bytes(signed: object) -> bytes:
    """Normalise ``sign_ssh_data``'s return to the raw SSH signature blob.

    A file-backed paramiko key returns a ``Message`` (unwrapped with
    ``.asbytes()``), but an ``AgentKey`` returns the signature ``bytes``
    directly — both are the identical ``string(algo) + string(sig)`` wire
    blob. Handling only ``.asbytes()`` would crash every ssh-agent key.
    """
    if isinstance(signed, bytes):
        return signed
    return signed.asbytes()  # ty: ignore[unresolved-attribute]


def sign_created(key: _SshKey, namespace: str, created: int) -> str:
    """Build the base64 SSHSIG over the OBS ``(created): <epoch>`` payload.

    Args:
        key: A loaded private key (paramiko ``Ed25519Key``/``ECDSAKey``/
            ``RSAKey`` or an ``AgentKey``).
        namespace: The SSHSIG namespace — the challenge ``realm`` (the live
            api.suse.de value is ``Use your developer account``).
        created: The signed Unix timestamp, also sent as the Authorization
            ``created`` field.

    Returns:
        The base64-encoded SSHSIG blob (no PEM armor) for the Authorization
        ``signature`` field.

    """
    namespace_bytes = namespace.encode("utf-8")
    sigdata = f"(created): {created}".encode()
    hashed = hashlib.sha512(sigdata).digest()

    # The signed blob (PROTOCOL.sshsig): MAGIC | namespace | reserved |
    # hash_alg | H(message).
    to_sign = (
        _MAGIC + _wstr(namespace_bytes) + _wstr(b"") + _wstr(_HASH_ALG) + _wstr(hashed)
    )
    signature = _sig_bytes(key.sign_ssh_data(to_sign, _sig_algorithm(key)))

    # The outer SSHSIG blob: MAGIC | version | publickey | namespace |
    # reserved | hash_alg | signature.
    outer = (
        _MAGIC
        + pack(">I", _SIG_VERSION)
        + _wstr(key.asbytes())
        + _wstr(namespace_bytes)
        + _wstr(b"")
        + _wstr(_HASH_ALG)
        + _wstr(signature)
    )
    return base64.b64encode(outer).decode("ascii")
