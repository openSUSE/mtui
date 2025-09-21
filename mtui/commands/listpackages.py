"""The `list_packages` command."""

from typing import final

from .. import messages
from ..argparse import ArgumentParser
from ..utils import blue, complete_choices, green, red, requires_update, yellow
from . import Command


@final
class ListPackages(Command):
    """Lists packages and their versions on target hosts."""

    command = "list_packages"

    state_map: dict[None | int, str] = {
        None: blue("not installed"),
        -1: yellow("update needed"),
        0: green("updated"),
        1: red("too recent"),
    }

    def _vers2state(self, current, wanted) -> str:
        """Converts package versions to a human-readable state.

        Args:
            current: The current package version.
            wanted: The wanted package version.

        Returns:
            A string representing the state of the package.
        """
        if not current:
            return self.state_map[None]

        return self.state_map[(current > wanted) - (current < wanted)]

    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """Adds arguments to the command's argument parser."""
        parser.add_argument(
            "-p",
            "--package",
            type=str,
            action="append",
            default=[],
            help="Cumulative packages to list",
        )

        parser.add_argument(
            "-w",
            "--wanted",
            action="store_true",
            default=False,
            help="Print versions wanted by the testreport",
        )

        cls._add_hosts_arg(parser)

    @requires_update
    def _run_just_wanted(self) -> None:
        """Prints the package versions wanted by the test report."""
        for key in self.metadata.packages.keys():
            self.println(f"Packages for version {key}:")
            for xs in list(self.metadata.packages[key].items()):
                self.printPVLN(*(xs + ("",)))

    def __call__(self) -> None:
        """Executes the `list_packages` command."""
        if self.args.wanted:
            self._run_just_wanted()
            return

        hosts = self.parse_hosts()

        pkgs = self.metadata.get_package_list() if self.metadata else []
        pkgs += self.args.package

        if not pkgs:
            raise messages.MissingPackagesError()

        for target, pvs in hosts.query_versions(pkgs):
            self.println(f"packages on {target.hostname} ({target.system}):")
            column_size = [30, 20]
            host_output = []
            for p, v in list(pvs.items()):
                if self.metadata:
                    try:
                        # if package p is in target.packages it alwas has set required --> from metadata
                        wanted = target.packages[p].required  # type: ignore
                    except KeyError:
                        state = None
                    else:
                        state = self._vers2state(v, wanted)
                else:
                    state = "" if v else self.state_map[None]

                if len(p) > column_size[0]:
                    column_size[0] = len(p) + 1
                if len(str(v)) > column_size[1]:
                    column_size[1] = len(str(v)) + 1

                host_output.append([p, v, state])

            format_output = "{{0:{0}}}: {{1!s:{1}}} {{2}}".format(
                column_size[0], column_size[1]
            )
            for line in host_output:
                self.printPVLN(line[0], line[1], line[2], format_output)

            self.println()

    def printPVLN(self, package, version, state, format_output="{0:30}: {1!s:20} {2}"):
        """Prints a formatted line of package, version, and state.

        Args:
            package: The name of the package.
            version: The version of the package.
            state: The state of the package.
            format_output: The format string to use.
        """
        self.println(format_output.format(package, version, state))

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """Provides tab completion for the command."""
        return complete_choices(
            [("-p", "--package"), ("-t", "--target"), ("-w", "--wanted")],
            line,
            text,
            state["hosts"].names(),
        )
