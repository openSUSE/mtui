import argparse
from abc import ABCMeta, abstractmethod
from gettext import gettext as _
import traceback
import os

from mtui.target import HostsGroupException, TargetLockedError
from mtui.utils import flatten

class ArgsParseFailure(RuntimeError):
    pass

class MTUICommandArgParser(argparse.ArgumentParser):
    def __init__(self, stdout, *a, **kw):
        super(MTUICommandArgParser, self).__init__(*a, **kw)
        self.stdout = stdout

    def print_help(self, file=None):
        """
        :param file: ignored, self.stdout is always used instead
        """
        # also takes care of default _HelpAction calling
        # print_help
        super(MTUICommandArgParser, self).print_help(self.stdout)

    def print_usage(self, file=None):
        """
        :param file: ignored, self.stdout is always used instead
        """
        super(MTUICommandArgParser, self).print_usage(self.stdout)

    def exit(self, *a, **kw):
        # don't want to call sys.exit when calling -h or parsing
        # failed inside mtui
        raise ArgsParseFailure

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

    def __init__(self, args, hosts, config, stdout, logger, prompt):
        """
        :type args: str
        :param args: arguments remaidner for the command

        :type hosts: L{mtui.target.HostGroup}
        :param hosts: enabled hosts
        """
        self.hosts = hosts
        self.args = args
        self.stdout = stdout
        self.logger = logger
        self.config = config
        self.prompt = prompt

    @classmethod
    def parse_args(cls, args, stdout):
        args = [] if args is '' else args.split(" ")
        return cls.argparser(stdout).parse_args(args)

    @classmethod
    def _add_arguments(cls, parser):
        """
        :returns: None
        """
        pass

    @classmethod
    def argparser(cls, stdout):
        """
        :returns: L{argparse.ArgumentParser}
        """
        p = MTUICommandArgParser(stdout, prog=cls.command,
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

    def println(self, xs):
        """
        `print` replacement method for the outputs to be testable by
        injecting `StringIO`
        """
        self.stdout.write(xs + "\n")
        self.stdout.flush()

class HostsUnlock(Command):
    command = 'unlock'
    stable  = '3.0'

    @classmethod
    def _add_arguments(cls, parser):
        parser.add_argument('-f', '--force', action='store_true',
            help='force unlock - remove locks set by other users or'
                ' sessions')

        parser.add_argument('hosts', metavar='host', type=list,
            nargs='*', help='hosts to unlock. If no hosts are' +
            ' given all enabled hosts are unlocked.')

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
            stdout=p.stdout)
        p_show.add_argument("attributes", type=str, nargs="*")
        p_show.set_defaults(func="show")
