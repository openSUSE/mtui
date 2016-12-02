from __future__ import absolute_import
from abc import ABCMeta, abstractmethod
import os
import errno

from subprocess import Popen
from time import sleep

from .argparse import ArgumentParser
from mtui.utils import complete_choices
from mtui.utils import blue, yellow, green, red
from mtui import messages
from mtui.utils import requires_update
from mtui.five import with_metaclass
from mtui.rpmver import RPMVersion
from mtui.messages import HostIsNotConnectedError
from mtui.messages import ListPackagesAllHost


class Command(with_metaclass(ABCMeta, object)):
    _check_subparser = None
    """
    :type _check_subparser: str
    :param _check_subparser: Name of the subparser attribute if the
        derived class uses subparsers.

        On python 3 L{Command.parse_args} then checks if the attribute
        is set in parsed L{argparse.Namespace} and if not, prints an
        error message.

        This behaviour changed between python 2 an 3 where python2
        argparse printed the error message by itself but python3
        returns an empty Namespace instance instead.
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
        self.log = logger
        self.config = config
        self.prompt = prompt
        self.metadata = prompt.metadata

    @classmethod
    def parse_args(cls, args, sys):
        args = [] if args is '' else args.split(" ")
        p = cls.argparser(sys)
        pa = p.parse_args(args)

        if cls._check_subparser and not hasattr(pa, cls._check_subparser):
            # workaround for python3 to keep same behaviour as with
            # python2
            # see https://gist.github.com/yaccz/2b7835b1e9429ee35ae5
            p.error("too few arguments")

        return pa

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
    def complete(hosts, text, line, begidx, endidx):
        """
        :type hosts: L{mtui.target.HostsGroup}
        :returns: callable suitable for tab completion
        """
        return lambda text, line, begidx, endidx: []

    @abstractmethod
    def run(self):
        raise RuntimeError()

    def println(self, xs=""):
        """
        `print` replacement method for the outputs to be testable by
        injecting `StringIO`
        """
        self.sys.stdout.write(xs + "\n")
        self.sys.stdout.flush()

    @classmethod
    def _add_hosts_arg(cls, parser):
        parser.add_argument(
            'hosts',
            metavar='host',
            type=str,
            nargs='*',
            help='hosts to act on. If no hosts are' +
            ' given all enabled hosts are used.')


class HostsUnlock(Command):
    command = 'unlock'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            '-f',
            '--force',
            action='store_true',
            help='force unlock - remove locks set by other users or'
            ' sessions')

        cls._add_hosts_arg(parser)
        return parser

    def run(self):
        args = self.args

        try:
            hosts = self.hosts.select(args.hosts)
        except ValueError as e:
            self.log.error(e)
            return

        hosts.unlock(force=args.force)

    @staticmethod
    def complete(hosts, text, line, begidx, endidx):
        return complete_choices(
            [
                ("-h", "--help"),
                ("-a",),
                ("-f", "--force")
            ],
            line,
            text,
            hosts.names()
        )


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
            "-p", "--packages",
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
        for xs in self.metadata.packages.items():
            self.printPVLN(*(xs + ("",)))

    def run(self):
        if self.args.wanted:
            self._run_just_wanted()
            return

        try:
            hosts = self.hosts.select(self.args.hosts)
        except HostIsNotConnectedError as e:
            if e.host == "all":
                self.log.error(e)
                self.log.info(ListPackagesAllHost())
                return
            else:
                raise

        pkgs = list(
            self.metadata.packages.keys()) if self.metadata else self.args.packages
        if not pkgs:
            raise messages.MissingPackagesError()

        for target, pvs in hosts.query_versions(pkgs):
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


class ReportBug(Command):

    """
    Open mtui bugzilla with fields common for all mtui bugs prefilled
    """
    command = "report-bug"

    def __init__(self, *a, **kw):
        self.popen = kw.pop('popen', Popen)

        super(ReportBug, self).__init__(*a, **kw)

    def run(self):
        url = self.config.report_bug_url

        if self.args.print_url:
            self.println(url)
            return

        args = ["xdg-open", url]
        try:
            p = self.popen(args)
        except OSError as e:
            if e.errno == errno.ENOENT:
                raise messages.SystemCommandNotFoundError(args[0])
            else:
                raise

        # xdg-open starts the appropriate command and waits for it
        # to exit.
        # Assuming to propagate it's return code to the caller.
        # However we don't want to block the mtui prompt.

        sleep(1)
        # So we wait a second to let the xdg-open do it's forks and
        # execs
        rc = p.poll()
        if rc is None:
            # and if by now it did not return, we'll assume it done it's
            # job successfully and kill it, leaving it's child still
            # running reparented to init.
            p.kill()
        elif rc != 0:
            # otherwise raise error if ended with non-zero
            raise messages.SystemCommandError(rc, args)
        else:
            # otherwise log a debug message as this state is expected
            # not to happen and we might be interested in knowing about
            # when it does.
            self.log.debug(messages.UnexpectedlyFastCleanExitFromXdgOpen())

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument(
            "-p", "--print-url",
            help='just print url to the stdout',
            action='store_true',
        )

        return parser

    @staticmethod
    def complete(_, text, line, begidx, endidx):
        return complete_choices([("-p", "--print-url"), ], line, text)


class Whoami(Command):

    """
    Display current user name and session pid.

    (username, pid) is used as user identity in rest of the codebase
    (eg. locking, logging on hosts) so it makes sense to treat this
    command consistently with those.

    TODO: consolidate these into a SessionIdentity object
    """
    command = 'whoami'

    def get_pid(self):
        return os.getpid()

    def run(self):
        self.println(" ".join([
            self.config.session_user,
            str(self.get_pid()),
            ]))


class Config(Command):

    """
    Display and manipulate (TODO) configuration in runtime.
    """
    command = "config"
    _check_subparser = "func"

    def run(self):
        getattr(self, self.args.func)()

    def show(self):
        attrs = self.args.attributes
        if not attrs:
            attrs = [x[0] for x in self.config.data]

        max_attr_len = len(max(attrs, key=len))
        for i in attrs:
            fmt = "{0:<" + str(max_attr_len) + "} = {1!r}"
            self.println(fmt.format(i, getattr(self.config, i)))

    @classmethod
    def _add_arguments(cls, p):
        sp = p.add_subparsers()
        p_show = sp.add_parser("show", help="show config values",
                               sys_=p.sys)
        p_show.add_argument("attributes", type=str, nargs="*")
        p_show.set_defaults(func="show")
