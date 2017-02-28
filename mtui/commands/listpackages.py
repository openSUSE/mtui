# -*- coding: utf-8 -*-


from mtui.commands import Command
from mtui.utils import blue, yellow, red, green
from mtui.utils import requires_update
from mtui.utils import complete_choices
from mtui import messages
from mtui.rpmver import RPMVersion


class ListPackages(Command):
    command = 'list_packages'

    state_map = {
        None: blue("not installed"),
        -1:   yellow("update needed"),
        0:    green("updated"),
        1:    red("too recent"),
    }

    def _vers2state(self, current, wanted):
        if not current:
            return self.state_map[None]

        return self.state_map[cmp(current, wanted)]

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "-p", "--package",
            type=str,
            action='append',
            default=[],
            help='Cumulative packages to list'
        )

        parser.add_argument(
            "-w", "--wanted",
            action='store_true',
            default=False,
            help="Print versions wanted by the testreport"
        )

        cls._add_hosts_arg(parser)

    @requires_update
    def _run_just_wanted(self):
        for xs in list(self.metadata.packages.items()):
            self.printPVLN(*(xs + ("",)))

    def run(self):
        if self.args.wanted:
            self._run_just_wanted()
            return

        hosts = self.parse_hosts()

        pkgs = list(self.metadata.packages.keys()
                    ) if self.metadata else []
        pkgs += self.args.package

        if not pkgs:
            raise messages.MissingPackagesError()

        for target, pvs in hosts.query_versions(pkgs):
            self.println("packages on {0} ({1}):".format(
                target.hostname,
                target.system,
            ))

            for p, v in list(pvs.items()):
                if self.metadata:
                    try:
                        wanted = self.metadata.packages[p]
                    except KeyError:
                        state = None
                    else:
                        state = self._vers2state(v, RPMVersion(wanted))
                else:
                    state = "" if v else self.state_map[None]

                self.printPVLN(p, v, state)

            self.println()

    def printPVLN(self, package, version, state):
        self.println('{0:30}: {1:15} {2}'.format(package, version, state))

    @staticmethod
    def complete(state, text, line, begidx, endidx):
        return complete_choices(
            [("-p", "--package"),
             ('-t', '--target'),
             ("-w", "--wanted"), ],
            line, text, state['hosts'].names())
