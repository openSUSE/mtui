"""A read-only client for the TeReGen Report API (``qam.suse.de/api/v1``).

TeReGen serves the generated test-report data over HTTP: the decoded
``metadata.json`` plus template status. mtui prefers it as the **source of
truth** for report metadata (priority, deadline, review groups, product-composer
routing, …), falling back to the locally checked-out ``metadata.json`` when the
API is unreachable or doesn't carry a field.

Every call is best-effort: any failure returns ``None`` so a TeReGen hiccup
never breaks the surrounding command. The base URL comes from ``[teregen] api``
(defaults to ``https://qam.suse.de/api/v1``).

The one documented exception is :meth:`regenerate` (a write): it returns ``None``
only when TeReGen is *unreachable*, and ``{"error": ...}`` when the server
*refuses*, so callers can tell the two apart.
"""

from __future__ import annotations

import threading
import time
from collections.abc import Callable
from dataclasses import dataclass
from logging import getLogger
from typing import Any

import requests

from ..support.config import Config
from ..support.http import HTTP_TIMEOUT, build_session, resolve_verify

logger = getLogger("mtui.connector.teregen")


@dataclass(frozen=True)
class RegenOutcome:
    """The result of a regenerate-and-wait attempt (see :meth:`TeReGen.regenerate_and_wait`).

    Exactly one of the flags is meaningful at a time; ``ok`` is the only success.
    The fields carry just enough for a caller to message the user and decide
    whether to reload:

    - ``ok``: the job finished and the template is built.
    - ``unreachable``: TeReGen could not be asked at all.
    - ``error``: the server refused the request (e.g. the template was edited).
    - ``state`` / ``minion_error``: set when the job ran but did not finish
      (timed out, was cancelled, or failed).
    - ``job``: the enqueued job id, for logging.
    """

    ok: bool
    unreachable: bool = False
    error: str | None = None
    state: str | None = None
    minion_error: str | None = None
    job: Any | None = None


class TeReGen:
    """Best-effort read-only TeReGen Report API client."""

    def __init__(self, config: Config) -> None:
        self.base = (config.teregen_api or "").rstrip("/")
        self._verify = resolve_verify(True, config.ssl_verify)

    def _get(self, path: str, params: dict[str, str] | None = None) -> Any | None:
        url = f"{self.base}/{path.lstrip('/')}"
        try:
            r = build_session(self._verify).get(
                url, params=params, timeout=HTTP_TIMEOUT
            )
            r.raise_for_status()
            return r.json()
        except (requests.exceptions.RequestException, ValueError) as e:
            logger.debug("TeReGen GET %s failed: %s", path, e)
            return None

    def info(self, rrid: object) -> dict[str, Any] | None:
        """The main report endpoint (``GET /reports/{id}``): id, file list, and
        the live ``priority``/``deadline`` (refreshed from SMELT for SLFO)."""
        d = self._get(f"reports/{rrid}")
        return d if isinstance(d, dict) else None

    def metadata(self, rrid: object) -> dict[str, Any] | None:
        """The decoded ``metadata.json`` for a report (file contents), or ``None``."""
        d = self._get(f"reports/{rrid}/metadata")
        return d if isinstance(d, dict) else None

    def status(self, rrid: object) -> dict[str, Any] | None:
        """Template existence + Minion job state for a report, or ``None``."""
        d = self._get(f"reports/{rrid}/status")
        return d if isinstance(d, dict) else None

    def priority_deadline(self, rrid: object) -> tuple[int | None, str | None]:
        """``(priority, deadline)`` from the main report endpoint, best-effort."""
        info = self.info(rrid)
        if not info:
            return None, None
        return info.get("priority"), info.get("deadline")

    def checkers(self, rrid: object) -> list[Any] | None:
        """Live checker (build-check) result runs for a report, or ``None``."""
        d = self._get(f"reports/{rrid}/checkers")
        return d.get("checkers") if isinstance(d, dict) else None

    def updates(
        self,
        review_group: str | None = None,
        status: str | None = None,
        assignee: str | None = None,
        unassigned: bool = False,
        with_assignment: bool = False,
        no_cache: bool = False,
    ) -> list[Any] | None:
        """The unreleased update queue (live from SMELT), or ``None``.

        Optional ``review_group`` / ``status`` narrow the queue server-side.

        Assignment exposure (each maps to a query param of the same name):

        - ``assignee``: keep only updates assigned to that user (any qam group);
          implies server-side ``status=testing``.
        - ``unassigned``: keep only updates with no assignee; implies
          ``status=testing``.
        - ``with_assignment``: include assignment on every row without
          filtering; implies ``status=testing``.
        - ``no_cache``: bypass the server's short assignment cache (use for the
          pickup moment).
        """
        params = {
            k: v
            for k, v in (
                ("review_group", review_group),
                ("status", status),
                ("assignee", assignee),
            )
            if v
        }
        for flag, name in (
            (unassigned, "unassigned"),
            (with_assignment, "with_assignment"),
            (no_cache, "no_cache"),
        ):
            if flag:
                params[name] = "1"
        d = self._get("updates", params=params or None)
        return d.get("updates") if isinstance(d, dict) else None

    def regenerate(
        self,
        rrid: object,
        *,
        force_overwrite: bool = False,
        ignore_inconsistent: bool = False,
    ) -> dict[str, Any] | None:
        """Enqueue a template regeneration job (``POST /reports/{id}/regenerate``).

        ``force_overwrite`` overwrites an existing but *unedited* template;
        ``ignore_inconsistent`` regenerates despite inconsistent metadata (e.g.
        an arch list that disagrees with the build).

        Returns the decoded JSON body: ``{"id", "job"}`` on success (HTTP 202)
        or ``{"error": ...}`` when the server refuses (HTTP 409, e.g. the
        template already exists or was hand-edited). Returns ``None`` only when
        TeReGen is unreachable — so callers can tell "refused" apart from
        "couldn't ask".
        """
        url = f"{self.base}/reports/{rrid}/regenerate"
        payload = {
            "force_overwrite": force_overwrite,
            "ignore_inconsistent": ignore_inconsistent,
        }
        try:
            r = build_session(self._verify).post(
                url, json=payload, timeout=HTTP_TIMEOUT
            )
        except requests.exceptions.RequestException as e:
            logger.debug("TeReGen POST regenerate %s failed: %s", rrid, e)
            return None
        try:
            body = r.json()
        except ValueError:
            body = None
        if isinstance(body, dict):
            return body
        # A 202 with no/!JSON body still means "enqueued"; anything else is an
        # error the caller should surface.
        return {} if r.status_code == 202 else {"error": f"HTTP {r.status_code}"}

    def wait_for_template(
        self,
        rrid: object,
        *,
        interval: float = 5.0,
        timeout: float = 600.0,
        should_stop: Callable[[], bool] | None = None,
    ) -> dict[str, Any] | None:
        """Poll :meth:`status` until the latest generate job finishes or fails.

        Blocks up to ``timeout`` seconds, polling every ``interval`` seconds.
        Returns the final status dict once ``minion_state`` is ``finished`` or
        ``failed``; returns the last seen status (or ``None``) on timeout. The
        caller inspects ``minion_state`` / ``minion_error`` to decide success.

        ``should_stop`` makes the wait interruptible: it is polled before each
        sleep and the inter-poll sleep itself is cancellable, so a caller can
        abandon the wait promptly (e.g. on Ctrl-C) and get back the last seen
        status. The sleep uses an :class:`threading.Event` rather than
        :func:`time.sleep` so the cancellation takes effect immediately instead
        of after up to ``interval`` seconds.
        """
        deadline = time.monotonic() + timeout
        sleeper = threading.Event()
        last: dict[str, Any] | None = None
        while True:
            last = self.status(rrid)
            if isinstance(last, dict) and last.get("minion_state") in (
                "finished",
                "failed",
            ):
                return last
            if (should_stop is not None and should_stop()) or (
                time.monotonic() >= deadline
            ):
                return last
            # Interruptible sleep: returns early once ``should_stop`` flips so
            # we re-check and exit on the next loop without waiting out the full
            # interval.
            if should_stop is None:
                sleeper.wait(interval)
            else:
                step = 0.1
                waited = 0.0
                while waited < interval and not should_stop():
                    sleeper.wait(min(step, interval - waited))
                    waited += step

    def regenerate_and_wait(
        self,
        rrid: object,
        *,
        force_overwrite: bool = False,
        ignore_inconsistent: bool = False,
        should_stop: Callable[[], bool] | None = None,
    ) -> RegenOutcome:
        """Enqueue a regeneration and wait for the job to finish.

        Bundles :meth:`regenerate` + :meth:`wait_for_template` into the single
        protocol both the ``regenerate`` command and the stale-template loader
        share, returning a :class:`RegenOutcome` the caller maps to its own
        messaging and reload strategy. ``should_stop`` is forwarded so the wait
        stays interruptible.
        """
        result = self.regenerate(
            rrid,
            force_overwrite=force_overwrite,
            ignore_inconsistent=ignore_inconsistent,
        )
        if result is None:
            return RegenOutcome(ok=False, unreachable=True)
        if result.get("error"):
            return RegenOutcome(ok=False, error=str(result["error"]))

        job = result.get("job")
        status = self.wait_for_template(rrid, should_stop=should_stop)
        state = status.get("minion_state") if isinstance(status, dict) else None
        if state != "finished":
            minion_error = (
                status.get("minion_error") if isinstance(status, dict) else None
            )
            return RegenOutcome(
                ok=False, state=state, minion_error=minion_error, job=job
            )
        return RegenOutcome(ok=True, job=job)
