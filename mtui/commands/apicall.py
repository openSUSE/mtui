"""Commands for interacting with backend APIs (OSC and Gitea).

This module defines a set of commands for interacting with backend APIs
like OSC and Gitea. It uses a base class `BaseApiCall` to handle the
dispatch logic between the two backends, and then provides concrete
implementations for `approve`, `assign`, `unassign`, `reject`, and
`comment`.
"""

from abc import ABC, abstractmethod
from argparse import REMAINDER
from logging import getLogger
from typing import final

from ..argparse import ArgumentParser
from ..commands import Command
from ..connector import OSC, Gitea
from ..exceptions import GiteaError, InvalidGiteaHash
from ..utils import complete_choices, prompt_user, requires_update

logger = getLogger("mtui.command.apicalls")


class BaseApiCall(Command, ABC):
    """An abstract base class for commands that interact with backend APIs."""

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds common arguments to the command's argument parser."""
        parser.add_argument(
            "-g",
            "--group",
            nargs="?",
            action="append",
            help=f"Group wanted to {cls.command}\n Not valid for Gitea Workflow",
        )
        parser.add_argument(
            "-u",
            "--user",
            action="store",
            default="",
            help="User override for gitea workflow (Gitea only)",
        )

    @property
    def _is_gitea_workflow(self) -> bool:
        """Determines if the request should be handled by the Gitea API."""
        rrid = self.metadata.rrid
        return rrid.kind == "SLFO" and rrid.maintenance_id != "1.1"

    @requires_update
    def __call__(self) -> None:
        """The main entry point for the command."""
        if self._is_gitea_workflow:
            self.gitea()
        else:
            self.osc()

    @abstractmethod
    def osc(self) -> None:
        """Provides OSC-specific logic."""
        raise NotImplementedError

    @abstractmethod
    def gitea(self) -> None:
        """Provides Gitea-specific logic."""
        raise NotImplementedError

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices([("-g", "--group"), ("-u", "--user")], line, text)


@final
class Approve(BaseApiCall):
    """A command to approve a review request."""

    command = "approve"

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
            if self.metadata.check_hash() != self.metadata.giteacohash:
                logger.error(
                    "GiteaPR hash is different from testreport, plese reconsider approval"
                )
                if prompt_user(
                    "Do you really want approve this update ?",
                    ["Yes", "Y", "yes", "y", "Ja", "ja"],
                    self.prompt.interactive,
                ):
                    gitea.approve(self.args.user)
                else:
                    raise InvalidGiteaHash(self.metadata.id)
            else:
                gitea.approve(self.args.user)

        except GiteaError as e:
            logger.error(e)


@final
class Assign(BaseApiCall):
    """A command to assign a review request to a user or group."""

    command = "assign"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        super()._add_arguments(parser)
        parser.add_argument(
            "-f",
            "--force",
            action="store_true",
            help="Force assign review to user in Gitea PR, even there isn't open group",
        )

    def osc(self) -> None:
        """Assigns the request in OSC."""
        logger.info("Assign request %s", self.metadata.rrid.review_id)
        osc = OSC(self.config, self.metadata.rrid)
        osc.assign(self.args.group)

    def gitea(self) -> None:
        """Assigns the pull request in Gitea."""
        logger.info("Assign PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.assign(self.args.user, self.args.force)
        except GiteaError as e:
            logger.error(e)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-g", "--group"), ("-u", "--user"), ("-f", "--force")], line, text
        )


@final
class Unassign(BaseApiCall):
    """A command to unassign a review request."""

    command = "unassign"

    def osc(self) -> None:
        """Unassigns the request in OSC."""
        logger.info("Unassign request %s", self.metadata.id.review_id)
        osc = OSC(self.config, self.metadata.id)
        osc.unassign(self.args.group)

    def gitea(self) -> None:
        """Unassigns the pull request in Gitea."""
        logger.info("Unassign PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.unassign(self.args.user)
        except GiteaError as e:
            logger.error(e)


@final
class Reject(BaseApiCall):
    """A command to reject a review request."""

    command = "reject"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        super()._add_arguments(parser)
        parser.add_argument(
            "-r",
            "--reason",
            required=True,
            choices=[
                "admin",
                "retracted",
                "build_problem",
                "not_fixed",
                "regression",
                "false_reject",
                "tracking_issue",
            ],
            help="Reason to reject update, required",
        )
        parser.add_argument(
            "-m",
            "--message",
            nargs=REMAINDER,
            help="Message to use for rejection-comment."
            + "Always as last of command, it takes remainder of command",
        )

    def osc(self) -> None:
        """Rejects the request in OSC."""
        logger.info("Reject request %s", self.metadata.rrid.review_id)
        osc = OSC(self.config, self.metadata.rrid)
        osc.reject(self.args.group, self.args.reason, self.args.message)

    def gitea(self) -> None:
        """Rejects the pull request in Gitea."""
        logger.info("Reject PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.reject(self.args.reason, self.args.user, self.args.message)
        except GiteaError as e:
            logger.error(e)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [
                ("-g", "--group"),
                ("-r", "--reason"),
                ("-m", "--message"),
                ("-u", "--user"),
                (
                    "admin",
                    "retracted",
                    "build_problem",
                    "not_fixed",
                    "regression",
                    "false_reject",
                    "tracking_issue",
                ),
            ],
            line,
            text,
        )


@final
class Comment(BaseApiCall):
    """A command to add a comment to a review request."""

    command = "comment"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        pass

    def osc(self) -> None:
        """Adds a comment to the request in OSC."""
        comment = input("Comment: ")
        osc = OSC(self.config, self.metadata.rrid)
        osc.comment(comment)

    def gitea(self) -> None:
        """Adds a comment to the pull request in Gitea."""
        comment = input("Comment: ")
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.comment(comment)
        except GiteaError as e:
            logger.error(e)
