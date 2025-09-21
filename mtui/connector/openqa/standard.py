"""A connector for the standard "auto" openQA workflow."""

from logging import getLogger
from os.path import join
from typing import Self

from ...types.urls import URLs
from .base import OpenQA

logger = getLogger("mtui.connector.openqa.standard")


class AutoOpenQA(OpenQA):
    """A connector for the standard "auto" openQA workflow."""

    kind = "auto"

    def _has_passed_install_jobs(self, jobs) -> bool:
        """Checks if all install jobs have passed.

        Args:
            jobs: A list of jobs to check.

        Returns:
            True if all install jobs have passed, False otherwise.
        """
        if jobs is None:
            return False

        def normalize(x: str) -> bool:
            if x == "passed" or x == "softfailed":
                return True
            return False

        # get all specified test results and return False if any
        # test FAILS or is Incomplete etc.
        return all(
            normalize(y["result"])
            for y in jobs
            if y["test"] in ["qam-incidentinstall", "qam-incidentinstall-ha"]
        )

    def _pretty_print(self, *args) -> list[str]:
        """Pretty-prints the results of the openQA jobs.

        Args:
            *args: A list of jobs to print.

        Returns:
            A list of formatted strings representing the job results.
        """
        jobs = args[0]
        if not jobs:
            logger.debug("No job - no results")
            return []
        ret: list[str] = []
        ret.append("Results from openQA incidents jobs:\n")
        ret.append("===================================\n")
        ret.append("\n")
        for job in jobs:
            ret.append(
                f"  Job in flavor: {job['settings']['FLAVOR']} - arch: {job['settings']['ARCH']} - version: {job['settings']['VERSION']} - test: {job['test']} - result: {job['result']}\n"
            )
            failed_modules = [
                (module["name"], module["category"])
                for module in job["modules"]
                if module["result"] == "failed"
            ]
            if failed_modules:
                ret.append("    Failed modules:\n")
                for mod in failed_modules:
                    ret.append("      Module: {} in category {} failed\n".format(*mod))
                ret.append("\n")

        return ret

    def _get_logs_url(self, jobs) -> list[URLs] | None:
        """Gets the URLs for the logs of the install jobs.

        Args:
            jobs: A list of jobs.

        Returns:
            A list of `URLs` objects, or None if there are no jobs.
        """
        if not jobs:
            return None
        return [
            URLs(
                job["settings"]["HDD_1"].split("-")[0],
                job["settings"]["ARCH"],
                job["settings"]["VERSION"],
                join(
                    self.host,
                    "tests",
                    str(job["id"]),
                    "file",
                    self.config.openqa_install_logs,
                ),
            )
            for job in jobs
            if job["test"] in ["qam-incidentinstall", "qam-incidentinstall-ha"]
        ]

    def run(self) -> Self:
        """Gets the processed result from openQA for the auto workflow."""
        jobs = self._get_jobs()
        if self._has_passed_install_jobs(jobs):
            self.results = self._get_logs_url(jobs)
        else:
            self.results = None
        self.pp = self._pretty_print(jobs)

        return self

    def __bool__(self) -> bool:
        """Returns `True` if the connector has results, `False` otherwise."""
        return bool(self.pp) or bool(self.results)
