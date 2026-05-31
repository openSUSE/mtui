"""QEM Dashboard incident metadata."""

from typing import Any

from ...types import RequestKind, RequestReviewID
from .client import QEMDashboardClient


class QEMIncident:
    """Incident metadata from QEM Dashboard."""

    def __init__(self, rrid: RequestReviewID, apiurl: str) -> None:
        self.rrid = rrid
        self.incident_number = self._incident_number(rrid)
        self.client = QEMDashboardClient(apiurl)
        self.data: dict[str, Any] | None = self.client.incident(self.incident_number)

    @staticmethod
    def _incident_number(rrid: RequestReviewID) -> str | int:
        if rrid.kind is RequestKind.SLFO and rrid.maintenance_id == "1.2":
            return rrid.review_id
        return rrid.maintenance_id

    def get_incident_name(self) -> str | None:
        """Return the shortest package name for build query compatibility."""
        if not self.data:
            return None
        packages = self.data.get("packages") or []
        if not packages:
            return None
        return str(sorted(packages, key=len)[0])

    def __bool__(self) -> bool:
        return bool(self.data)
