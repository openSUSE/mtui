
from mtui.commands import Command
from mtui.utils import complete_choices


class ReloadProducts(Command):
    """Reload and parse products on target refhosts"""

    command = "reload_products"

    @classmethod
    def _add_arguments(cls, parser):
        cls._add_hosts_arg(parser)
        return parser

    def run(self):
        targets = self.parse_hosts()
        for target in targets:
            targets[target]._parse_system()
            self.log.info('Reloaded products on refhost {}'.format(target))

    def complete(state, text, line, begidx, endidx):
        return complete_choices([('-t', '--target'), ], line, text, state['hosts'].names())
