import concurrent.futures
import readline

from mtui.commands import Command
from mtui.utils import complete_choices
from mtui.utils import prompt_user


class Quit(Command):
    """
    Disconnects from all hosts and exits the programm.
    If a bootarg  argument is set, the hosts are either rebooted or powered off.
    The tester is asked to save the XML log when exiting MTUI.
    """

    command = "quit"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "bootarg",
            nargs="?",
            choices=["reboot", "poweroff"],
            help="reboot or poweroff refhosts",
        )
        return parser

    def _close_target(self, target, args):
        self.targets[target].close(*args)
        self.targets.pop(target)

    def run(self):

        if not prompt_user(
            "save log? (Y,n) ",
            ["n", "no", "N", "No", "NO", "nein", "ne"],
            self.prompt.interactive,
        ):
            self.prompt._do_save_impl()

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
    def complete(_, text, line, begidx, endidx):
        return complete_choices([("reboot", "poweroff")], line, text)


class QExit(Quit):
    command = "exit"


class DEOF(Quit):
    command = "EOF"
