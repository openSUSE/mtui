"""HTTP client for the OBS/IBS API (native ``requests``, no osc).

Mirrors :mod:`mtui.data_sources.gitea`'s request wrapper: one
:class:`requests.Session` built via :mod:`mtui.support.http`, with
:class:`~mtui.data_sources.obs.auth.ObsSignatureAuth` driving SSH-signature
auth and the IBS session cookie reused in-memory across the handful of calls
one operation makes. Every hop is bounded by ``HTTP_TIMEOUT``; a coarse
wall-clock budget (``[obs] request_timeout``) is checked between calls (there
is no safe in-process mid-call hard kill across mtui's worker threads).
"""

from __future__ import annotations

import time
from logging import getLogger
from typing import TYPE_CHECKING
from urllib.parse import urlparse
from xml.etree import ElementTree as ET

import requests

from ...support.http import (
    HTTP_TIMEOUT,
    build_session,
    is_ssl_verification_error,
    resolve_verify,
    ssl_verification_hint,
)
from .auth import ObsSignatureAuth
from .errors import ObsApiError, ObsTimeoutError

if TYPE_CHECKING:
    from ...support.config import Config
    from .oscrc import ObsCredentials

logger = getLogger("mtui.data_sources.obs.client")


def _error_summary(text: str) -> str:
    """Extract ``<status><summary>`` from an OBS error body (best effort)."""
    if "<!DOCTYPE" in text or "<!ENTITY" in text:
        # OBS never sends a DTD; skip parsing so an entity-expansion body
        # cannot turn error handling into a DoS.
        return ""
    try:
        root = ET.fromstring(text)
    except ET.ParseError:
        return ""
    summary = root.findtext("summary")
    return summary.strip() if summary else ""


class ObsClient:
    """A thin OBS API client over one authenticated ``requests.Session``."""

    def __init__(self, config: Config, credentials: ObsCredentials) -> None:
        """Build the session and attach SSH-signature auth.

        Args:
            config: mtui config (supplies ``obs_api_url``, ``ssl_verify`` and
                the coarse ``obs_request_timeout`` budget).
            credentials: Resolved oscrc credentials (user + key path).

        """
        self._api_url = config.obs_api_url.rstrip("/")
        verify = resolve_verify(True, config.ssl_verify)
        self._session = build_session(verify)
        self._auth = ObsSignatureAuth(
            credentials.user,
            sshkey_path=credentials.sshkey_path,
            sshkey_fingerprint=credentials.sshkey_fingerprint,
        )
        # Coarse between-calls budget: a whole operation makes a few calls;
        # the deadline is checked before each one.
        self._deadline = time.monotonic() + float(config.obs_request_timeout)

    def _url(self, path: str) -> str:
        return f"{self._api_url}/{path.lstrip('/')}"

    def _check_budget(self, url: str) -> None:
        if time.monotonic() > self._deadline:
            raise ObsTimeoutError(
                f"OBS operation exceeded its between-calls time budget before {url}"
            )

    def _request(
        self,
        method: str,
        path: str,
        params: dict[str, str | int] | None = None,
        body: str | None = None,
    ) -> requests.Response:
        url = self._url(path)
        self._check_budget(url)
        headers = {"Accept": "application/xml"}
        data: bytes | None = None
        if body is not None:
            headers["Content-Type"] = "application/xml; charset=utf-8"
            data = body.encode("utf-8")
        try:
            # NB: never log the Authorization header or the request body.
            logger.debug("OBS %s %s", method, url)
            response = self._session.request(
                method,
                url,
                params=params,
                data=data,
                headers=headers,
                auth=self._auth,
                timeout=HTTP_TIMEOUT,
            )
        except requests.exceptions.RequestException as e:
            if is_ssl_verification_error(e):
                logger.error(ssl_verification_hint(urlparse(url).hostname))
                logger.debug("OBS TLS error detail: %s", e)
            else:
                logger.error("OBS %s %s failed: %s", method, url, e)
            raise

        if not response.ok:
            summary = _error_summary(response.text)
            logger.warning(
                "OBS %s %s -> %s%s",
                method,
                url,
                response.status_code,
                f": {summary}" if summary else "",
            )
            raise ObsApiError(response.status_code, url, summary)
        return response

    def get(
        self, path: str, params: dict[str, str | int] | None = None
    ) -> requests.Response:
        """GET ``path`` (relative to the API base) and return the response."""
        return self._request("GET", path, params=params)

    def post(
        self,
        path: str,
        params: dict[str, str | int] | None = None,
        body: str = "",
    ) -> requests.Response:
        """POST ``body`` to ``path`` with ``params`` and return the response."""
        return self._request("POST", path, params=params, body=body)
