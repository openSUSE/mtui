import concurrent.futures
from mtui.commands import Command
from mtui.utils import complete_choices


class AddHost(Command):
    """
    Adds another machine to the target host list. The system type needs
    to be specified as well.
    """

    command = "add_host"

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "-t",
            "--target",
            required=True,
            action="append",
            help="address of the target host (should be the FQDN)",
        )
        return parser

    def run(self):
        with concurrent.futures.ThreadPoolExecutor() as executor:
            connections = [
                executor.submit(self.metadata.add_target, hostname)
                for hostname in self.args.target
            ]
            concurrent.futures.wait(connections)

    @staticmethod
    def complete(_, text, line, begidx, endix):
        return complete_choices([("-t", "--target")], line, text)
