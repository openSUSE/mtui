import concurrent.futures
import readline

from mtui.argparse import ArgumentParser
from mtui.commands import Command
from mtui.target import Target
from mtui.utils import complete_choices


class Quit(Command):
    """
    Disconnects from all hosts and exits the programm.
    If a bootarg  argument is set, the hosts are either rebooted or powered off.
    """

    command = "quit"

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        parser.add_argument(
            "bootarg",
            nargs="?",
            choices=["reboot", "poweroff"],
            help="reboot or poweroff refhosts",
        )

    def _close_target(self, target: Target, args) -> None:
        self.targets[target].close(*args)
        self.targets.pop(target)

    def __call__(self) -> None:
        args_ = [self.args.bootarg] if self.args.bootarg else []

        with concurrent.futures.ThreadPoolExecutor() as executor:
            targets = [
                executor.submit(self._close_target, target, args_)
                for target in set(self.targets)
            ]
            concurrent.futures.wait(targets, timeout=45)

        try:
            readline.write_history_file(
                "{!s}/.mtui_history".format(self.prompt.homedir)
            )
        except Exception:
            pass

        self.sys.exit(0)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        return complete_choices([("reboot", "poweroff")], line, text)


class QExit(Quit):
    command = "exit"


class DEOF(Quit):
    command = "EOF"
