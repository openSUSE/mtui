"""A wrapper for interacting with the `osc qam` command-line tool."""

from logging import getLogger
from shlex import join as shlex_join
from shlex import quote
from subprocess import CalledProcessError, check_call

from ..config import Config
from ..types.rrid import RequestReviewID

logger = getLogger("mtui.connector.oscqam")

API = "https://api.suse.de"


class OSC:
    """A wrapper for interacting with the `osc qam` command-line tool."""

    def __init__(self, config: Config, rrid: RequestReviewID) -> None:
        """Initializes the OSC connector.

        Args:
            config: An instance of the application's Config class.
            rrid: A RequestReviewID object representing the target review.
        """
        self.config = config
        self.rrid = rrid

    def __operation(
        self,
        operation: str,
        groups: list[str],
        reason: str = "",
        message: str = "",
        comment: str = "",
    ) -> None:
        """Constructs and executes `osc qam` commands safely.

        This method builds the command as a list of arguments to
        prevent command injection vulnerabilities.

        Args:
            operation: The `qam` subcommand to perform (e.g., 'approve').
            groups: A list of group names to apply the operation to.
            reason: The reason for a rejection.
            message: The message to include with the operation.
            comment: A comment to add to the request.
        """
        # Start with the base command components that are always present.
        base_cmd = ["osc", "-A", API, "qam", operation]

        # Dynamically build the list of group arguments (e.g., ["-G", "group1", "-G", "group2"]).
        group_args = []
        if groups:
            for g in groups:
                group_args.extend(["-G", g])

        # Conditionally add optional arguments to the command list.
        reason_args = ["-R", reason] if reason else []
        message_args = ["-M", quote(message)] if message else []

        # Add a specific workaround for 'PI' kinds which have a different RRID format
        # that oscqam does not expect by default.
        skip_args = (
            ["--skip-template"]
            if (self.rrid.kind == "PI" and operation == "assign")
            else []
        )

        # must be converted to str -> shlex.join accepts only str or bytestr
        rrid_args = [str(self.rrid.review_id)]
        comment_args = [quote(comment)] if comment else []

        # Combine all parts into the final command list in the correct order.
        command: list[str] = (
            base_cmd
            + group_args
            + rrid_args
            + reason_args
            + skip_args
            + message_args
            + comment_args
        )

        logger.info("Performing '%s' operation on %s", operation, str(self.rrid))

        # For logging purposes, it's helpful to see the command as a single string.
        # shlex_join (or shlex.join) safely quotes each argument.
        logger.debug("Executing command: %s", shlex_join(command))

        try:
            check_call(command)

        except CalledProcessError:
            logger.error(
                "'%s' operation failed. The command returned a non-zero exit code.",
                operation,
            )
            logger.debug("Call stack trace:", stack_info=True)

        except FileNotFoundError:
            logger.error("'osc' command not found. Is it installed and in your PATH?")

    def approve(self, group: list[str]) -> None:
        """Approves a review request for one or more groups.

        Args:
            group: A list of group names to approve the request for.
        """
        self.__operation("approve", group)

    def assign(self, group: list[str]) -> None:
        """Assigns a review request to one or more groups.

        Args:
            group: A list of group names to assign the request to.
        """
        self.__operation("assign", group)

    def unassign(self, group: list[str]) -> None:
        """Unassigns a review request from one or more groups.

        Args:
            group: A list of group names to unassign the request from.
        """
        self.__operation("unassign", group)

    def comment(self, comment: str) -> None:
        """Adds a comment to a review request.

        Args:
            comment: The comment to add.
        """
        self.__operation("comment", [], comment=comment)

    def reject(self, group: list[str], reason: str, message: str) -> None:
        """Rejects a review request.

        Args:
            group: A list of group names to reject the request for.
            reason: The reason for the rejection.
            message: The rejection message.
        """
        self.__operation("reject", group, reason=reason, message=message)
