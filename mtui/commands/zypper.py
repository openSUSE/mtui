from logging import getLogger

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.utils import complete_choices, requires_update

logger = getLogger("mtui.command.zypper")


class Install(Command):
    """
    Installs packages from the current active repositories.
    """

    command = "install"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        parser.add_argument("package", nargs="+", type=str, help="package to install")

        cls._add_hosts_arg(parser)

    @requires_update
    def __call__(self) -> None:
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
            logger.debug("%s", e)
            return

        logger.info("Done")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        parameters: list[tuple[str, ...]] = [("-t", "--target")]
        packages: list[tuple[str, ...]] = [
            (package,) for package in state["metadata"].get_package_list()
        ]

        parameters += packages

        return complete_choices(parameters, line, text, state["hosts"].names())


class Uninstall(Command):
    """
    Removes packages from system
    """

    command = "uninstall"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        parser.add_argument("package", nargs="+", type=str, help="package to install")
        cls._add_hosts_arg(parser)

    @requires_update
    def __call__(self) -> None:
        logger.info("Removing")
        packages: list[str] = self.args.package
        targets = self.parse_hosts()

        try:
            self.metadata.perform_uninstall(targets, packages)
        except KeyboardInterrupt:
            logger.info("Uninstallation process aborted")
            return
        except Exception as e:
            logger.critical("failed to uninstall packages")
            logger.debug("%s", e)
            return

        logger.info("Done")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        parameters: list[tuple[str, ...]] = [("-t", "--target")]
        packages: list[tuple[str, ...]] = [
            (package,) for package in state["metadata"].get_package_list()
        ]

        parameters += packages

        return complete_choices(parameters, line, text, state["hosts"].names())
