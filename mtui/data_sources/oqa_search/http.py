"""HTTP helpers for the openQA / QAM Dashboard search."""

from functools import lru_cache
from logging import getLogger
from typing import Any

import requests
import urllib3

logger = getLogger("mtui.connector.oqa_search")

# (connect, read) timeout for every HTTP call made by this module.
_HTTP_TIMEOUT: tuple[float, float] = (5.0, 30.0)

# Most of the hosts we talk to (openqa.suse.de, dashboard.qam.suse.de,
# qam.suse.de, internal mirrors) present self-signed or internal-CA
# certificates that the system trust store does not know about. Mirror
# the upstream oqa-search behaviour (which works because users typically
# run it on a SUSE machine with the SUSE CA installed) by disabling
# verification here, and silence the resulting per-request warning to
# keep the REPL output readable.
urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)


@lru_cache(maxsize=1)
def _session() -> requests.Session:
    """Lazy shared session with TLS verification disabled.

    Internal SUSE hosts (openqa.suse.de, dashboard.qam.suse.de,
    qam.suse.de) frequently present self-signed or internal-CA certs
    that the user's system trust store does not validate. Disable
    verification on the session so the command works out of the box;
    the InsecureRequestWarning is silenced module-wide above.
    """
    session = requests.Session()
    session.verify = False
    return session


class _HTTPError(RuntimeError):
    """Raised when an HTTP call fails after retries."""


def _get_json(url: str) -> Any:
    """Fetch JSON from a URL with a bounded timeout.

    Raises :class:`_HTTPError` on any transport or HTTP-status failure
    so callers can convert into a user-friendly message.
    """
    try:
        response = _session().get(url, timeout=_HTTP_TIMEOUT)
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
        response = _session().get(url, timeout=_HTTP_TIMEOUT)
        response.raise_for_status()
        return response.text
    except requests.exceptions.RequestException as e:
        logger.debug("HTTP GET %s failed: %s", url, e)
        raise _HTTPError(str(e)) from e
