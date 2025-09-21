"""The `load_template` command."""

from mtui.argparse import ArgumentParser

from . import Command
from ..messages import TestReportNotLoadedError
from ..types.updateid import AutoOBSUpdateID, KernelOBSUpdateID
from ..utils import prompt_user
from ..utils import complete_choices


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
        """Executes the `load_template` command."""
        if self.metadata:
            msg = "Should i owerwrite already loaded session {}? (y/N) "
            if not prompt_user(
                msg.format(self.metadata.id),
                ["y", "Y", "yes", "YES", "Yes"],
                self.prompt.interactive,
            ):
                return

        re_add = list(self.targets.keys())
        for target in re_add:
            self.targets[target].close()
            del self.targets[target]

        if self.args.update.kind == "kernel":
            self.config.kernel = True
            self.config.auto = False
        elif self.args.update.kind == "auto":
            self.config.kernel = False
            self.config.auto = True
        else:
            raise TestReportNotLoadedError

        self.prompt.load_update(self.args.update, autoconnect=True)

        # Reload hosts to which we already have a connection
        # close hosts we are already connected to but add them to the
        # testreport.systems so they get connected to again.
        # This feature comes from pre-1.0 versions.
        # NOTE: the only reason we need to reconnect seems to be that
        # when the L{Target} object is created, it is passed a list of
        # packages, which changes with the testreport change. So this
        # may go away when refactored.

        if self.args.chosts:
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
