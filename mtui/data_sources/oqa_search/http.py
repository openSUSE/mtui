"""HTTP helpers for the openQA / QAM Dashboard search."""

from functools import lru_cache
from logging import getLogger
from typing import Any

import requests

from ...support.http import HTTP_TIMEOUT, VerifyPolicy, build_session

logger = getLogger("mtui.connector.oqa_search")

# Effective TLS verification policy for this module's shared session.
# Defaults to verifying; the command layer overrides it from the global
# ``[mtui] ssl_verify`` config via :func:`set_verify` before searching.
_verify: VerifyPolicy = True


def set_verify(verify: VerifyPolicy) -> None:
    """Set the TLS verification policy for the shared search session.

    Resets the cached session when the policy actually changes so the
    next request rebuilds it with the new ``verify`` value. The command
    layer calls this with the resolved ``[mtui] ssl_verify`` value so
    the openQA / Dashboard search honors the global policy.
    """
    global _verify
    if verify != _verify:
        _verify = verify
        _session.cache_clear()


@lru_cache(maxsize=1)
def _session() -> requests.Session:
    """Lazy shared session honoring the configured TLS verify policy.

    Internal SUSE hosts (openqa.suse.de, dashboard.qam.suse.de,
    qam.suse.de) present internal-CA certs that require the SUSE CA in
    the system trust store. Verification defaults to on; a user can
    disable it (or point at a CA bundle) globally via ``[mtui]
    ssl_verify`` -- :func:`mtui.support.http.build_session` silences the
    InsecureRequestWarning when verification is off.
    """
    return build_session(_verify)


class _HTTPError(RuntimeError):
    """Raised when an HTTP call fails after retries."""


def _get_json(url: str) -> Any:
    """Fetch JSON from a URL with a bounded timeout.

    Raises :class:`_HTTPError` on any transport or HTTP-status failure
    so callers can convert into a user-friendly message.
    """
    try:
        response = _session().get(url, timeout=HTTP_TIMEOUT)
        response.raise_for_status()
        return response.json()
    except requests.exceptions.RequestException as e:
        logger.debug("HTTP GET %s failed: %s", url, e)
        raise _HTTPError(str(e)) from e
    except ValueError as e:
        logger.debug("Invalid JSON from %s: %s", url, e)
        raise _HTTPError(str(e)) from e


def _fetch_url_content(url: str) -> str:
    """Fetch text from a URL with a bounded timeout."""
    try:
        response = _session().get(url, timeout=HTTP_TIMEOUT)
        response.raise_for_status()
        return response.text
    except requests.exceptions.RequestException as e:
        logger.debug("HTTP GET %s failed: %s", url, e)
        raise _HTTPError(str(e)) from e
