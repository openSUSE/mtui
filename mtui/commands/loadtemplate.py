"""The `load_template` command."""

from typing import ClassVar

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..support.messages import TestReportNotLoadedError
from ..types.updateid import AutoOBSUpdateID, KernelOBSUpdateID
from . import Command


class LoadTemplate(Command):
    """Loads a QA Maintenance template by its RRID identifier.

    All changes and logs from an already loaded template are lost if
    not saved previously.

    Each loaded template owns its own reference hosts. Loading a template
    never reconnects hosts that belong to another loaded template; the new
    template connects only the reference hosts defined in its own template
    file (or selected from its testplatforms).
    """

    command = "load_template"
    # Names its own target via -a/-k, so it must run exactly once. Without this
    # an unscoped call under MCP (where "active" fans out with several templates
    # loaded) would re-run the load — and its host autoconnect — once per already
    # loaded template, needlessly grabbing pool hosts. Mirrors ``unload``.
    scope: ClassVar[str] = "single"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        group = parser.add_mutually_exclusive_group(required=True)
        group.add_argument(
            "-a",
            "--auto-review-id",
            metavar="RequestReviewID",
            type=AutoOBSUpdateID,
            help="OBS request review id\nexample: SUSE:Maintenance:1:1",
            dest="update",
        )
        group.add_argument(
            "-k",
            "--kernel-review-id",
            metavar="RequestReviewID",
            type=KernelOBSUpdateID,
            help="OBS kernel/live-patch request review id\nexample: SUSE:Maintenance:1:1",
            dest="update",
        )

    def __call__(self):
        """Executes the `load_template` command.

        The template is *added* to the registry rather than replacing the
        current session. Loading an RRID that is already loaded replaces its
        stored report (``TemplateRegistry.add`` is keyed by RRID) and makes it
        active; previously loaded templates are left untouched.

        The newly loaded template connects only its own reference hosts. Hosts
        belonging to other loaded templates are left alone -- each template
        owns its own hosts and pool claims.
        """
        if self.args.update.kind not in ("kernel", "auto"):
            raise TestReportNotLoadedError

        # Workflow mode (auto/kernel) is seeded onto the TestReport by
        # make_testreport when load_update runs below.
        self.prompt.load_update(self.args.update, autoconnect=True)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [
                ("SUSE:Maintenance:", "openSUSE:Maintenance:"),
                ("-a", "--auto-review-id"),
                ("-k", "--kernel-review-id"),
            ],
            line,
            text,
        )
