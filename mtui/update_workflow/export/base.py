"""The base class for all exporters in mtui."""

from abc import ABC, abstractmethod
from logging import getLogger
from pathlib import Path

from ...cli.term import prompt_user
from ...support.fileops import timestamp
from ...support.systemcheck import system_info
from ...types import FileList, OpenQAResults

logger = getLogger("mtui.export.base")


class BaseExport(ABC):
    """The base class for all exporters in mtui.

    This class provides common functionality for writing to files,
    deduplicating lines, adding system information, and injecting
    openQA results.
    """

    def __init__(
        self,
        config,
        openqa,
        template: FileList,
        force: bool,
        rrid,
        interactive,
        **kwargs,
    ) -> None:
        """Initializes the exporter.

        Args:
            config: The application configuration.
            openqa: The openQA connector instance.
            template: The template to export to.
            force: Whether to force overwriting existing files.
            rrid: The RequestReviewID of the current update.
            interactive: Whether to run in interactive mode.
            **kwargs: Additional keyword arguments.

        """
        self.config = config
        self.openqa: OpenQAResults = openqa
        self.template: FileList | list[str] = template[:]
        self.force = force
        self.rrid = rrid
        self.interactive = interactive
        for key in kwargs:
            self.__setattr__(key, kwargs[key])

    def _writer(self, fn: Path, data) -> None:
        """Writes data to a file.

        Args:
            fn: The path to the file to write to.
            data: The data to write.

        """
        to_write = "\n".join(data)
        if fn.exists() and not self.force:
            if to_write == fn.read_text():
                logger.info("Log %s exists and is same as export", fn)
                return
            logger.warning("file %s exists.", fn)
            if not prompt_user(
                f"Should I overwrite {fn} (y/N) ",
                ["y", "Y", "yes", "Yes", "YES"],
                self.interactive,
            ):
                fn = fn.with_suffix("." + timestamp())

        logger.info("exporting log to %s", fn)

        try:
            with fn.open(mode="w", encoding="utf-8") as f:
                f.write(to_write)
        except OSError:
            logger.error("Failed to write %s", fn)

    def installlogs_lines(self, filenames) -> None:
        """Adds install log links to the template.

        Args:
            filenames: A list of filenames to add.

        """
        # Index just past the HAS_UNTRACKED marker; the install-log links are
        # deduplicated against the template from there on. The ``o += 1`` used
        # to sit outside the loop, so ``o`` was always 1 and the marker search
        # was dead. If the marker is absent, scan the whole template so dups are
        # still caught.
        o = 0
        for i, line in enumerate(self.template):
            if "HAS_UNTRACKED" in line:
                o = i + 1
                break

        marker = "Links for update logs:\n"
        try:
            # Reuse an existing section: manual/kernel exports run this on
            # every export, and unconditionally inserting a fresh header
            # stacked empty 'Links for update logs:' sections that grew
            # with each re-export (the links themselves were de-duplicated,
            # the header was not). New links are appended after the
            # section's existing links.
            index = self.template.index(marker) + 1
            self._drop_empty_link_sections(marker, index)
            if index >= len(self.template) or (
                self.template[index] != "\n"
                and str(self.config.reports_url) not in self.template[index]
            ):
                # Hand-trimmed template: the header is followed directly by
                # foreign content (e.g. the export footer). Restore the
                # canonical blank so the links land inside the section, not
                # after the footer.
                self.template.insert(index, "\n")
            while (
                index + 1 < len(self.template)
                and str(self.config.reports_url) in self.template[index + 1]
            ):
                index += 1
        except ValueError:
            index = len(self.template)
            if "## export MTUI:" in self.template[-1]:
                index -= 1
            self.template.insert(index, "\n")
            self.template.insert(index + 1, marker)
            self.template.insert(index + 2, "\n")
            index += 2

        add_empty_line = False
        for fn in filenames:
            install_log = f"{self.config.reports_url!s}/{self.rrid!s}/{self.config.install_logs!s}/{fn!s}\n"
            if install_log not in self.template[o:]:
                index += 1
                self.template.insert(index, install_log)
                add_empty_line = True

        if add_empty_line and (
            index + 1 >= len(self.template) or self.template[index + 1] != "\n"
        ):
            self.template.insert(index + 1, "\n")

    def _drop_empty_link_sections(self, marker: str, search_from: int) -> None:
        """Remove empty duplicate 'Links for update logs:' headers.

        Pre-fix exports stacked one fresh header per run while the links
        stayed under the original section, so damaged templates carry
        trailing header blocks with nothing but blanks under them. They
        would otherwise survive forever (``dedup_lines`` never collapses
        blank-separated duplicates). A duplicate section that does hold
        links is left alone -- never delete content.

        Args:
            marker: The section header line.
            search_from: Index to start scanning at (just past the first,
                kept, header).

        """
        i = search_from
        while True:
            try:
                j = self.template.index(marker, i)
            except ValueError:
                return
            k = j + 1
            while k < len(self.template) and self.template[k] == "\n":
                k += 1
            if (
                k < len(self.template)
                and str(self.config.reports_url) in (self.template[k])
            ):
                i = k  # a section with real links: keep it
                continue
            # Empty duplicate: drop the header, its trailing blanks, and the
            # framing blank the old code inserted before it.
            start = j - 1 if j > 0 and self.template[j - 1] == "\n" else j
            del self.template[start:k]
            i = start

    def dedup_lines(self) -> None:
        """Deduplicates lines in the template."""
        pr_line = None
        lines = []
        for c_line in self.template:
            if pr_line == c_line and c_line != "\n":
                pass
            else:
                lines.append(c_line)
            pr_line = c_line

        self.template = lines

    def add_sysinfo(self) -> None:
        """Adds system information to the template."""
        system_information = system_info(
            self.config.distro,
            self.config.distro_ver,
            self.config.distro_kernel,
            self.config.session_user,
        )
        if system_information != self.template[-1].rstrip():
            self.template.append(system_information)

    @abstractmethod
    def get_logs(self, *args, **kwds) -> list[Path]:
        """An abstract method for getting logs."""

    @abstractmethod
    def run(self, *args, **kwds) -> FileList | list[str]:
        """An abstract method for running the exporter."""

    def inject_overview(self) -> None:
        """Inject the ``openqa_overview`` block into the template.

        No-op unless ``metadata.openqa.overview`` was populated by the
        ``openqa_overview`` command (typically with ``--export``).
        Idempotent via begin/end markers -- a previously-inserted block
        is replaced in place rather than duplicated.
        """
        overview = self.openqa.overview
        if not overview:
            return

        # Lazy import: avoids dragging the connector module into export
        # base's import path when nobody used the overview feature.
        from .overview_inject import inject_overview as _inject

        if _inject(
            self.template,
            overview.single_incidents,
            overview.aggregated_updates,
            overview.build_checks,
            skip_aggregated=overview.skip_aggregated,
        ):
            logger.info("Injected openqa_overview block into template")

    def inject_openqa(self) -> None:
        """Injects openQA results into the template."""
        if not self.openqa.auto:
            return

        openqa = self.openqa.auto.pp
        if not openqa:
            return

        # remove previous results
        # TODO: simplify in future +- 5m after release of MTUI12

        for title in (
            "Results from openQA jobs:\n",
            "Results from incidents openQA jobs:\n",
            "Results from openQA incidents jobs:\n",
        ):
            if title not in self.template:
                continue
            r_start = self.template.index(title)
            try:
                r_end = self.template.index("End of openQA Incidents results\n") + 1
            except ValueError:
                r_end = self.template.index("source code change review:\n", 0) - 1

            del self.template[r_start:r_end]
            break

        index = self.template.index("source code change review:\n", 0) - 1
        for line in reversed(openqa):
            self.template.insert(index, line)

        index = self.template.index("source code change review:\n", 0) - 1
        self.template.insert(index, "\n")
        self.template.insert(index + 1, "End of openQA Incidents results\n")
        self.template.insert(index + 2, "\n")

    def install_results(self) -> None:
        """Adds installation results to the template."""
        index = self.template.index("Test results by product-arch:\n", 0)
        line = "All installation tests done in openQA please see installlogs section\n"
        # Idempotent: on a kernel re-export the previous run's notice is
        # still there, separated from a fresh insert by the blank line, so
        # dedup_lines() never collapsed them and the notice multiplied with
        # every export. Copies stacked by pre-fix exports are dropped so a
        # damaged template converges back to a single notice.
        while self.template.count(line) > 1:
            extra = len(self.template) - 1 - self.template[::-1].index(line)
            del self.template[extra]
            if extra < len(self.template) and self.template[extra] == "\n":
                del self.template[extra]
        if line not in self.template:
            self.template.insert(index + 3, line)
            self.template.insert(index + 4, "\n")
