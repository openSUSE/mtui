from abc import ABC, abstractmethod
from argparse import REMAINDER
from logging import getLogger
from typing import final

from ..argparse import ArgumentParser
from ..commands import Command
from ..connector import OSC, Gitea
from ..exceptions import GiteaError
from ..utils import complete_choices, requires_update

logger = getLogger("mtui.command.apicalls")


class _Base(Command, ABC):
    """
    An abstract base class for commands that interact with backend APIs (OSC or Gitea).

    This class provides the core dispatch logic to determine which backend to use
    for a given review request. Subclasses must implement the backend-specific
    methods `_osc` and `_gitea`.
    """

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        # These are common arguments, but their applicability depends on the backend.
        # Gitea workflow primarily uses the --user argument.
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
        """
        Determines if the request should be handled by the Gitea API.

        This encapsulates the business logic for routing. Requests of kind "SLFO"
        that are not on the "1.1" maintenance track are managed via Gitea pull requests
        instead of the standard OSC review process.
        """
        rrid = self.metadata.id
        return rrid.kind == "SLFO" and rrid.maintenance_id != "1.1"

    @requires_update
    def __call__(self) -> None:
        """
        Main entry point for the command. Acts as a router to the correct backend.

        The `@requires_update` decorator ensures that the necessary metadata
        is loaded before this command logic is executed.
        """
        if self._is_gitea_workflow:
            self.__gitea()
        else:
            self.__osc()

    @abstractmethod
    def __osc(self) -> None:
        # Must be implemented by subclasses to provide OSC-specific logic.
        # The OSC class handles its own exceptions internally, so no try/except
        # block is needed in the implementations.
        raise NotImplementedError

    @abstractmethod
    def __gitea(self) -> None:
        # Must be implemented by subclasses to provide Gitea-specific logic.
        # The Gitea class raises GiteaError as part of its control flow,
        # so implementations should handle this exception.
        raise NotImplementedError

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        # Provides shell command-line completion for common arguments.
        return complete_choices([("-g", "--group"), ("-u", "--user")], line, text)


@final
class Approve(_Base):
    """Command to approve a review request."""

    command = "approve"

    def __osc(self) -> None:
        logger.info("Approving request %s", self.metadata.id.review_id)
        osc = OSC(self.config, self.metadata.id)
        osc.approve(self.args.group)

    def __gitea(self) -> None:
        logger.info("Approving PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.approve(self.args.user)
        except GiteaError as e:
            # Gitea connector uses exceptions to signal API failures or
            # permission issues (e.g., user is not an assigned reviewer).
            logger.error(e)


@final
class Assign(_Base):
    """Command to assign a review request to a user or group."""

    command = "assign"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        # Inherit the common arguments from the base class before adding new ones.
        super()._add_arguments(parser)
        parser.add_argument(
            "-f",
            "--force",
            action="store_true",
            help="Force assign review to user in Gitea PR, even there isn't open group",
        )

    def __osc(self) -> None:
        logger.info("Assign request %s", self.metadata.id.review_id)
        osc = OSC(self.config, self.metadata.id)
        osc.assign(self.args.group)

    def __gitea(self) -> None:
        logger.info("Assign PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.assign(self.args.user, self.args.force)
        except GiteaError as e:
            logger.error(e)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        # Provides shell completion for this command's specific arguments.
        return complete_choices(
            [("-g", "--group"), ("-u", "--user"), ("-f", "--force")], line, text
        )


@final
class Unassign(_Base):
    """Command to unassign a review request."""

    command = "unassign"

    def __osc(self) -> None:
        logger.info("Unassign request %s", self.metadata.id.review_id)
        osc = OSC(self.config, self.metadata.id)
        osc.unassign(self.args.group)

    def __gitea(self) -> None:
        logger.info("Unassign PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.unassign(self.args.user)
        except GiteaError as e:
            logger.error(e)


@final
class Reject(_Base):
    """Command to reject a review request with a reason and message."""

    command = "reject"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
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
            # `REMAINDER` is a special argparse value that consumes all
            # subsequent command-line arguments. This allows the message
            # to contain spaces without needing quotes.
            help="Message to use for rejection-comment."
            + "Always as last of command, it takes remainder of command",
        )

    def __osc(self) -> None:
        logger.info("Reject request %s", self.metadata.id.review_id)
        osc = OSC(self.config, self.metadata.id)
        osc.reject(self.args.group, self.args.reason, self.args.message)

    def __gitea(self) -> None:
        logger.info("Reject PR %s", self.metadata.id)
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.reject(self.args.reason, self.args.user, self.args.message)
        except GiteaError as e:
            logger.error(e)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        return complete_choices(
            [
                ("-g", "--group"),
                ("-r", "--reason"),
                ("-m", "--message"),
                ("-u", "--user"),
                # This tuple provides completion for the possible values of the --reason argument.
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
class Comment(_Base):
    """Command to add a comment to a review request."""

    command = "comment"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        # This command does not take any command-line arguments, so we
        # override the base method with `pass` to prevent inheriting
        # the --group and --user arguments.
        pass

    def __osc(self) -> None:
        # This command operates interactively, prompting the user directly
        # for the comment text when executed.
        comment = input("Comment: ")
        osc = OSC(self.config, self.metadata.id)
        osc.comment(comment)

    def __gitea(self) -> None:
        comment = input("Comment: ")
        try:
            gitea = Gitea(self.config, self.metadata.giteaprapi)
            gitea.comment(comment)
        except GiteaError as e:
            logger.error(e)
