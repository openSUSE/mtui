"""qam.suse.de testreport preconditions for the native QAM ops.

A plain HTTPS GET of the machine-readable testreport log (no OBS auth); the
same guards the plugin applies. ``assign`` only needs the log to EXIST (a
200); ``approve``/``reject`` additionally require ``SUMMARY: PASSED`` /
``SUMMARY: FAILED`` plus a non-empty ``comment`` for reject. Skipped by the
caller for PI/SLFO requests, which carry no maintenance testreport.
"""

from __future__ import annotations

import re
from logging import getLogger
from typing import TYPE_CHECKING

import requests

from ...support.http import HTTP_TIMEOUT, build_session, resolve_verify

if TYPE_CHECKING:
    from ...support.config import Config
    from ...types.rrid import RequestReviewID

logger = getLogger("mtui.data_sources.obs.preconditions")

# Capture the whole trimmed value, not just the first token, so a trailing
# qualifier ("PASSED with notes") reads as UNKNOWN — matching the plugin's
# whole-value compare rather than approving/rejecting on the first word.
_SUMMARY_RE = re.compile(r"^SUMMARY:\s*(.+?)\s*$", re.MULTILINE)
_COMMENT_RE = re.compile(r"^comment:\s*(.*)$", re.MULTILINE)


def _log_url(config: Config, rrid: RequestReviewID) -> str:
    return f"{config.reports_url.rstrip('/')}/{rrid}/log"


def fetch_testreport_log(config: Config, rrid: RequestReviewID) -> str | None:
    """GET the testreport log; ``None`` when absent (404) or unreachable."""
    url = _log_url(config, rrid)
    session = build_session(resolve_verify(True, config.ssl_verify))
    try:
        response = session.get(url, timeout=HTTP_TIMEOUT)
    except requests.RequestException as e:
        logger.error("could not fetch testreport %s: %s", url, e)
        return None
    if response.status_code == 404:
        return None
    if not response.ok:
        logger.error("testreport %s returned %s", url, response.status_code)
        return None
    return response.text


def summary(log: str) -> str:
    """The upper-cased ``SUMMARY:`` value of a testreport log (else UNKNOWN)."""
    match = _SUMMARY_RE.search(log)
    return match.group(1).upper() if match else "UNKNOWN"


def comment(log: str) -> str:
    """The ``comment:`` value of a testreport log (empty when absent)."""
    match = _COMMENT_RE.search(log)
    return match.group(1).strip() if match else ""
