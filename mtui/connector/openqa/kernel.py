from collections import namedtuple
from logging import getLogger

from .base import OpenQA

logger = getLogger("mtui.connector.openqa.kernel")

Test = namedtuple("test", ["name", "result", "test_id", "arch", "modules"])


class KernelOpenQA(OpenQA):
    kind = "kernel"

    @staticmethod
    def _filter_jobs(jobs):
        if jobs is None:
            return None
        return (
            job
            for job in jobs
            if "kernel" in job["settings"]["FLAVOR"].lower().split("-")
        )

    @staticmethod
    def _parse_jobs(jobs):
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

    def _pretty_print(self):
        if not self:
            return []
        lines = []
        lines.insert(0, f"openQA instance: {self.host} :\n")

        for i, l in enumerate(self._result_matrix(self.results), start=1):
            lines.insert(i, l)

        return lines

    def run(self):
        jobs = self._get_jobs()
        jobs = self._filter_jobs(jobs)
        self.results = self._parse_jobs(jobs)
        self.pp = self._pretty_print()
        return self

    @staticmethod
    def _result_matrix(testresults):
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
