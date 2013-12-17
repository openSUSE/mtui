import argparse
from abc import ABCMeta, abstractmethod

from mtui.target import HostsGroupException, TargetLockedError

class Command(object):
    __metaclass__ = ABCMeta

    def __init__(self, raw_args, hosts, config, out):
        """
        :type raw_args: str
        :param raw_args: arguments remaidner for the command

        :type hosts: L{mtui.target.HostGroup}
        :param hosts: enabled hosts
        """
        self.hosts = hosts
        self.raw_args = raw_args and raw_args.split(" ") or []
        self.out = out
        self.config = config

    def args(self):
        return self.argparser().parse_args(self.raw_args)

    @abstractmethod
    def _argparser(self):
        """
        :returns: L{argparse.ArgumentParser}
        """
        raise NotImplementedError()

    def argparser(self):
        p = self._argparser()
        p.exit = lambda: None
        # don't want to call sys.exit when calling -h or parsing failed
        # inside mtui
        return p

class HostsUnlock(Command):
    command = 'unlock'

    def _argparser(self):
        parser = argparse.ArgumentParser(prog=self.command)
        parser.add_argument('-a', action='store_true',
            help='execute on all hosts')

        parser.add_argument('-f', action='store_true',
            help='force execution for locks set by other people')

        parser.add_argument('hosts', metavar='host', type=str,
            nargs='*', help='hosts to execute at')

        return parser

    def run(self):
        args = self.args()

        if args.a and bool(args.hosts):
            self.out.error("conflicting options")

        try:
            hosts = self.hosts.select(args.hosts)
        except ValueError as e:
            self.out.error(e)
            return

        try:
            hosts.unlock(force=args.f)
        except HostsGroupException as e:
            e.handle([
                (lambda e: isinstance(e, TargetLockedError),
                lambda e: self.out.warning(e))
            ])

    def complete(self):
        raise NotImplementedError
