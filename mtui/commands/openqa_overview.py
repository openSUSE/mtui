"""The ``openqa_overview`` interactive command.

Port of the upstream `oqa-search`_ helper. After a test report is
loaded (``load_template <RRID>``) this prints three sections suitable
for pasting into an MU log:

* Single Incidents - Core: PASSED/FAILED/RUNNING per SLE version
* Aggregated Updates: most recent build per group covering the incident
* Build checks: parsed test-result summaries from qam.suse.de logs

URLs default to mtui config (``openqa_instance``, ``qem_dashboard_api``,
``reports_url``) and can be overridden per invocation.

.. _oqa-search: https://github.com/mjdonis/oqa-search
"""

from __future__ import annotations

import re
from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.colors import blue, green, red, yellow
from ..cli.completion import complete_choices
from ..data_sources import oqa_search as oqa
from ..support.http import resolve_verify
from ..support.misc import requires_update
from ..types import FileList, OpenQAOverviewResult
from ..update_workflow.export.overview_inject import inject_overview
from . import Command

logger = getLogger("mtui.commands.openqa_overview")

# OBS prepends each build-log line with `[ <seconds>s] ` (right-padded
# to a fixed width). Strip it from the printed output -- it's useful
# context when debugging slow builds but noise in an MU log paste. The
# stored payload on `metadata.openqa.overview` keeps the raw lines.
_OBS_TIMESTAMP_RE = re.compile(r"^\[\s*\d+s\]\s*")

# Aggregated-update job groups upstream offers. Static (not fetched from
# openQA) so REPL tab completion works offline; the connector validates
# against the live list at call time.
_AGGREGATED_GROUP_CHOICES: tuple[str, ...] = ("core", "containers", "yast", "security")

# Flag synonyms used both by the parser and by tab completion.
_FLAGS: tuple[tuple[str, ...], ...] = (
    ("--no-aggregated",),
    ("--days",),
    ("--aggregated-groups",),
    ("--url-openqa",),
    ("--url-dashboard-qam",),
    ("--url-qam",),
    ("--test-pattern",),
    ("--export",),
    ("--no-fetch",),
)


class OpenQAOverview(Command):
    """Print an openQA / Dashboard / build-checks overview for the loaded MU.

    Mirrors the upstream oqa-search command-line tool. Sourced from the
    currently loaded testreport (``load_template`` must have run first).
    """

    command = "openqa_overview"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        parser.add_argument(
            "--no-aggregated",
            action="store_true",
            help="Skip the Aggregated Updates section",
        )
        parser.add_argument(
            "--days",
            type=int,
            default=5,
            choices=range(1, 31),
            metavar="N",
            help="How many days to scan back for aggregated builds (1-30, default 5)",
        )
        parser.add_argument(
            "--aggregated-groups",
            type=str,
            default=["core"],
            choices=_AGGREGATED_GROUP_CHOICES,
            nargs="+",
            metavar="GROUP",
            help=(
                "Job groups to search inside the Aggregated Updates section "
                "(default: core)"
            ),
        )
        parser.add_argument(
            "--url-openqa",
            type=str,
            default=None,
            help="Override openQA URL (default: config openqa_instance)",
        )
        parser.add_argument(
            "--url-dashboard-qam",
            type=str,
            default=None,
            help="Override QAM Dashboard base URL (default: derived from config)",
        )
        parser.add_argument(
            "--url-qam",
            type=str,
            default=None,
            help="Override QAM base URL (default: derived from config reports_url)",
        )
        parser.add_argument(
            "--test-pattern",
            type=str,
            default=None,
            help="Custom regex to extract test results from build-check logs",
        )
        parser.add_argument(
            "--export",
            action="store_true",
            help=(
                "Also inject the overview into the loaded testreport's `log` "
                "file under the `regression tests:` section. Idempotent: a "
                "previously-inserted block is replaced in place"
            ),
        )
        parser.add_argument(
            "--no-fetch",
            action="store_true",
            help=(
                "Skip the network search and reuse the cached overview "
                "(only meaningful with --export). No-op if nothing is cached"
            ),
        )

    @requires_update
    def __call__(self) -> None:
        """Run the overview and print formatted sections to the REPL."""
        # --no-fetch reuses the cached payload (skip the network search).
        if self.args.no_fetch:
            cached = self.metadata.openqa.overview
            if cached is None:
                logger.warning(
                    "--no-fetch given but no cached overview is present; "
                    "fetching anyway"
                )
            else:
                self._render_to_repl(cached)
                if self.args.export:
                    self._export_to_testreport(cached)
                return

        overview = self._fetch_overview()

        # Store the structured payload so exporters / later commands can
        # use it without re-fetching.
        self.metadata.openqa.overview = overview

        if self.args.export:
            self._export_to_testreport(overview)

    def _fetch_overview(self) -> OpenQAOverviewResult:
        """Run the network search and return a populated overview.

        Also prints colourised output to the REPL as the data arrives.
        """
        rrid = self.metadata.rrid

        # Pin the global TLS verify policy on the search session before any
        # network call so the openQA / Dashboard search honors
        # ``[mtui] ssl_verify`` (defaulting to verify).
        oqa.set_verify(resolve_verify(True, self.config.ssl_verify))

        url_openqa = self.args.url_openqa or self.config.openqa_instance
        url_dashboard_qam = self.args.url_dashboard_qam or self._derive_dashboard_url(
            self.config.qem_dashboard_api
        )
        url_qam = self.args.url_qam or self._derive_qam_url(self.config.reports_url)

        logger.debug(
            "openQA: %s, Dashboard: %s, QAM: %s",
            url_openqa,
            url_dashboard_qam,
            url_qam,
        )

        # incident_id is rrid.maintenance_id (int for Maintenance, "1.2" for SLFO);
        # request_id is rrid.review_id; product is rrid.kind.value.
        incident_id = rrid.maintenance_id
        request_id = rrid.review_id
        product = rrid.kind.value
        effective_incident_id = (
            incident_id if isinstance(incident_id, int) else request_id
        )

        overview = OpenQAOverviewResult(skip_aggregated=self.args.no_aggregated)

        self.println(self._title("OpenQA:"))
        self.println(self._title("#######"))

        try:
            build, versions = oqa.get_incident_info(
                url_dashboard_qam, effective_incident_id
            )
        except oqa._HTTPError as e:  # noqa: SLF001 -- module-internal exception
            logger.error("Failed to query QEM Dashboard: %s", e)
            return overview

        if versions:
            overview.single_incidents = oqa.single_incidents(
                build, versions, url_openqa
            )
            self.println(self._title("Single incidents - Core"))
            for row in overview.single_incidents:
                self._print_version_row(row)

            if not self.args.no_aggregated:
                self.println("-------")
                overview.aggregated_updates = oqa.aggregated_updates(
                    effective_incident_id,
                    versions,
                    self.args.days,
                    self.args.aggregated_groups,
                    url_openqa,
                )
                for group in overview.aggregated_updates:
                    self.println(
                        self._title(f"\nAggregated updates - {group.group.title()}")
                    )
                    for row in group.versions:
                        self._print_version_row(row)
                if not overview.aggregated_updates:
                    self.println(
                        yellow(
                            "No aggregated updates builds available for this incident"
                        )
                    )
        else:
            self.println(yellow("No openQA builds for this incident yet"))

        self.println("-------")
        self.println(self._title("\nBuild checks:"))
        self.println(self._title("#############"))
        packages = self.metadata.get_package_list() if self.metadata else []
        if not packages and build:
            packages = [build.split(":")[-1]]
        overview.build_checks = oqa.build_checks(
            product,
            incident_id,
            request_id,
            packages,
            url_qam,
            self.args.test_pattern,
        )
        if overview.build_checks:
            for entry in overview.build_checks:
                self._print_build_check(entry)
        else:
            self.println("No build checks for this incident")

        return overview

    def _render_to_repl(self, overview: OpenQAOverviewResult) -> None:
        """Print a cached overview to the REPL without re-fetching."""
        self.println(self._title("OpenQA:"))
        self.println(self._title("#######"))
        if overview.single_incidents:
            self.println(self._title("Single incidents - Core"))
            for row in overview.single_incidents:
                self._print_version_row(row)
        else:
            self.println(yellow("No openQA builds for this incident yet"))

        if overview.aggregated_updates:
            self.println("-------")
            for group in overview.aggregated_updates:
                self.println(
                    self._title(f"\nAggregated updates - {group.group.title()}")
                )
                for row in group.versions:
                    self._print_version_row(row)

        self.println("-------")
        self.println(self._title("\nBuild checks:"))
        self.println(self._title("#############"))
        if overview.build_checks:
            for entry in overview.build_checks:
                self._print_build_check(entry)
        else:
            self.println("No build checks for this incident")

    def _export_to_testreport(self, overview: OpenQAOverviewResult) -> None:
        """Append (or replace) the overview block in the testreport `log`."""
        log_path = self.metadata.path
        if log_path is None:
            logger.error("No testreport path available; cannot export")
            return

        with FileList.load(log_path) as text:
            modified = inject_overview(
                text,
                overview.single_incidents,
                overview.aggregated_updates,
                overview.build_checks,
                skip_aggregated=self.args.no_aggregated,
            )

        if modified:
            logger.info("openqa_overview block written to %s", log_path)
        else:
            logger.warning(
                "Could not locate 'regression tests:' section in %s; "
                "overview NOT exported",
                log_path,
            )

    # --- formatting helpers ---

    @staticmethod
    def _derive_dashboard_url(qem_dashboard_api: str) -> str:
        """Drop a trailing ``/api`` so we have the dashboard base URL.

        ``config.qem_dashboard_api`` is the API base
        (``http://dashboard.qam.suse.de/api``); the connector adds
        ``/api/...`` itself.
        """
        return qem_dashboard_api.rstrip("/").removesuffix("/api")

    @staticmethod
    def _derive_qam_url(reports_url: str) -> str:
        """Drop a trailing ``/testreports`` to recover the QAM base URL.

        ``config.reports_url`` defaults to ``https://qam.suse.de/testreports``;
        ``build_checks`` re-appends ``/testreports/...``.
        """
        return reports_url.rstrip("/").removesuffix("/testreports")

    @staticmethod
    def _title(text: str) -> str:
        """Color section titles blue for the REPL.

        Output is REPL-only (``_export_to_testreport`` builds its own
        plain-text via ``render_overview``), so coloring here is pure
        UX. ``blue()`` honours ``colors_enabled()``, so piping mtui's
        stdout to a file still produces plain text.
        """
        return blue(text)

    def _print_version_row(self, row: oqa.VersionResult) -> None:
        """Render one PASSED/FAILED/RUNNING/MISSING line."""
        if row.status == "missing":
            self.println(yellow(f"{row.version} -> {row.note}"))
            return

        if row.url:
            self.println(f"{row.version} -> {row.url}")
        else:
            self.println(f"{row.version}")

        if row.status == "failed":
            label = (
                f"FAILED ({row.failed_count} jobs)" if row.failed_count else "FAILED"
            )
            self.println(red(label))
        elif row.status == "running":
            label = (
                f"RUNNING/SCHEDULED ({row.running_count} jobs)"
                if row.running_count
                else "RUNNING/SCHEDULED"
            )
            self.println(yellow(label))
        else:
            self.println(green("PASSED"))

        if row.note:
            self.println(yellow(row.note))

    def _print_build_check(self, entry: oqa.BuildCheckResult) -> None:
        """Render one build-check log entry."""
        self.println(entry.url)
        if not entry.matches:
            self.println(
                "No test results found (try using a custom pattern with --test-pattern)"
            )
            self.println("")
            return

        if entry.summary:
            # Folded form: first match, summary, last match.
            self.println(self._strip_obs_timestamp(entry.matches[0]))
            self.println(entry.summary)
            self.println(self._strip_obs_timestamp(entry.matches[-1]))
        else:
            for line in entry.matches:
                self.println(self._strip_obs_timestamp(line))
        self.println("")

    @staticmethod
    def _strip_obs_timestamp(line: str) -> str:
        """Remove the OBS ``[  <seconds>s]`` prefix from a build-log line."""
        return _OBS_TIMESTAMP_RE.sub("", line)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Tab-complete the command's flags."""
        return complete_choices(_FLAGS, line, text)
