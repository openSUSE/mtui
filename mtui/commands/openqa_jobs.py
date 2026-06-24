"""The ``openqa_jobs`` interactive command.

Lists the individual openQA jobs for the loaded update's incident build, so a
tester (or an MCP client) can see *which* scenarios passed or failed — and judge
whether a failure relates to the package under test — rather than only the
per-version PASSED/FAILED/RUNNING summary that ``openqa_overview`` prints.

By default ``obsoleted`` jobs (superseded by a later retrigger) are dropped; only
the current run matters. Use ``--all`` to keep them, ``--failed`` to show only
non-passing jobs, and ``--arch`` to filter by architecture.
"""

from __future__ import annotations

from collections import Counter
from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.colors import green, red, yellow
from ..cli.completion import complete_choices, template_completion
from ..data_sources import oqa_search as oqa
from ..support.http import resolve_verify
from ..support.misc import requires_update
from . import Command

logger = getLogger("mtui.commands.openqa_jobs")

# openQA results that count as "not a failure" for the --failed filter.
_PASSING = frozenset({"passed", "softfailed"})
_NEUTRAL = frozenset({"obsoleted", "skipped"})


class OpenQAJobs(Command):
    """List the individual openQA jobs for the loaded update."""

    command = "openqa_jobs"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Register the command's arguments."""
        cls._add_template_arg(parser)
        parser.add_argument(
            "--all",
            action="store_true",
            help="include obsoleted (superseded) jobs",
        )
        parser.add_argument(
            "--failed",
            action="store_true",
            help="show only non-passing jobs (failed / parallel_failed / incomplete)",
        )
        parser.add_argument(
            "--arch",
            type=str,
            default=None,
            help="only jobs for this architecture",
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

    @requires_update
    def __call__(self) -> None:
        """Fetch and print the incident's openQA jobs."""
        rrid = self.metadata.rrid
        oqa.set_verify(resolve_verify(True, self.config.ssl_verify))

        url_openqa = self.args.url_openqa or self.config.openqa_instance
        url_dashboard_qam = self.args.url_dashboard_qam or (
            self.config.qem_dashboard_api.rstrip("/").removesuffix("/api")
        )

        # incident_id is an int for Maintenance, "1.2" for SLFO; fall back to the
        # review id (the gitea PR number) in the SLFO case -- mirrors openqa_overview.
        incident_id = rrid.maintenance_id
        effective_incident_id = (
            incident_id if isinstance(incident_id, int) else rrid.review_id
        )

        try:
            build, _ = oqa.get_incident_info(url_dashboard_qam, effective_incident_id)
        except oqa._HTTPError as e:  # noqa: SLF001 -- module-internal exception
            logger.error("Failed to query QEM Dashboard: %s", e)
            return

        jobs = oqa.incident_jobs(build, url_openqa, include_obsoleted=self.args.all)
        if self.args.arch:
            jobs = [j for j in jobs if j.arch == self.args.arch]
        if self.args.failed:
            jobs = [
                j for j in jobs if j.result not in _PASSING and j.result not in _NEUTRAL
            ]

        if not jobs:
            self.println(yellow(f"No openQA jobs for build {build!r}"))
            return

        counts = Counter(j.result for j in jobs)
        summary = ", ".join(f"{k}={v}" for k, v in sorted(counts.items()))
        self.println(f"openQA jobs for build {build} ({len(jobs)}): {summary}")
        self.println("")
        for j in jobs:
            if j.result in _PASSING:
                colour = green
            elif j.result in _NEUTRAL:
                colour = yellow
            else:
                colour = red
            self.println(
                f"  {colour(j.result.ljust(15))} {j.arch:<8} {j.test}  {j.url}"
            )

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [
                ("--all",),
                ("--failed",),
                ("--arch",),
                ("--url-openqa",),
                ("--url-dashboard-qam",),
                *template_completion(state),
            ],
            line,
            text,
        )
