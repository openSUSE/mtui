# -*- coding: utf-8 -*-

from mtui.commands import Command
from mtui.utils import complete_choices
from mtui.utils import requires_update


class Install(Command):
    """
    Installs packages from the current active repositories.
    """

    command = "install"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument("package", nargs="+", help="package to install")

        cls._add_hosts_arg(parser)
        return parser

    @requires_update
    def run(self):
        self.log.info("Installing")
        packages = self.args.package
        targets = self.parse_hosts()

        try:
            self.metadata.perform_install(targets, packages)
        except KeyboardInterrupt:
            self.log.info("Installation process aborted")
            return
        except Exception as e:
            self.log.critical("failed to install packages")
            self.log.debug("{!s}".format(e))
            return

        self.log.info("Done")

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
    def run(self):
        self.log.info("Removing")
        packages = self.args.package
        targets = self.parse_hosts()

        try:
            self.metadata.perform_uninstall(targets, packages)
        except KeyboardInterrupt:
            self.log.info("Uninstallation process aborted")
            return
        except Exception as e:
            self.log.critical("failed to install packages")
            self.log.debug("{!s}".format(e))
            return

        self.log.info("Done")

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        parameters = [("-t", "--target")]
        packages = [(package,) for package in state["metadata"].get_package_list()]

        parameters += packages

        return complete_choices(parameters, line, text, state["hosts"].names())
