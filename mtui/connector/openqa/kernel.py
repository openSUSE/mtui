"""A connector for the kernel openQA workflow."""

from logging import getLogger
from typing import Self

from ...types.test import Test
from .base import OpenQA

logger = getLogger("mtui.connector.openqa.kernel")


class KernelOpenQA(OpenQA):
    """A connector for the kernel openQA workflow."""

    kind = "kernel"

    @staticmethod
    def _filter_jobs(jobs):
        """Filters jobs to include only kernel-related jobs.

        Args:
            jobs: A list of jobs to filter.

        Returns:
            A generator of kernel-related jobs.
        """
        if jobs is None:
            return None
        return (
            job
            for job in jobs
            if "kernel" in job["settings"]["FLAVOR"].lower().split("-")
        )

    @staticmethod
    def _parse_jobs(jobs) -> list[Test] | None:
        """Parses the jobs and returns a list of `Test` objects.

        Args:
            jobs: A list of jobs to parse.

        Returns:
            A list of `Test` objects, or None if there are no jobs.
        """
        if jobs is None:
            return None
        return [
            y
            for y in (
                Test(
                    x["test"],
                    x["result"],
                    x["id"],
                    x["settings"]["ARCH"],
                    {
                        y["name"]: y["result"]
                        for y in x["modules"]
                        if y["name"] not in ("boot_ltp", "shutdown_ltp")
                    },
                )
                for x in jobs
                if not x["clone_id"]
            )
            if y.result
            not in (
                "skipped",
                "user_cancelled",
                "incomplete",
                "user_restarted",
                "obsoleted",
            )
        ]

    def _pretty_print(self, *args) -> list[str]:
        """Pretty-prints the results of the kernel tests.

        Args:
            *args: Additional arguments (not used).

        Returns:
            A list of formatted strings representing the test results.
        """
        if not self:
            return []
        lines: list[str] = []
        lines.insert(0, f"openQA instance: {self.host} :\n")

        for i, line in enumerate(self._result_matrix(self.results), start=1):
            lines.insert(i, line)

        return lines

    def run(self) -> Self:
        """Gets the processed result from openQA for the kernel workflow."""
        jobs = self._get_jobs()
        jobs = self._filter_jobs(jobs)
        self.results = self._parse_jobs(jobs)
        self.pp = self._pretty_print()
        return self

    @staticmethod
    def _result_matrix(testresults) -> list[str]:
        """Formats the test results into a matrix for display.

        Args:
            testresults: A list of `Test` objects.

        Returns:
            A list of formatted strings representing the test results.
        """
        matrix = []
        for test in testresults:
            text = None
            if test.name.startswith("ltp_"):
                text = "  test: {0:36} {1:<3}arch: {2:8} {1:<3}result: {3}\n".format(
                    test.name, "-", test.arch, test.result
                )
                if test.result == "failed":
                    text.replace("failed", "failed:")
                    for module in test.modules.keys():
                        if test.modules[module] == "failed":
                            text += f"\n      {module}: ...\n"
            if text:
                matrix.append(text)
        return sorted(matrix)
