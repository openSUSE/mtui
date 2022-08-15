from logging import getLogger
from traceback import format_exc

from mtui.commands import Command
from mtui.messages import NoRefhostsDefinedError
from mtui.utils import complete_choices, requires_update

logger = getLogger("mtui.command.downgrade")


class Downgrade(Command):
    """
    Downgrades all related packages to the last released version

    Warning: this can't work for new packages.
    """

    command = "downgrade"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        cls._add_hosts_arg(parser)

    @requires_update
    def __call__(self):

        targets = self.parse_hosts()
        if not targets:
            raise NoRefhostsDefinedError

        logger.info("Downgrading")

        try:
            self.metadata.perform_downgrade(targets)
        except KeyboardInterrupt:
            logger.info("downgrade process canceled")
            return
        except Exception:
            logger.critical("failed to downgrade target systems")
            logger.debug(format_exc())
            return

        message = "done"
        for target in targets.values():
            target.query_versions()
            if message == "done":
                for package in target.packages.keys():
                    target.packages[package].before = target.packages[package].after
                    target.packages[package].after = target.packages[package].current
                    if (
                        target.packages[package].before
                        == target.packages[package].after
                        and target.packages[package].after is not None
                    ):
                        message = "downgrade not completed"
                        break
            else:
                break

        if message == "done":
            logger.info(message)
        else:
            logger.warn(message)

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [("-t", "--target")], line, text, state["hosts"].names()
        )
