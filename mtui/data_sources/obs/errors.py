"""Internal error types for the native OBS backend (no osc import).

The caller-facing :class:`~mtui.support.exceptions.ObsConfigError` lives in
``mtui.support.exceptions``; these transport/timeout subtypes are internal to
the backend. All subclass :class:`ObsError`, so the ``OSC`` facade catches the
whole family with one ``except ObsError`` and returns ``False``.
"""

from __future__ import annotations

from ...support.exceptions import ObsError


class ObsApiError(ObsError):
    """An OBS API call returned a non-2xx HTTP response."""

    def __init__(self, status: int, url: str, summary: str = "") -> None:
        """Record the status, URL and any parsed error summary."""
        self.status = status
        self.url = url
        self.summary = summary
        detail = f": {summary}" if summary else ""
        super().__init__(f"OBS API returned {status} for {url}{detail}")


class ObsTimeoutError(ObsError):
    """A native OBS operation exceeded its coarse between-calls budget."""
