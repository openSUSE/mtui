from logging import getLogger

from mtui.commands import Command
from mtui.utils import complete_choices, requires_update

logger = getLogger("mtui.command.zypper")


class Install(Command):
    """
    Installs packages from the current active repositories.
    """

    command = "install"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        parser.add_argument("package", nargs="+", help="package to install")

        cls._add_hosts_arg(parser)

    @requires_update
    def __call__(self):
        logger.info("Installing")
        packages = self.args.package
        targets = self.parse_hosts()

        try:
            self.metadata.perform_install(targets, packages)
        except KeyboardInterrupt:
            logger.info("Installation process aborted")
            return
        except Exception as e:
            logger.critical("failed to install packages")
            logger.debug("{!s}".format(e))
            return

        logger.info("Done")

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        parameters = [("-t", "--target")]
        packages = [(package,) for package in state["metadata"].get_package_list()]

        parameters += packages

        return complete_choices(parameters, line, text, state["hosts"].names())


class Uninstall(Command):
    """
    Removes packages from system
    """

    command = "uninstall"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument("package", nargs="+", help="package to install")

        cls._add_hosts_arg(parser)
        return parser

    @requires_update
    def __call__(self):
        logger.info("Removing")
        packages = self.args.package
        targets = self.parse_hosts()

        try:
            self.metadata.perform_uninstall(targets, packages)
        except KeyboardInterrupt:
            logger.info("Uninstallation process aborted")
            return
        except Exception as e:
            logger.critical("failed to install packages")
            logger.debug("{!s}".format(e))
            return

        logger.info("Done")

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        parameters = [("-t", "--target")]
        packages = [(package,) for package in state["metadata"].get_package_list()]

        parameters += packages

        return complete_choices(parameters, line, text, state["hosts"].names())
