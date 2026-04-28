"""Connector for the QEM Dashboard API."""

from logging import getLogger
from os.path import join
from typing import Any, Self

import requests

from ..types import RequestReviewID, URLs

logger = getLogger("mtui.connector.qem_dashboard")


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


class QEMIncident:
    """Incident metadata from QEM Dashboard."""

    def __init__(self, rrid: RequestReviewID, apiurl: str) -> None:
        self.rrid = rrid
        self.incident_number = self._incident_number(rrid)
        self.client = QEMDashboardClient(apiurl)
        self.data: dict[str, Any] | None = self.client.incident(self.incident_number)

    @staticmethod
    def _incident_number(rrid: RequestReviewID) -> str | int:
        if rrid.kind == "SLFO" and rrid.maintenance_id == "1.2":
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

    def get_version(self) -> str | None:
        """Best-effort version derived from the first dashboard channel."""
        if not self.data:
            return None
        channels = self.data.get("channels") or []
        if not channels:
            return None
        parts = str(channels[0]).split(":")[-2].split("-")
        if len(parts) < 2:
            return None
        if parts[0] in ("SLE", "SLES") and len(parts) > 2:
            return f"{parts[1]}-{parts[2]}"
        return f"{parts[0]}-{parts[1]}"

    def __bool__(self) -> bool:
        return bool(self.data)


class DashboardAutoOpenQA:
    """Dashboard-backed auto workflow data provider."""

    kind = "auto"

    def __init__(
        self, config, host: str, incident: QEMIncident, rrid: RequestReviewID
    ) -> None:
        self.config = config
        self.host = host
        self.incident = incident
        self.rrid = rrid
        self.client = incident.client
        self.pp: list[str] = []
        self.results: list[URLs] | None = None
        self.jobs: list[dict[str, Any]] = []

    def _load_jobs(self) -> list[dict[str, Any]]:
        jobs: list[dict[str, Any]] = []
        incident_settings = self.client.incident_settings(self.incident.incident_number)
        for setting in incident_settings:
            setting_id = setting.get("id")
            if setting_id is None:
                continue
            jobs.extend(
                self._normalize_job(job, "incident", setting)
                for job in self.client.incident_jobs(setting_id)
            )

        update_settings = self.client.update_settings(self.incident.incident_number)
        for setting in update_settings:
            setting_id = setting.get("id")
            if setting_id is None:
                continue
            jobs.extend(
                self._normalize_job(job, "aggregate", setting)
                for job in self.client.update_jobs(setting_id)
            )

        return jobs

    @staticmethod
    def _normalize_job(
        job: dict[str, Any], source: str, setting: dict[str, Any]
    ) -> dict[str, Any]:
        settings = setting.get("settings") or {}
        normalized = {
            "id": job.get("job_id"),
            "test": job.get("name"),
            "result": job.get("status"),
            "source": source,
            "job_group": job.get("job_group"),
            "group_id": job.get("group_id"),
            "obsolete": job.get("obsolete", False),
            "settings": {
                "DISTRI": job.get("distri") or settings.get("DISTRI"),
                "FLAVOR": job.get("flavor") or setting.get("flavor"),
                "ARCH": job.get("arch") or setting.get("arch"),
                "VERSION": job.get("version") or setting.get("version"),
                "BUILD": job.get("build") or setting.get("build"),
            },
            "dashboard_setting": setting,
        }
        if source == "aggregate":
            normalized["product"] = setting.get("product")
            normalized["repohash"] = setting.get("repohash")
            normalized["incidents"] = setting.get("incidents") or []
        return normalized

    @staticmethod
    def _has_passed_install_jobs(jobs) -> bool:
        if jobs is None:
            return False

        def normalize(result: str | None) -> bool:
            return result in ("passed", "softfailed")

        return all(
            normalize(job.get("result"))
            for job in jobs
            if job.get("test") in ["qam-incidentinstall", "qam-incidentinstall-ha"]
        )

    def _get_logs_url(self, jobs) -> list[URLs] | None:
        if not jobs:
            return None
        return [
            URLs(
                str(job["settings"].get("DISTRI") or self.config.openqa_install_distri),
                str(job["settings"].get("ARCH") or ""),
                str(job["settings"].get("VERSION") or ""),
                join(
                    self.host,
                    "tests",
                    str(job["id"]),
                    "file",
                    self.config.openqa_install_logs,
                ),
            )
            for job in jobs
            if job.get("test") in ["qam-incidentinstall", "qam-incidentinstall-ha"]
            and job.get("result") in ("passed", "softfailed")
            and job.get("id") is not None
        ]

    def _pretty_print(self, jobs) -> list[str]:
        if not jobs:
            logger.debug("No dashboard jobs - no results")
            return []

        ret = [
            "\n",
            "Results from openQA jobs:\n",
            "=========================\n",
            "\n",
        ]
        self._pretty_print_section(ret, "Incident jobs", jobs, "incident")
        self._pretty_print_section(ret, "Aggregate jobs", jobs, "aggregate")
        return ret

    @staticmethod
    def _pretty_print_section(
        ret: list[str], title: str, jobs: list[dict[str, Any]], source: str
    ) -> None:
        section_jobs = [job for job in jobs if job.get("source") == source]
        if not section_jobs:
            return

        ret.append(f"{title}:\n")
        for job in section_jobs:
            settings = job["settings"]
            if source == "aggregate":
                ret.append(
                    f"  product: {job.get('product')} - build: {settings.get('BUILD')} - arch: {settings.get('ARCH')} - test: {job.get('test')} - result: {job.get('result')}\n"
                )
            else:
                ret.append(
                    f"  flavor: {settings.get('FLAVOR')} - arch: {settings.get('ARCH')} - version: {settings.get('VERSION')} - test: {job.get('test')} - result: {job.get('result')}\n"
                )
        ret.append("\n")

    def run(self) -> Self:
        self.jobs = self._load_jobs()
        if self._has_passed_install_jobs(self.jobs):
            self.results = self._get_logs_url(self.jobs)
        else:
            self.results = None
        self.pp = self._pretty_print(self.jobs)
        return self

    def __bool__(self) -> bool:
        return bool(self.pp) or bool(self.results)
