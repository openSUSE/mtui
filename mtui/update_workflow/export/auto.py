"""An exporter for the automatic workflow."""

from itertools import zip_longest
from logging import getLogger
from pathlib import Path

import requests

from ...support.http import HTTP_TIMEOUT, build_session, resolve_verify
from ...types import FileList
from .base import BaseExport

logger = getLogger("mtui.export.auto")


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

    def _openqa_installog_to_template(self, url) -> list[str]:
        """Download an openQA install log and return its lines.

        Args:
            url: A ``URLs`` instance whose ``url`` attribute points at the
                install log to fetch.

        Returns:
            The log content as a list of lines (with trailing newlines),
            or an empty list if the download failed.

        TLS verification follows the global ``[mtui] ssl_verify`` policy.
        When that policy is unset this preserves the historical
        behaviour of trying verified first and falling back to an
        unverified retry on a certificate error -- most openQA hosts use
        an internal CA the system trust store may not know about. When
        ``ssl_verify`` is set explicitly the user's choice is honoured
        with no insecure fallback.

        """
        verify = resolve_verify(True, self.config.ssl_verify)
        try:
            response = build_session(verify).get(url.url, timeout=HTTP_TIMEOUT)
            response.raise_for_status()
            return response.text.splitlines(keepends=True)
        except requests.exceptions.SSLError:
            if self.config.ssl_verify is not None:
                # User explicitly chose a verification policy; do not
                # silently downgrade to an unverified connection.
                logger.error("log %s failed to download", url.url)
                return []
            logger.debug(
                "verified fetch of %s failed on TLS; retrying unverified", url.url
            )
            try:
                response = build_session(verify=False).get(
                    url.url, timeout=HTTP_TIMEOUT
                )
                response.raise_for_status()
                return response.text.splitlines(keepends=True)
            except requests.exceptions.RequestException:
                logger.error("log %s failed to download", url.url)
                return []
        except requests.exceptions.RequestException:
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
