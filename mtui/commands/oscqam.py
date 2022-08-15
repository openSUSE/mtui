from argparse import REMAINDER
from logging import getLogger
from shlex import quote
from subprocess import check_call
from traceback import format_exc

from mtui.commands import Command
from mtui.utils import complete_choices, requires_update

osc_api = {"SUSE": "https://api.suse.de", "openSUSE": "https://api.opensuse.org"}

logger = getLogger("mtui.command.osc")


class OSCCommand(Command):
    """Base class for osc commands, don't use directly"""

    _infopl = ""
    _errorpl = ""

    @classmethod
    def _add_arguments(cls, parser) -> None:
        parser.add_argument(
            "-g",
            "--group",
            nargs="?",
            action="append",
            help="Group wanted to {}".format(cls.command),
        )

    @requires_update
    def __call__(self):
        apiid, _, _, reviewid = str(self.metadata.id).split(":")
        logger.info("{}: {}".format(self._infopl, reviewid))
        cmd = "osc -A {} qam {}".format(osc_api[apiid], self.command)
        group = " "

        if self.args.group:
            for i in self.args.group:
                group += "".join("-G " + i) + " "

        cmd += group + reviewid
        logger.debug(cmd)
        try:
            check_call(cmd.split())
        except Exception as e:
            logger.error("{}: {!s}".format(self._errorpl, e))
            logger.debug(format_exc())

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices([("-g", "--group")], line, text)


class OSCAssign(OSCCommand):
    """
    Wrapper on 'osc qam assign' command, assings you current update.
    Can be specified groups for assigment
    """

    command = "assign"
    _infopl = "Assign request"
    _errorpl = "Assign failed"


class OSCUnassign(OSCCommand):
    """
    Wrapper on 'osc qam unassign' command, assings you current update.
    Can be specified groups for unassigment
    """

    command = "unassign"
    _infopl = "Unassign request"
    _errorpl = "Unassign failed"


class OSCApprove(OSCCommand):
    """
    Wrapper around 'osc qam approve' commad.
    It's possible to specify more groups to approve
    """

    command = "approve"
    _infopl = "Approve request"
    _errorpl = "Approve failed"


class OSCReject(Command):
    """
    Wrapper around 'osc qam reject', '-r'  option is required.
    """

    command = "reject"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        parser.add_argument(
            "-g",
            "--group",
            nargs="?",
            action="append",
            help="Group wanted by user to reject",
        )
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
            "--msg",
            nargs=REMAINDER,
            help="Message to use for rejection-comment."
            + "Always as last part of command please",
        )

    @requires_update
    def __call__(self):
        apiid, _, _, reviewid = str(self.metadata.id).split(":")
        logger.info(f"Reject request: {reviewid}")
        cmd = f"osc -A {osc_api[apiid]} qam reject"
        group = " "

        if self.args.group:
            for i in self.args.group:
                group += "".join("-G " + i) + " "

        reason = "-R " + self.args.reason

        cmd += group + reason + " " + reviewid + " "
        if self.args.msg:
            message = ""
            message += " ".join(self.args.msg)
            cmd += "-M " + quote(message)

        logger.debug(cmd)

        try:
            check_call(cmd, shell=True)
        except Exception as e:
            logger.error("Reject failed: {!s}".format(e))
            logger.debug(format_exc())

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices(
            [
                ("-g", "--group"),
                ("-r", "--reason"),
                ("-m", "--msg"),
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


class OSCComment(Command):
    """
    Wrapper around 'osc qam comment' command.
    """

    command = "comment"

    @requires_update
    def __call__(self):
        comment = input("Comment: ")
        reviewid = str(self.metadata.id)
        apiid = self.metadata.id.project
        cmd = f"osc -A {osc_api[apiid]} qam {self.command} {reviewid}"
        logger.debug(f"comment release request with: {cmd} {comment}")

        try:
            check_call(cmd.split() + [comment])
        except Exception as e:
            logger.error("Comment failed: {!s}".format(e))
            logger.debug(format_exc())
