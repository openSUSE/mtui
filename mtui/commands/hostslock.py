# -*- coding: utf-8 -*-

from mtui.commands import Command
from mtui.utils import complete_choices


class HostLock(Command):
    """
    Lock host for exclusive usage. This locks all repository transactions
    like enabling or disabling the testing repository on the target hosts.
    The Hosts are locked with a timestamp, the UID and PID of the session.
    This influences the update process of concurrent instances, use with
    care.

    Enabled locks are automatically removed when exiting the session.
    To lock the run command on other sessions as well, it's necessary to
    set a comment.
    """

    command = "lock"

    @classmethod
    def _add_arguments(cls, parser):
        cls._add_hosts_arg(parser)
        return parser

    def run(self):
        targets = self.parse_hosts()
        comment = input("comment: ").strip()

        targets.lock(comment)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
