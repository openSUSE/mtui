"""The `add_host` command."""

import concurrent.futures
from logging import getLogger

from ..cli.completion import complete_choices
from . import Command

logger = getLogger("mtui.commands.addhost")


class AddHost(Command):
    """Adds one or more machines to the target host list.

    If no target is specified, all hosts from the test platform are added.
    """

    command = "add_host"

    @classmethod
    def _add_arguments(cls, parser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "-t",
            "--target",
            action="append",
            help="address of the target host (should be the FQDN)",
        )
        parser.add_argument(
            "-k",
            "--keep-mode",
            action="store_true",
            help="do not switch to the manual workflow when in automatic mode",
        )

    def __call__(self) -> None:
        """Executes the `add_host` command."""
        # Running add_host is a manual action. If the session is still in
        # automatic mode the user almost certainly meant to test manually
        # (and just forgot to switch), so move to the manual workflow --
        # unless --keep-mode was given.
        if self.config.auto and not self.args.keep_mode:
            logger.info("add_host: switching from automatic to manual workflow")
            self.config.auto = False
            self.config.kernel = False
            self.prompt.set_prompt(self.prompt.session)

        before = set(self.metadata.targets)

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

        self._report_product_warnings(before)

    def _report_product_warnings(self, before: set) -> None:
        """Print any product-drift warnings for hosts added by this call.

        :meth:`TestReport._verify_target_products` records drift versus
        ``refhosts.yml`` in ``metadata.product_warnings`` and logs it, but
        MCP clients only see command stdout -- so echo the warnings via
        ``println`` for the hosts that were just connected.
        """
        added = sorted(set(self.metadata.targets) - before)
        for host in added:
            for line in self.metadata.product_warnings.get(host, []):
                self.println(f"WARNING: {host}: {line}")

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices([("-t", "--target"), ("-k", "--keep-mode")], line, text)
