"""OBS "Signature" (SSH) authentication as a ``requests`` auth handler.

Reproduces the OBS Signature challenge/response in-process (no ``osc``, no
subprocess): the first request goes out unauthenticated (the session may
already hold a cookie); on a 401 the ``WWW-Authenticate: Signature`` realm
is read, an SSHSIG over ``(created): <epoch>`` is built with paramiko, and
the request is resent exactly once with the ``Authorization: Signature``
header. The Authorization header and its signature are never logged.

Any OpenSSH key type works — Ed25519, ECDSA, and RSA private-key files, plus
keys held by a running ssh-agent (selected by ``SHA256:…`` fingerprint or as
the passphrase-protected counterpart of a file on disk). Under headless
``mtui-mcp`` we must never block on a passphrase prompt, so an encrypted key
is only usable via the agent; every unresolvable case fails closed with a
typed :class:`ObsConfigError`.
"""

from __future__ import annotations

import base64
import time
from logging import getLogger
from pathlib import Path
from typing import Any, override
from urllib.request import parse_http_list, parse_keqv_list

import paramiko
import requests
import requests.auth

from ...support.exceptions import ObsConfigError
from . import sshsig

logger = getLogger("mtui.data_sources.obs.auth")

_SIGNATURE_SCHEME = "signature"


def _challenge_params(response: requests.Response) -> dict[str, dict[str, str]]:
    """Parse ``WWW-Authenticate`` into ``{scheme: {param: value}}``.

    Reads the RAW multi-value header list from urllib3 rather than
    ``response.headers['WWW-Authenticate']`` — ``requests`` comma-merges
    duplicate headers into a single unparseable string, which would hide a
    second challenge (e.g. ``Basic`` alongside ``Signature``).
    """
    raw = getattr(response.raw, "headers", None)
    if raw is not None and hasattr(raw, "getlist"):
        lines = raw.getlist("WWW-Authenticate")
    else:  # pragma: no cover - requests always exposes urllib3 raw headers
        merged = response.headers.get("WWW-Authenticate", "")
        lines = [merged] if merged else []

    schemes: dict[str, dict[str, str]] = {}
    for line in lines:
        line = line.strip()
        if not line:
            continue
        scheme, _, rest = line.partition(" ")
        params: dict[str, str] = {}
        if rest.strip():
            try:
                params = parse_keqv_list(parse_http_list(rest))
            except ValueError:
                params = {}
        schemes[scheme.lower()] = params
    return schemes


def _pubkey_blob(path: Path) -> bytes | None:
    """Return the public-key blob from ``<path>.pub``, or ``None`` if absent.

    Used to identify a passphrase-protected private key's counterpart among
    the ssh-agent's loaded keys by comparing raw public-key blobs.
    """
    try:
        text = Path(f"{path}.pub").read_text(encoding="utf-8")
    except OSError:
        return None
    parts = text.split()
    if len(parts) < 2:
        return None
    try:
        return base64.b64decode(parts[1])
    except ValueError:  # includes binascii.Error
        return None


class ObsSignatureAuth(requests.auth.AuthBase):
    """A ``requests`` auth handler for OBS SSH-signature authentication."""

    def __init__(
        self,
        user: str,
        sshkey_path: Path | None = None,
        sshkey_fingerprint: str | None = None,
    ) -> None:
        """Initialise with the acting user and its key locator.

        Exactly one of ``sshkey_path`` (a private-key file) or
        ``sshkey_fingerprint`` (an ssh-agent key's ``SHA256:…`` fingerprint)
        identifies the signing key.
        """
        self.user = user
        self.sshkey_path = sshkey_path
        self.sshkey_fingerprint = sshkey_fingerprint
        self._retried = False

    @staticmethod
    def _agent_keys() -> list[Any]:
        """The keys currently held by the ssh-agent (fails closed on error)."""
        try:
            return list(paramiko.Agent().get_keys())
        except paramiko.SSHException as e:
            raise ObsConfigError(f"could not query the ssh-agent: {e}") from e

    def _agent_key_by_fingerprint(self, fingerprint: str) -> Any:
        """Select the ssh-agent key matching ``fingerprint`` (``SHA256:…``)."""
        want = fingerprint.strip()
        for key in self._agent_keys():
            key_fp = key.fingerprint
            if key_fp == want or key_fp.split(":", 1)[-1] == want:
                return key
        raise ObsConfigError(
            f"ssh-agent has no key matching fingerprint {want!r}; load it with "
            "'ssh-add' (the native OBS backend never prompts for a passphrase)"
        )

    def _agent_key_for_file(self, path: Path) -> Any:
        """Find the ssh-agent key that is ``path``'s decrypted counterpart."""
        blob = _pubkey_blob(path)
        if blob is not None:
            for key in self._agent_keys():
                if key.asbytes() == blob:
                    return key
        hint = "" if blob is not None else f" (no {path}.pub found to identify it)"
        raise ObsConfigError(
            f"ssh key {path} is passphrase-protected and no matching key is "
            f"loaded in the ssh-agent{hint}; run 'ssh-add {path}' first — the "
            "native OBS backend never prompts for a passphrase"
        )

    def _load_key_file(self, path: Path) -> Any:
        """Load any private-key type from ``path``, failing closed.

        ``PKey.from_path`` auto-detects Ed25519/ECDSA/RSA. A missing
        passphrase surfaces as ``PasswordRequiredException`` or (from the
        cryptography backend) a bare ``TypeError``; either way we must never
        prompt under headless mtui-mcp, so we fall back to an ssh-agent that
        holds the decrypted key. A missing file may still be an agent key
        identified by its ``.pub`` on disk.
        """
        try:
            return paramiko.PKey.from_path(str(path))
        except (paramiko.PasswordRequiredException, TypeError):
            return self._agent_key_for_file(path)
        except FileNotFoundError:
            return self._agent_key_for_file(path)
        except (paramiko.SSHException, ValueError) as e:
            raise ObsConfigError(
                f"ssh key {path} is not a usable private key ({e})"
            ) from e
        except OSError as e:
            raise ObsConfigError(f"ssh key {path} could not be read: {e}") from e

    def _load_key(self) -> Any:
        """Resolve the configured key locator to a loaded signing key."""
        if self.sshkey_fingerprint is not None:
            return self._agent_key_by_fingerprint(self.sshkey_fingerprint)
        if self.sshkey_path is None:  # pragma: no cover - oscrc sets one
            raise ObsConfigError("no ssh key configured for OBS signature auth")
        return self._load_key_file(self.sshkey_path)

    def _authorization(self, realm: str) -> str:
        """Build the ``Authorization: Signature …`` header value for ``realm``."""
        created = int(time.time())
        blob = sshsig.sign_created(self._load_key(), realm, created)
        return (
            f'Signature keyId="{self.user}",algorithm="ssh",'
            f'headers="(created)",created={created},signature="{blob}"'
        )

    def handle_401(
        self, response: requests.Response, **kwargs: Any
    ) -> requests.Response:
        """On a 401 Signature challenge, sign and resend the request once."""
        if response.status_code != 401 or self._retried:
            return response

        schemes = _challenge_params(response)
        if _SIGNATURE_SCHEME not in schemes:
            logger.error(
                "OBS returned 401 but did not offer Signature auth (offered: %s)",
                ", ".join(sorted(schemes)) or "nothing",
            )
            return response

        self._retried = True
        realm = schemes[_SIGNATURE_SCHEME].get("realm", "")

        # Consume and release the challenge response before resending.
        _ = response.content
        response.close()

        prepared = response.request.copy()
        prepared.headers["Authorization"] = self._authorization(realm)

        retried = response.connection.send(prepared, **kwargs)
        retried.history.append(response)
        retried.request = prepared
        return retried

    @override
    def __call__(self, r: requests.PreparedRequest) -> requests.PreparedRequest:
        """Attach the 401 handler; the first request carries no Authorization."""
        self._retried = False
        r.register_hook("response", self.handle_401)
        return r
