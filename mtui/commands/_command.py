# -*- coding: utf-8 -*-

from __future__ import absolute_import

from abc import ABCMeta, abstractmethod

from ..argparse import ArgumentParser
from mtui.five import with_metaclass
from mtui.messages import HostIsNotConnectedError


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
        self.display = prompt.display
        self.targets = prompt.targets

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
    def complete(state, text, line, begidx, endidx):
        """
        :type state: dict(prompt instance states)
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
            '-t', '--target', dest='hosts', action='append', type=str,
            help='Host to act on. Can be used multiple times.' +
            'If is ommited all hosts are used')

    def parse_hosts(self, henabled=True):
        """
        parses self.args.hosts
        returns [str] with hosts, or connection error.
        Handles decaprated 'all' alias

        By default all selects only enabled hosts

        use in run(self) ... etc
        """

        try:
            if self.args.hosts:
                targets = self.hosts.select(self.args.hosts)
            else:
                targets = self.hosts.select(enabled=henabled)
        except HostIsNotConnectedError as e:
            if e.host == 'all':
                self.log.error(e)
                self.log.info(
                    "Using all hosts. Warning option 'all' is decaprated")

                targets = self.hosts.select(enabled=henabled)

            else:
                raise

        return targets
