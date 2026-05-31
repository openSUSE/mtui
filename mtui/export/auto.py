"""An exporter for the automatic workflow."""

import ssl
from http.client import RemoteDisconnected
from itertools import zip_longest
from logging import getLogger
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import urlopen

from ..types import FileList
from .base import BaseExport

logger = getLogger("mtui.export.auto")

no_verify = ssl._create_unverified_context()  # noqa: SLF001


class AutoExport(BaseExport):
    """An exporter for the automatic workflow."""

    @staticmethod
    def _install_job_line(result) -> str:
        status = result.result.upper() if result.result else "UNKNOWN"
        job_url = result.url.rsplit("/file/", 1)[0]
        return (
            f"{result.distri}_{result.version}_{result.arch} => {status}: {job_url}\n"
        )

    def _install_status(self) -> str:
        auto = self.openqa.auto
        results = auto.results if auto is not None else None
        if not results:
            # No results means the install tests have not run, are still
            # running, or could not be fetched - which is distinct from a
            # genuine FAILED outcome.
            return "UNKNOWN"
        return (
            "PASSED"
            if all(result.result in {"passed", "softfailed"} for result in results)
            else "FAILED"
        )

    def install_results(self) -> None:
        """Adds installation results to the template."""
        status_line = (
            "Installation tests done in openQA with following results: "
            f"{self._install_status()}\n"
        )
        auto = self.openqa.auto
        results = auto.results if auto is not None else []
        result_lines = [self._install_job_line(result) for result in results or []]

        try:
            start = self.template.index("Install tests:\n") - 1
        except ValueError:
            start = len(self.template)
            if "## export MTUI:" in self.template[-1]:
                start -= 1
            self.template[start:start] = [
                "##############\n",
                "Install tests:\n",
                "##############\n",
                "\n",
            ]

        try:
            end = self.template.index("Links for update logs:\n", start)
            while end > start and self.template[end - 1] == "\n":
                end -= 1
        except ValueError:
            try:
                end = self.template.index("## export MTUI:", start)
            except ValueError:
                end = len(self.template)

        block = [
            "##############\n",
            "Install tests:\n",
            "##############\n",
            "\n",
            status_line,
            "\n",
            *result_lines,
            "\n",
        ]
        self.template[start:end] = block

    def get_logs(self, *args, **kwds) -> list[Path]:
        """Gets the logs from openQA.

        Args:
            *args: Additional arguments (not used).
            **kwds: Additional keyword arguments (not used).

        Returns:
            A list of paths to the log files.

        """
        filepath = self.config.template_dir / str(self.rrid) / self.config.install_logs
        auto = self.openqa.auto
        if auto is None:
            return []
        ilogs = zip_longest(
            auto.results,
            map(self._openqa_installog_to_template, auto.results),
        )
        filenames = []
        for i, y in ilogs:
            fn = f"{i.distri.lower()}_{i.version}_{i.arch}.log"
            if y:
                self._writer(filepath.joinpath(fn), y)
                filenames.append(Path(fn))

        return filenames

    @staticmethod
    def _openqa_installog_to_template(url) -> list[str]:
        """Converts an openQA install log to a template.

        Args:
            url: The URL of the log to convert.

        Returns:
            A list of strings representing the log content.

        """
        # input is URLs instance
        try:
            with urlopen(url.url) as log:
                t = log.readlines()
            return [x.decode() for x in t]
        except (RemoteDisconnected, HTTPError):
            logger.error("log %s failed to download", url.url)
            return []
        except URLError:
            try:
                with urlopen(url.url, context=no_verify) as log:
                    t = log.readlines()
                return [x.decode() for x in t]
            except (RemoteDisconnected, HTTPError, URLError):
                logger.error("log %s failed to download", url.url)
                return []

    def run(self, *args, **kwds) -> FileList | list[str]:
        """Runs the exporter.

        Args:
            *args: Additional arguments (not used).
            **kwds: Additional keyword arguments (not used).

        Returns:
            The exported template.

        """
        install_logs_current = (
            "Installation tests done in openQA with following results:" in self.template
            and not self.force
        )
        self.install_results()
        self.inject_openqa()
        self.inject_overview()
        if not install_logs_current:
            filenames = self.get_logs()
            self.installlogs_lines(filenames)
        self.add_sysinfo()
        self.dedup_lines()
        return self.template
