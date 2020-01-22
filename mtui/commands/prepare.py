from logging import getLogger
from traceback import format_exc

from mtui.commands import Command
from mtui.messages import NoRefhostsDefinedError
from mtui.types.systems import UnknownSystemError
from mtui.utils import complete_choices, requires_update

logger = getLogger("mtui.commands.prepare")


class Prepare(Command):
    """
    Installs missing and update packages to last released version.
    This is also run by the update procedure before applying the updates.
    """

    command = "prepare"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "-f",
            "--force",
            action="store_const",
            const="force",
            help="force package installation",
        )
        parser.add_argument(
            "-i",
            "--installed",
            action="store_const",
            const="installed",
            help="prepare only installed packages",
        )
        parser.add_argument(
            "-u",
            "--update",
            action="store_const",
            const="testing",
            help="enable test update repositories",
        )
        cls._add_hosts_arg(parser)
        return parser

    @requires_update
    def __call__(self):

        targets = self.parse_hosts()
        if not targets:
            raise NoRefhostsDefinedError

        params = []
        params.append(self.args.force)
        params.append(self.args.installed)
        params.append(self.args.update)

        logger.info("preparing")

        try:
            self.metadata.perform_prepare(
                targets,
                force="force" in params,
                installed_only="installed" in params,
                testing="testing" in params,
            )
        except KeyboardInterrupt:
            logger.info("preparation process canceled")
            return False
        except UnknownSystemError as e:
            logger.critical(f"Failed to prepare system due unknown base system : {e}")
            return False
        except Exception:
            logger.critical("Failed to prepare systems")
            logger.debug(format_exc())
            return False

        logger.info("done")

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [
                ("-t", "--target"),
                ("-i", "--installed"),
                ("-f", "--force"),
                ("-u", "--update"),
            ],
            line,
            text,
            state["hosts"].names(),
        )
