"""Dashboard-backed auto workflow data provider for openQA."""

import concurrent.futures
from logging import getLogger
from os.path import join
from typing import Any, Self

from ...support.concurrency import ContextExecutor
from ...types import RequestReviewID, URLs
from ..openqa_install import install_logfile_for
from .client import _FUTURE_TIMEOUT, FAILED_RESULTS
from .incident import QEMIncident

logger = getLogger("mtui.connector.qem_dashboard")


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
        # Fetch the two top-level settings lists concurrently; they are
        # independent of each other.
        incident_number = self.incident.incident_number
        with ContextExecutor() as executor:
            incident_settings_future = executor.submit(
                self.client.incident_settings, incident_number
            )
            update_settings_future = executor.submit(
                self.client.update_settings, incident_number
            )
            incident_settings = self._await_settings(
                incident_settings_future, "incident_settings"
            )
            update_settings = self._await_settings(
                update_settings_future, "update_settings"
            )

        # Build a flat task list of (source, setting, fetcher) preserving
        # the original insertion order: all incident settings first, then
        # all update settings. Downstream pretty-printing relies on this
        # order.
        tasks: list[tuple[str, dict[str, Any], Any]] = []
        for setting in incident_settings:
            setting_id = setting.get("id")
            if setting_id is None:
                continue
            tasks.append(("incident", setting, setting_id))
        for setting in update_settings:
            setting_id = setting.get("id")
            if setting_id is None:
                continue
            tasks.append(("aggregate", setting, setting_id))

        if not tasks:
            return []

        # Fan out the per-setting jobs fetches concurrently. Read futures
        # back in submission order so the resulting `jobs` list keeps the
        # same order as the sequential implementation.
        def _fetch(source: str, setting_id: int) -> list[dict[str, Any]]:
            if source == "incident":
                return self.client.incident_jobs(setting_id)
            return self.client.update_jobs(setting_id)

        with ContextExecutor() as executor:
            futures = [
                executor.submit(_fetch, source, setting_id)
                for source, _, setting_id in tasks
            ]

            jobs: list[dict[str, Any]] = []
            for (source, setting, setting_id), future in zip(
                tasks, futures, strict=True
            ):
                try:
                    setting_jobs = future.result(timeout=_FUTURE_TIMEOUT)
                except concurrent.futures.TimeoutError:
                    logger.warning(
                        "QEM Dashboard %s jobs fetch for setting %s timed out "
                        "after %.0fs; skipping",
                        source,
                        setting_id,
                        _FUTURE_TIMEOUT,
                    )
                    future.cancel()
                    continue
                for job in setting_jobs:
                    normalized = self._normalize_job(job, source, setting)
                    if self._is_obsolete(normalized):
                        logger.debug(
                            "dropping obsoleted %s job %s (%s)",
                            source,
                            normalized.get("id"),
                            normalized.get("test"),
                        )
                        continue
                    jobs.append(normalized)

        return jobs

    @staticmethod
    def _await_settings(
        future: "concurrent.futures.Future[list[dict[str, Any]]]",
        label: str,
    ) -> list[dict[str, Any]]:
        """Wait for a top-level settings future, returning [] on timeout.

        Mirrors `QEMDashboardClient.{incident,update}_settings` returning
        [] on a swallowed `_get` failure, so callers see the same shape
        whether the failure was an HTTP error or an executor timeout.
        """
        try:
            return future.result(timeout=_FUTURE_TIMEOUT)
        except concurrent.futures.TimeoutError:
            logger.warning(
                "QEM Dashboard %s fetch timed out after %.0fs; treating as empty",
                label,
                _FUTURE_TIMEOUT,
            )
            future.cancel()
            return []

    @staticmethod
    def _normalize_job(
        job: dict[str, Any], source: str, setting: dict[str, Any]
    ) -> dict[str, Any]:
        settings = setting.get("settings", {}) or {}
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
    def _is_obsolete(job: dict[str, Any]) -> bool:
        """True for a superseded run that must not count toward results.

        When an openQA job is retriggered the dashboard keeps the older
        run but marks it superseded — either with an ``obsolete`` flag or
        an ``"obsoleted"`` result. Both must be dropped so a stale failure
        does not poison the install verdict (:meth:`_has_passed_install_jobs`)
        or surface as a phantom entry in the failed-jobs listing. Mirrors
        :func:`mtui.data_sources.oqa_search.search.incident_jobs`, which
        filters ``result == "obsoleted"``; only the current run matters.
        """
        return bool(job.get("obsolete")) or job.get("result") == "obsoleted"

    @staticmethod
    def _normalize_result(result: str | None) -> bool:
        return result in ("passed", "softfailed")

    @classmethod
    def _has_passed_install_jobs(cls, jobs) -> bool:
        if jobs is None:
            return False

        return all(
            cls._normalize_result(job.get("result"))
            for job in jobs
            if "qam-incidentinstall" in (job.get("test", "") or "")
        )

    def _get_logs_url(self, jobs) -> list[URLs] | None:
        if not jobs:
            return None

        return [
            URLs(
                str(job["settings"].get("DISTRI", self.config.openqa_install_distri)),
                str(job["settings"].get("ARCH", "")),
                str(job["settings"].get("VERSION", "")),
                join(
                    self.host,
                    "tests",
                    str(job["id"]),
                    "file",
                    install_logfile_for(
                        job.get("test", "") or "", self.config.openqa_install_logs
                    ),
                ),
                str(job.get("result", "") or ""),
            )
            for job in jobs
            if (
                "qam-incidentinstall" in (job.get("test", "") or "")
                and self._normalize_result(job.get("result"))
            )
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
    def _job_url(host: str, job_id: Any) -> str:
        if job_id is None:
            return ""
        return f"{host.rstrip('/')}/tests/{job_id}"

    # Counter keys, in display order. `total` is always kept; the others
    # are only printed when non-zero so the Summary block stays scannable.
    _COUNT_KEYS: tuple[str, ...] = (
        "passed",
        "softfailed",
        "failed",
        "incomplete",
        "timeout_exceeded",
        "other",
    )

    @staticmethod
    def _val(value: Any) -> str:
        return str(value) if value not in (None, "") else "unknown"

    @classmethod
    def _group_key(cls, job: dict[str, Any], source: str) -> tuple[str, str, str]:
        settings = job.get("settings") or {}
        if source == "aggregate":
            return (
                cls._val(job.get("product")),
                cls._val(settings.get("BUILD")),
                cls._val(settings.get("ARCH")),
            )
        return (
            cls._val(settings.get("VERSION")),
            cls._val(settings.get("FLAVOR")),
            cls._val(settings.get("ARCH")),
        )

    @classmethod
    def _format_counts(cls, counts: dict[str, int]) -> str:
        """Render counts dropping zero entries; `total` is always last."""
        parts = [f"{key}: {counts[key]}" for key in cls._COUNT_KEYS if counts[key]]
        parts.append(f"total: {counts['total']}")
        return ", ".join(parts)

    @staticmethod
    def _empty_counts() -> dict[str, int]:
        return {
            "passed": 0,
            "softfailed": 0,
            "failed": 0,
            "incomplete": 0,
            "timeout_exceeded": 0,
            "other": 0,
            "total": 0,
        }

    @staticmethod
    def _has_problems(counts: dict[str, int]) -> bool:
        return bool(
            counts["failed"]
            or counts["incomplete"]
            or counts["timeout_exceeded"]
            or counts["other"]
        )

    @staticmethod
    def _format_group_header(
        source: str, key: tuple[str, str, str], *, hoisted_build: bool = False
    ) -> str:
        if source == "aggregate":
            product, build, arch = key
            if hoisted_build:
                return f"    product: {product} - arch: {arch}"
            return f"    product: {product} - build: {build} - arch: {arch}"
        version, flavor, arch = key
        return f"    version: {version} - flavor: {flavor} - arch: {arch}"

    @staticmethod
    def _format_folded_header(
        source: str, fold_key: tuple[str, ...], *, hoisted_build: bool
    ) -> str:
        if source == "aggregate":
            if hoisted_build:
                (product,) = fold_key
                return f"    product: {product}"
            product, build = fold_key
            return f"    product: {product} - build: {build}"
        version, flavor = fold_key
        return f"    version: {version} - flavor: {flavor}"

    @staticmethod
    def _failed_group_header(
        source: str, key: tuple[str, str, str], n_failed: int, *, hoisted_build: bool
    ) -> str:
        if source == "aggregate":
            product, build, arch = key
            if hoisted_build:
                return f"    {product} / {arch} ({n_failed} failed):\n"
            return f"    {product} / {build} / {arch} ({n_failed} failed):\n"
        version, flavor, arch = key
        return f"    {version} / {flavor} / {arch} ({n_failed} failed):\n"

    def _pretty_print_section(
        self,
        ret: list[str],
        title: str,
        jobs: list[dict[str, Any]],
        source: str,
    ) -> None:
        section_jobs = [job for job in jobs if job.get("source") == source]
        if not section_jobs:
            return

        ret.append(f"{title}:\n")

        # Hoist a shared aggregate BUILD when every job in the section uses
        # the same one; this strips ~80 redundant `build: …` repetitions.
        hoisted_build: str | None = None
        if source == "aggregate":
            builds = {
                self._val((job.get("settings", {}) or {}).get("BUILD"))
                for job in section_jobs
            }
            if len(builds) == 1:
                hoisted_build = next(iter(builds))
                ret.append(f"  build: {hoisted_build}\n")

        # Build per-group counts in insertion order so the summary mirrors
        # the order in which the dashboard returned the jobs.
        groups: dict[tuple[str, str, str], dict[str, int]] = {}
        failed_by_group: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
        for job in section_jobs:
            key = self._group_key(job, source)
            counts = groups.setdefault(key, self._empty_counts())
            counts["total"] += 1
            result = job.get("result") or "other"
            if result in counts and result != "total":
                counts[result] += 1
            else:
                counts["other"] += 1
            if result in FAILED_RESULTS:
                failed_by_group.setdefault(key, []).append(job)

        # Split into problem and all-passed groups; problem groups stay
        # per-arch so reviewers see exactly which arch failed. All-passed
        # groups fold across architectures into one row.
        problem_keys = [
            key for key, counts in groups.items() if self._has_problems(counts)
        ]
        passed_keys = [
            key for key, counts in groups.items() if not self._has_problems(counts)
        ]

        ret.append("  Summary:\n")

        # Problem groups first, in original insertion order.
        ret.extend(
            f"  {self._format_group_header(source, key, hoisted_build=hoisted_build is not None)}"
            f" -> {self._format_counts(groups[key])}\n"
            for key in problem_keys
        )

        # Fold all-passed groups. For incidents, fold by (version, flavor).
        # For aggregates, fold by (product,) when build is hoisted, else by
        # (product, build) so the build stays visible in mixed-build sections.
        folded: dict[tuple[str, ...], dict[str, Any]] = {}
        for key in passed_keys:
            if source == "aggregate":
                product, build, arch = key
                fold_key: tuple[str, ...] = (
                    (product,) if hoisted_build is not None else (product, build)
                )
            else:
                version, flavor, arch = key
                fold_key = (version, flavor)
            entry = folded.setdefault(
                fold_key,
                {"archs": [], "counts": self._empty_counts()},
            )
            if arch not in entry["archs"]:
                entry["archs"].append(arch)
            for ckey in self._COUNT_KEYS:
                entry["counts"][ckey] += groups[key][ckey]
            entry["counts"]["total"] += groups[key]["total"]

        for fold_key, entry in folded.items():
            archs = ", ".join(entry["archs"])
            n_archs = len(entry["archs"])
            ret.append(
                f"  {self._format_folded_header(source, fold_key, hoisted_build=hoisted_build is not None)}"
                f" - archs: {archs}"
                f" -> {self._format_counts(entry['counts'])}"
                f" ({n_archs} arch{'es' if n_archs != 1 else ''})\n"
            )

        # Failed jobs, nested under their group header so the redundant
        # product/build/arch prefix on each line disappears.
        if failed_by_group:
            ret.append("  Failed jobs:\n")
            for key in problem_keys:
                fjobs = failed_by_group.get(key, [])
                if not fjobs:
                    continue
                ret.append(
                    self._failed_group_header(
                        source, key, len(fjobs), hoisted_build=hoisted_build is not None
                    )
                )
                # Pad test name with spaces so URLs align inside this group.
                width = max(len(job.get("test") or "") for job in fjobs)
                for job in fjobs:
                    test = job.get("test") or ""
                    url = self._job_url(self.host, job.get("id"))
                    result = job.get("result")
                    suffix = f"  {url}" if url else ""
                    if result == "failed":
                        ret.append(f"      {test.ljust(width)}{suffix}\n")
                    else:
                        ret.append(f"      {test.ljust(width)}  [{result}]{suffix}\n")
        elif problem_keys:
            # Problem groups exist but none carry a failed/incomplete/
            # timeout_exceeded job, so `failed_by_group` is empty — their
            # problems are entirely in the `other` bucket (still-running,
            # parallel_failed, skipped, ...). The Summary already flags
            # them; don't claim success (the old bug) and don't print an
            # empty `Failed jobs:` block.
            ret.append(
                "  No failed jobs, but some groups need review (see Summary above).\n"
            )
        else:
            ret.append("  All jobs passed.\n")
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
