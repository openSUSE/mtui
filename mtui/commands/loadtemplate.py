"""The `load_template` command."""

from ..cli.argparse import ArgumentParser
from ..cli.completion import complete_choices
from ..support.messages import TestReportNotLoadedError
from ..types.updateid import AutoOBSUpdateID, KernelOBSUpdateID
from . import Command


class LoadTemplate(Command):
    """Loads a QA Maintenance template by its RRID identifier.

    All changes and logs from an already loaded template are lost if
    not saved previously.

    Already connected hosts are kept and extended by the reference
    hosts defined in the template file. This behavior can be changed
    with the -c/--clean-hosts parameter.
    """

    command = "load_template"

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
        parser.add_argument(
            "-c",
            "--clean-hosts",
            dest="chosts",
            action="store_false",
            help="clean up old hosts",
        )

    def __call__(self):
        """Executes the `load_template` command.

        The template is *added* to the registry rather than replacing the
        current session. Loading an RRID that is already loaded replaces its
        stored report (``TemplateRegistry.add`` is keyed by RRID) and makes it
        active; previously loaded templates are left untouched.
        """
        if self.args.update.kind not in ("kernel", "auto"):
            raise TestReportNotLoadedError

        # Snapshot the currently active template's hosts *before* loading so
        # the -c/--clean-hosts carry-over can reconnect them to the new
        # template. We do not close the old template's connections any more:
        # the old report stays in the registry with its own hosts.
        re_add = list(self.targets.keys()) if self.args.chosts else []

        # Workflow mode (auto/kernel) is seeded onto the TestReport by
        # make_testreport when load_update runs below.
        self.prompt.load_update(self.args.update, autoconnect=True)

        # Reconnect the hosts we were already connected to, adding them to the
        # freshly loaded testreport so they get connected to again.
        # This feature comes from pre-1.0 versions.
        # NOTE: the only reason we need to reconnect seems to be that
        # when the L{Target} object is created, it is passed a list of
        # packages, which changes with the testreport change. So this
        # may go away when refactored.
        for target in re_add:
            self.prompt.metadata.add_target(target)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [
                ("-c", "--clean-hosts"),
                ("SUSE:Maintenance:", "openSUSE:Maintenance:"),
                ("-a", "--auto-review-id"),
                ("-k", "--kernel-review-id"),
            ],
            line,
            text,
        )
