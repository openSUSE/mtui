"""OpenSSH SSHSIG wire-format signer for OBS "Signature" auth.

Reproduces ``ssh-keygen -Y sign`` byte-for-byte for an Ed25519 key using
paramiko, so mtui authenticates to the OBS API in-process — no ``osc``
library and no signing subprocess. The format is OpenSSH's SSHSIG
(``PROTOCOL.sshsig``): the message is hashed, wrapped with the namespace,
signed, and the signature plus public key are packed into the outer SSHSIG
blob, returned base64-encoded WITHOUT the PEM armor (that raw base64 is what
the OBS ``Authorization: Signature`` header carries).

paramiko ships no ``py.typed``, so ``ty`` cannot check the calls below; the
committed golden-vector test (byte-for-byte vs ssh-keygen) is the real guard.
"""

from __future__ import annotations

import base64
import hashlib
from struct import pack
from typing import Protocol

_MAGIC = b"SSHSIG"
_SIG_VERSION = 1
_HASH_ALG = b"sha512"


class _SshKey(Protocol):
    """The narrow paramiko key surface used here (keeps the boundary typed)."""

    def asbytes(self) -> bytes: ...

    def sign_ssh_data(self, data: bytes) -> object: ...


def _wstr(data: bytes) -> bytes:
    """Encode ``data`` as an SSH string: 4-byte big-endian length + bytes."""
    return pack(">I", len(data)) + data


def sign_created(key: _SshKey, namespace: str, created: int) -> str:
    """Build the base64 SSHSIG over the OBS ``(created): <epoch>`` payload.

    Args:
        key: A loaded Ed25519 private key (paramiko ``Ed25519Key``).
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
    signature: bytes = key.sign_ssh_data(to_sign).asbytes()  # ty: ignore[unresolved-attribute]

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
