"""OSC review backend: native direct-OBS-API calls (no osc, no subprocess).

The ``OSC(config, rrid)`` seam exposes five bool / never-raising methods that
call the OBS/IBS API directly via :mod:`mtui.data_sources.obs`, reading
credentials from the user's oscrc (located like ``osc``: ``$OSC_CONFIG`` →
``$XDG_CONFIG_HOME/osc/oscrc`` → ``~/.oscrc``). It replaces the historical
shell-out to the external ``osc qam`` plugin.
"""

from collections.abc import Callable
from logging import getLogger

from ..support.config import Config
from ..types.rrid import RequestReviewID
from .obs import qam as obs_qam
from .obs.client import ObsClient
from .obs.oscrc import read_credentials

logger = getLogger("mtui.connector.oscqam")


class OSC:
    """Native OBS review backend (approve/assign/unassign/reject/comment)."""

    def __init__(self, config: Config, rrid: RequestReviewID) -> None:
        """Store the config and target review; construction cannot fail.

        Args:
            config: The application's Config instance.
            rrid: The target review's RequestReviewID.

        """
        self.config = config
        self.rrid = rrid

    def _native(self, op: Callable[[ObsClient, str], None]) -> bool:
        """Run a native OBS operation, converting any failure into ``False``.

        Everything — reading oscrc, loading the key, building the session,
        the authenticated calls, XML parsing — happens here inside one
        try/except, because callers (apicall.py / approve.py) invoke the
        seam methods bare with no guard of their own. The catch is
        deliberately broad (``Exception``, not a narrow tuple): besides the
        expected ObsError / requests / paramiko / ElementTree failures, edge
        inputs raise plain ``ValueError`` (a non-PEM key, a lone surrogate in
        the body) or ``RuntimeError`` (``Path.expanduser`` with no home), and
        the never-raise contract must hold for those too. ``BaseException``
        is intentionally NOT caught, so KeyboardInterrupt/SystemExit
        propagate.
        """
        try:
            credentials = read_credentials(self.config.obs_api_url)
            client = ObsClient(self.config, credentials)
            op(client, credentials.user)
        except Exception as e:  # noqa: BLE001 - documented never-raise seam
            logger.error("OBS operation on %s failed: %s", self.rrid, e)
            return False
        return True

    def approve(self, group: list[str]) -> bool:
        """Approves a review request. Returns ``True`` on success."""
        return self._native(
            lambda c, u: obs_qam.approve(c, self.config, self.rrid, u, group or [])
        )

    def assign(self, group: list[str]) -> bool:
        """Assigns a review request. Returns ``True`` on success."""
        return self._native(
            lambda c, u: obs_qam.assign(c, self.config, self.rrid, u, group or [])
        )

    def unassign(self, group: list[str]) -> bool:
        """Unassigns a review request. Returns ``True`` on success."""
        return self._native(
            lambda c, u: obs_qam.unassign(c, self.config, self.rrid, u, group or [])
        )

    def comment(self, comment: str) -> bool:
        """Adds a comment to a review request. Returns ``True`` on success."""
        return self._native(lambda c, _u: obs_qam.comment(c, self.rrid, comment))

    def reject(self, group: list[str], reason: str, message: str) -> bool:
        """Rejects a review request. Returns ``True`` on success."""
        return self._native(
            lambda c, u: obs_qam.reject(
                c, self.config, self.rrid, u, group or [], reason, message
            )
        )
