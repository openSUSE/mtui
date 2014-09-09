from __future__ import absolute_import
import argparse
from abc import ABCMeta, abstractmethod
from gettext import gettext as _
import traceback
import os

from .argparse import ArgumentParser
from mtui.target import HostsGroupException, TargetLockedError
from mtui.utils import flatten
from mtui.utils import blue, yellow, green, red
from mtui import messages
from mtui.utils import requires_update
from mtui.rpmver import RPMVersion

class Command(object):
    __metaclass__ = ABCMeta
    # FIXME: see L{CommandPrompt.__getattr__}

    stable = None
    """
    :type stable: str
    :param stable: Major version since which the command is stabilized.
        Derived classes must set this property.
        Version must include at least major and minor
    """

    def __init__(self, args, hosts, config, sys, logger, prompt):
        """
        :type args: str
        :param args: arguments remaidner for the command

        :type hosts: L{mtui.target.HostGroup}
        :param hosts: enabled hosts
        """
        self.hosts = hosts
        self.args = args
        self.sys = sys
        self.logger = logger
        self.config = config
        self.prompt = prompt
        self.metadata = prompt.metadata

    @classmethod
    def parse_args(cls, args, sys):
        args = [] if args is '' else args.split(" ")
        return cls.argparser(sys).parse_args(args)

    @classmethod
    def _add_arguments(cls, parser):
        """
        :returns: None
        """
        pass

    @classmethod
    def argparser(cls, sys):
        """
        :returns: L{ArgumentParser}
        """
        p = ArgumentParser(sys_=sys, prog=cls.command,
            description=cls.__doc__)
        cls._add_arguments(p)

        return p

    @staticmethod
    def completer(hosts):
        """
        :type hosts: L{mtui.target.HostsGroup}
        :returns: callable suitable for tab completion
        """
        raise NotImplementedError

    @abstractmethod
    def run(self):
        raise RuntimeError()

    def println(self, xs = ""):
        """
        `print` replacement method for the outputs to be testable by
        injecting `StringIO`
        """
        self.sys.stdout.write(xs + "\n")
        self.sys.stdout.flush()

    @classmethod
    def _add_hosts_arg(cls, parser):
        parser.add_argument('hosts', metavar = 'host', type = str,
            nargs = '*', help = 'hosts to act on. If no hosts are' +
            ' given all enabled hosts are used.')

class HostsUnlock(Command):
    command = 'unlock'
    stable  = '3.0'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument('-f', '--force', action='store_true',
            help='force unlock - remove locks set by other users or'
                ' sessions')

        cls._add_hosts_arg(parser)
        return parser

    def run(self):
        args = self.args

        try:
            hosts = self.hosts.select(args.hosts)
        except ValueError as e:
            self.logger.error(e)
            return

        try:
            hosts.unlock(force=args.force)
        except HostsGroupException as e:
            e.handle([
                (lambda e: isinstance(e, TargetLockedError),
                lambda e: self.logger.warning(e))
            ])

    @staticmethod
    def completer(hosts):
        def wrap(text, line, begidx, endidx):
            # TODO: there is argcomplete package as bach completion for
            # argparse that may simplyfi this. But declares support for
            # 2.7 and 3.3 only
            synonyms = [("-h", "--help"), ("-a",),  ("-f", "--force")]
            choices = set(flatten(synonyms) + hosts.names())

            ls = line.split(" ")
            ls.pop(0)

            for l in ls:
                if len(l) >= 2 and l[0] == "-" and l[1] != "-":
                    if len(l) > 2:
                        for c in list(l[1:]):
                            ls.append("-" + c)

                        continue

                for s in synonyms:
                    if l in s:
                        choices = choices - set(s)

            endchoices = []
            for c in choices:
                if text == c:
                    return [c]
                if text == c[0:len(text)]:
                    endchoices.append(c)

            return endchoices
        return wrap

class ListPackages(Command):
    command = 'list_packages'
    stable = '2.0'

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
            "-p", "--packages",
            type    = str,
            action  = 'append',
            default = [],
            help    = 'Cumulative packages to list'
        )

        parser.add_argument(
            "-w", "--wanted",
            action  = 'store_true',
            default = False,
            help    = "Print versions wanted by the testreport"
        )

        cls._add_hosts_arg(parser)

    @requires_update
    def _run_just_wanted(self):
        for xs in self.metadata.packages.items():
            self.printPVLN(*(xs + ("",)))

    def run(self):
        if self.args.wanted:
            self._run_just_wanted()
            return

        hosts = self.hosts.select(self.args.hosts)

        pkgs = list(self.metadata.packages.keys()) if self.metadata else self.args.packages
        if not pkgs:
            raise messages.MissingPackagesError()

        for target, pvs in hosts.query_versions(pkgs).items():
            self.println("packages on {0} ({1}):".format(
                target.hostname,
                target.system,
            ))

            for p, v in pvs.items():
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
        self.println('{0:30}: {1:15} {2}'.format(
            package,
            version,
            state
        ))

class Whoami(Command):
    """
    Display current user name and session pid.

    (username, pid) is used as user identity in rest of the codebase
    (eg. locking, logging on hosts) so it makes sense to treat this
    command consistently with those.

    TODO: consolidate these into a SessionIdentity object
    """
    command = 'whoami'
    stable = '2.0'

    def get_pid(self):
        return os.getpid()

    def run(self):
        self.println(" ".join([
            self.config.session_user,
            str(self.get_pid()),
            ]))

    @staticmethod
    def completer(hosts):
        raise NotImplementedError

class Config(Command):
    """
    Display and manipulate (TODO) configuration in runtime.
    """
    command = "config"
    stable = '3.0'

    def run(self):
        getattr(self, self.args.func)()

    def show(self):
        attrs = self.args.attributes
        if not attrs:
            attrs = [x[0] for x in self.config.data]

        max_attr_len = len(max(attrs, key = len))
        for i in attrs:
            fmt="{0:<" + str(max_attr_len) + "} = {1!r}"
            self.println(fmt.format(i, getattr(self.config, i)))

    @classmethod
    def _add_arguments(cls, p):
        sp = p.add_subparsers()
        p_show = sp.add_parser("show", help="show config values",
            sys_=p.sys)
        p_show.add_argument("attributes", type=str, nargs="*")
        p_show.set_defaults(func="show")
