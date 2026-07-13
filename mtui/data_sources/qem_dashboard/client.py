"""Low-level HTTP client for the QEM Dashboard API."""

from logging import getLogger
from typing import Any

import requests

from ...support.http import HTTP_TIMEOUT, VerifyPolicy, build_session

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

    def __init__(self, apiurl: str, verify: VerifyPolicy = True) -> None:
        """Initialize the client.

        Args:
            apiurl: Base URL of the QEM Dashboard API.
            verify: TLS verification policy (the resolved ``[mtui]
                ssl_verify`` value). Defaults to ``True`` so the client
                verifies certificates unless the user opted out.

        """
        self.apiurl = apiurl.rstrip("/")
        # A shared session pins the verify policy (and silences the
        # InsecureRequestWarning once when verification is disabled),
        # so every request honors the global ssl_verify config.
        self._session = build_session(verify)

    def _get(self, path: str) -> Any | None:
        try:
            response = self._session.get(
                f"{self.apiurl}/{path.lstrip('/')}",
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
