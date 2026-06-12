"""HTTP helpers for the openQA / QAM Dashboard search."""

from functools import lru_cache
from logging import getLogger
from typing import Any

import requests

from ...support.http import HTTP_TIMEOUT, build_session

logger = getLogger("mtui.connector.oqa_search")


@lru_cache(maxsize=1)
def _session() -> requests.Session:
    """Lazy shared session with TLS verification disabled.

    Internal SUSE hosts (openqa.suse.de, dashboard.qam.suse.de,
    qam.suse.de) frequently present self-signed or internal-CA certs
    that the user's system trust store does not validate. Disable
    verification on the session so the command works out of the box;
    :func:`mtui.support.http.build_session` silences the resulting
    InsecureRequestWarning.
    """
    return build_session(verify=False)


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
