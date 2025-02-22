import concurrent.futures

from mtui.commands import Command
from mtui.utils import complete_choices


class AddHost(Command):
    """
    Adds another machine to the target host list.\n
    Withou parameter adds all host by Testplatform
    """

    command = "add_host"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        parser.add_argument(
            "-t",
            "--target",
            action="append",
            help="address of the target host (should be the FQDN)",
        )

    def __call__(self) -> None:
        if not self.args.target:
            for tp in self.metadata.testplatforms:
                self.metadata.refhosts_from_tp(tp)
            self.metadata.connect_targets()
        else:
            with concurrent.futures.ThreadPoolExecutor() as executor:
                connections = [
                    executor.submit(self.metadata.add_target, hostname)
                    for hostname in self.args.target
                ]
                concurrent.futures.wait(connections)

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        return complete_choices([("-t", "--target")], line, text)
