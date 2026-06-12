"""Low-level HTTP client for the QEM Dashboard API."""

from logging import getLogger
from typing import Any

import requests

from ...support.http import HTTP_TIMEOUT

logger = getLogger("mtui.connector.qem_dashboard")

# Job result statuses that should be reported individually in the exported
# log. Every other status (passed, softfailed, ...) is collapsed into a
# per-group summary count to keep the report short and reviewable.
FAILED_RESULTS: frozenset[str] = frozenset({"failed", "incomplete", "timeout_exceeded"})

# Wall-clock cap per future in the parallel fan-out. Defense-in-depth on
# top of the shared per-request HTTP_TIMEOUT; a stuck worker won't block
# the whole batch.
_FUTURE_TIMEOUT: float = 60.0


class QEMDashboardClient:
    """Small read-only client for the QEM Dashboard API."""

    def __init__(self, apiurl: str) -> None:
        self.apiurl = apiurl.rstrip("/")

    def _get(self, path: str, **params) -> Any | None:
        try:
            response = requests.get(
                f"{self.apiurl}/{path.lstrip('/')}",
                params=params or None,
                headers={"Accept": "application/json"},
                timeout=HTTP_TIMEOUT,
            )
            response.raise_for_status()
            return response.json()
        except requests.exceptions.RequestException as e:
            logger.debug("QEM Dashboard request failed: %s", e)
            return None
        except ValueError as e:
            logger.debug("QEM Dashboard returned invalid JSON: %s", e)
            return None

    def incident(self, incident_number: str | int) -> dict[str, Any] | None:
        return self._get(f"incidents/{incident_number}")

    def incident_settings(self, incident_number: str | int) -> list[dict[str, Any]]:
        return self._get(f"incident_settings/{incident_number}") or []

    def update_settings(self, incident_number: str | int) -> list[dict[str, Any]]:
        return self._get(f"update_settings/{incident_number}") or []

    def incident_jobs(self, incident_settings_id: int) -> list[dict[str, Any]]:
        return self._get(f"jobs/incident/{incident_settings_id}") or []

    def update_jobs(self, update_settings_id: int) -> list[dict[str, Any]]:
        return self._get(f"jobs/update/{update_settings_id}") or []
