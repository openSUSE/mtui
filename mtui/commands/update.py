from logging import getLogger
from traceback import format_exc

from mtui.commands import Command
from mtui.messages import NoRefhostsDefinedError
from mtui.target.locks import TargetLockedError
from mtui.utils import complete_choices, requires_update

logger = getLogger("mtui.command.update")


class Update(Command):
    """
    Applies the testing update to the target hosts. While updating the
    machines, the pre-, post- and compare scripts are run before and
    after the update process.
    If the update adds new packages to the channel, the "--newpackage" parameter
    triggers the package installation right after the update.
    To skip the preparation procedure, append "--noprepare" to the argument list.
    """

    command = "update"

    @classmethod
    def _add_arguments(cls, parser) -> None:

        parser.add_argument(
            "--newpackage",
            action="store_const",
            const="newpackage",
            help="Install new packages after update",
        )
        parser.add_argument(
            "--noprepare",
            action="store_const",
            const="noprepare",
            help="Skip prepare procedure",
        )
        parser.add_argument(
            "--noscript",
            action="store_const",
            const="noscript",
            help="Don't run pre and post scripts",
        )

        cls._add_hosts_arg(parser)

    @requires_update
    def __call__(self):

        logger.info("Updating")

        targets = self.parse_hosts()
        if not targets:
            raise NoRefhostsDefinedError

        params = []
        params.append(self.args.newpackage)
        params.append(self.args.noprepare)
        params.append(self.args.noscript)

        try:
            self.metadata.perform_update(targets, params)

        except TargetLockedError as e:
            logger.warning(e)
            logger.critical("failed to update target systems")
            logger.debug(format_exc())
            return
        except Exception:
            logger.critical("failed to update target systems")
            logger.debug(format_exc())
            self.prompt.notify_user(
                "updating {!s} failed".format(self.prompt.session), "stock_dialog-error"
            )
            raise

        except KeyboardInterrupt:
            logger.info("update process canceled")
            return

        self.prompt.notify_user("updating {!s} finished".format(self.prompt.session))
        logger.info("done")

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [("-t", "--target"), ("--noprepare",), ("--newpackage",), ("--noscript",)],
            line,
            text,
            state["hosts"].names(),
        )
