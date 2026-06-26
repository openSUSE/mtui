"""The ``regenerate`` interactive command.

Regenerates the loaded update's test-report template via the TeReGen API
(``POST /reports/{id}/regenerate``), then waits for the Minion generation job to
finish and reloads the freshly generated template into the session.

This is the agent-/operator-facing counterpart to the regenerate offer made by
the loader when a checked-out template's hash no longer matches its Gitea PR
(see :mod:`mtui.types.updateid`). Use it to pick up a regenerated template after
an arch/metadata fix without leaving and re-entering mtui.
"""

from __future__ import annotations

import shutil
from logging import getLogger

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices, template_completion
from ..data_sources import TeReGen
from ..support.misc import requires_update
from ..support.spinner import spinner
from ..types.enums import Workflow
from ..types.updateid import AutoOBSUpdateID, KernelOBSUpdateID
from . import Command

logger = getLogger("mtui.commands.regenerate")


class Regenerate(Command):
    """Regenerate the loaded update's template via TeReGen and reload it."""

    command = "regenerate"
    scope = "fanout"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Register the command's arguments."""
        parser.add_argument(
            "--force",
            action="store_true",
            dest="force",
            help="overwrite an existing (but unedited) template",
        )
        parser.add_argument(
            "--ignore-inconsistent",
            action="store_true",
            dest="ignore_inconsistent",
            help="regenerate despite inconsistent metadata (e.g. arch mismatch)",
        )
        parser.add_argument(
            "--no-wait",
            action="store_true",
            dest="no_wait",
            help="enqueue the job and return without waiting or reloading",
        )
        cls._add_template_arg(parser)

    @requires_update
    def __call__(self) -> None:
        """Trigger regeneration, wait for completion, and reload the template."""
        teregen = TeReGen(self.config)
        rrid = self.metadata.rrid

        # --no-wait: enqueue only, so we don't need the shared wait path.
        if self.args.no_wait:
            result = teregen.regenerate(
                rrid,
                force_overwrite=self.args.force,
                ignore_inconsistent=self.args.ignore_inconsistent,
            )
            if not self._report_enqueue(rrid, result):
                return
            self.println(
                "Not waiting (--no-wait); reload the template once it is built."
            )
            return

        # Interactive: a TTY spinner shows the wait is alive and yields a stop
        # predicate so Ctrl-C abandons the wait promptly. Over MCP it is a no-op
        # (the server heartbeats progress, and `regenerate` can be backgrounded).
        self.println(f"Regenerating {rrid} ...")
        with spinner(f"Regenerating {rrid}") as is_stopped:
            outcome = teregen.regenerate_and_wait(
                rrid,
                force_overwrite=self.args.force,
                ignore_inconsistent=self.args.ignore_inconsistent,
                should_stop=is_stopped,
            )

        if outcome.unreachable:
            self.println(
                f"Regeneration request for {rrid} failed (TeReGen unreachable)"
            )
            return
        if outcome.error:
            self.println(f"Regeneration refused: {outcome.error}")
            self._println_retry_hint()
            return

        self.println(f"Regeneration job {outcome.job} enqueued for {rrid}")
        if not outcome.ok:
            self.println(
                f"Regeneration did not finish (state={outcome.state or 'unknown'})"
                + (f": {outcome.minion_error}" if outcome.minion_error else "")
            )
            return

        self.println(f"Template for {rrid} regenerated — reloading")
        self._reload(rrid)

    def _report_enqueue(self, rrid: object, result: dict | None) -> bool:
        """Print the enqueue outcome for the --no-wait path; return True on success."""
        if result is None:
            self.println(
                f"Regeneration request for {rrid} failed (TeReGen unreachable)"
            )
            return False
        if result.get("error"):
            self.println(f"Regeneration refused: {result['error']}")
            self._println_retry_hint()
            return False
        self.println(f"Regeneration job {result.get('job')} enqueued for {rrid}")
        return True

    def _println_retry_hint(self) -> None:
        """Suggest the flags that might lift a refusal, skipping ones already set."""
        flags = [
            flag
            for flag, already in (
                ("--force", self.args.force),
                ("--ignore-inconsistent", self.args.ignore_inconsistent),
            )
            if not already
        ]
        if flags:
            self.println(f"Retry with {' and/or '.join(flags)} if appropriate.")

    def _reload(self, rrid: object) -> None:
        """Drop the stale local checkout and load the freshly built template."""
        trdir = self.config.template_dir / str(rrid)
        if trdir.exists():
            shutil.rmtree(trdir, ignore_errors=True)
            logger.info("Removed stale checked out template %s", trdir)

        factory = (
            KernelOBSUpdateID
            if self.metadata.workflow is Workflow.KERNEL
            else AutoOBSUpdateID
        )
        self.prompt.load_update(factory(str(rrid)), autoconnect=False)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [
                ("--force",),
                ("--ignore-inconsistent",),
                ("--no-wait",),
                *template_completion(state),
            ],
            line,
            text,
        )
