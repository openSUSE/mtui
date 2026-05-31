"""The `approve` command.

Approves the current update via OSC or Gitea, reusing the backend-dispatch
logic from :class:`mtui.commands.apicall.BaseApiCall`. When ``-r/--reviewer``
is given, the reviewer is recorded in the testreport and the template is
committed to SVN before the approval happens.
"""

import subprocess
from logging import getLogger
from typing import final

from ..argparse import ArgumentParser
from ..completion import complete_choices
from ..connector import OSC, Gitea
from ..support.exceptions import GiteaError, InvalidGiteaHashError
from ..support.misc import requires_update
from ..template import TemplateFormatError, svn_commit_testreport
from ..term import prompt_user
from .apicall import BaseApiCall

logger = getLogger("mtui.command.approve")


@final
class Approve(BaseApiCall):
    """A command to approve a review request."""

    command = "approve"
    _pi_action = "unlock"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        super()._add_arguments(parser)
        parser.add_argument(
            "-r",
            "--reviewer",
            action="store",
            default=None,
            help="Record reviewer in the testreport, commit it to SVN, "
            "then approve. Aborts the approval if either step fails.",
        )

    @requires_update
    def __call__(self) -> None:
        """The main entry point for the command.

        When ``-r/--reviewer`` is given, record the reviewer in the
        testreport and commit it to SVN before approving. If either step
        fails, the approval is aborted.
        """
        if self.args.reviewer is not None and not self._record_reviewer():
            return
        super().__call__()

    def _record_reviewer(self) -> bool:
        """Records the reviewer and commits the testreport to SVN.

        Returns:
            ``True`` if the reviewer was recorded and committed and the
            approval should proceed; ``False`` if it failed and the
            approval must be aborted.

        """
        name = self.args.reviewer.strip()
        if not name:
            logger.error("Reviewer must be a non-empty string; not approving")
            return False

        try:
            self.metadata.set_reviewer(name)
        except (TemplateFormatError, ValueError, OSError) as e:
            logger.error("Failed to record reviewer, not approving: %s", e)
            return False

        try:
            svn_commit_testreport(
                self.metadata.report_wd(),
                self.config.install_logs,
                ["-m", f"Add Test Plan Reviewer: {name}"],
            )
        except subprocess.CalledProcessError as e:
            logger.error("Failed to commit testreport to SVN, not approving: %s", e)
            return False

        return True

    def osc(self) -> None:
        """Approves the request in OSC."""
        logger.info("Approving request %s", self.metadata.rrid.review_id)
        osc = OSC(self.config, self.metadata.rrid)
        osc.approve(self.args.group)

    def gitea(self) -> None:
        """Approves the pull request in Gitea."""
        logger.info("Approving PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            result, old_hash, new_hash = self.metadata.check_hash()
            if not result:
                logger.error(
                    "GiteaPR hash is different from testreport, please reconsider approval\n Testreport %s ->repo %s",
                    old_hash,
                    new_hash,
                )
                if prompt_user(
                    "Do you really want approve this update ?",
                    ["Yes", "Y", "yes", "y", "Ja", "ja"],
                    self.prompt.interactive,
                ):
                    gitea.approve(self.args.user)
                else:
                    raise InvalidGiteaHashError(
                        self.metadata.id, self.metadata.giteacohash, new_hash
                    )
            else:
                gitea.approve(self.args.user)

        except GiteaError as e:
            logger.error(e)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-g", "--group"), ("-u", "--user"), ("-r", "--reviewer")], line, text
        )
