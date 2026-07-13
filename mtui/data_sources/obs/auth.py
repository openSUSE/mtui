"""OBS "Signature" (SSH) authentication as a ``requests`` auth handler.

Reproduces the OBS Signature challenge/response in-process (no ``osc``, no
subprocess): the first request goes out unauthenticated (the session may
already hold a cookie); on a 401 the ``WWW-Authenticate: Signature`` realm
is read, an SSHSIG over ``(created): <epoch>`` is built with paramiko, and
the request is resent exactly once with the ``Authorization: Signature``
header. Day-one scope is an Ed25519 key from an explicit file; encrypted,
agent, and non-Ed25519 keys fail closed with a typed :class:`ObsConfigError`.
The Authorization header and its signature are never logged.
"""

from __future__ import annotations

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


class ObsSignatureAuth(requests.auth.AuthBase):
    """A ``requests`` auth handler for OBS SSH-signature authentication."""

    def __init__(self, user: str, sshkey_path: Path) -> None:
        """Initialise with the acting user and its Ed25519 private-key path."""
        self.user = user
        self.sshkey_path = sshkey_path
        self._retried = False

    def _load_key(self) -> paramiko.Ed25519Key:
        """Load the Ed25519 key, failing closed (never prompting)."""
        try:
            return paramiko.Ed25519Key.from_private_key_file(str(self.sshkey_path))
        except paramiko.PasswordRequiredException as e:
            raise ObsConfigError(
                f"ssh key {self.sshkey_path} is passphrase-protected; the native "
                "OBS backend needs an unencrypted key or an ssh-agent (agent "
                "support is not yet available)"
            ) from e
        except paramiko.SSHException as e:
            raise ObsConfigError(
                f"ssh key {self.sshkey_path} is not a usable Ed25519 key "
                f"({e}); only Ed25519 keys are supported by the native OBS "
                "backend today"
            ) from e
        except OSError as e:
            raise ObsConfigError(
                f"ssh key {self.sshkey_path} could not be read: {e}"
            ) from e

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
