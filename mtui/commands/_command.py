from abc import ABC, abstractmethod
from argparse import Namespace, RawDescriptionHelpFormatter
from logging import getLogger

from ..argparse import ArgumentParser
from ..messages import HostIsNotConnectedError
from ..target.hostgroup import HostsGroup

logger = getLogger("mtui.commands.command")


class Command(ABC):
    command: str

    __slots__ = [
        "args",
        "config",
        "display",
        "metadata",
        "prompt",
        "sys",
        "targets",
    ]
    _check_subparser: str = ""
    """
    :type _check_subparser: str
    :param _check_subparser: Name of the subparser attribute if the
        derived class uses subparsers.

        On python 3 L{Command.parse_args} then checks if the attribute
        is set in parsed L{argparse.Namespace} and if not, prints an
        error message.
    """

    def __init__(self, args, config, sys, prompt) -> None:
        """:type args: str
        :param args: arguments remaidner for the command

        :type hosts: L{mtui.target.HostGroup}
        :param hosts: enabled hosts
        """

        self.args = args
        self.sys = sys
        self.config = config
        self.prompt = prompt
        self.metadata = prompt.metadata
        self.display = prompt.display
        self.targets: HostsGroup = prompt.targets

    @classmethod
    def parse_args(cls, args: str, sys) -> Namespace:
        arg = [] if args == "" else args.split()
        p = cls.argparser(sys)
        pa = p.parse_args(arg)

        if cls._check_subparser and not hasattr(pa, cls._check_subparser):
            p.error("too few arguments")

        return pa

    # in reality abstract method which implemenatation is optional
    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None: ...

    @classmethod
    def argparser(cls, sys) -> ArgumentParser:
        p = ArgumentParser(
            sys_=sys,
            prog=cls.command,
            description=cls.__doc__,
            formatter_class=RawDescriptionHelpFormatter,
        )
        cls._add_arguments(p)

        return p

    @staticmethod
    def complete(state, text, line, begidx, endidx) -> list[str]:
        """:type state: dict(prompt instance states)
        :returns: callable suitable for tab completion
        """

        return []

    @abstractmethod
    def __call__(self) -> None: ...

    def println(self, xs: str = "") -> None:
        """`print` replacement method for the outputs to be testable by
        injecting `StringIO`
        """

        self.sys.stdout.write(xs + "\n")
        self.sys.stdout.flush()

    @classmethod
    def _add_hosts_arg(cls, parser: ArgumentParser) -> None:
        parser.add_argument(
            "-t",
            "--target",
            dest="hosts",
            action="append",
            type=str,
            help="Host to act on. Can be used multiple times. "
            + "If is ommited all hosts are used",
        )

    def parse_hosts(self, enabled: bool = True) -> HostsGroup:
        """Parses self.args.hosts
        returns HostsGroup with hosts, or connection error.
        Handles decaprated 'all' alias

        By default all selects only enabled hosts

        use in run(self) ... etc
        """
        try:
            if self.args.hosts:
                targets = self.targets.select(self.args.hosts)
            else:
                targets = self.targets.select(enabled=enabled)
        except HostIsNotConnectedError as e:
            if e.host == "all":
                logger.error(e)
                logger.info("Using all hosts. Warning option 'all' is decaprated")

                targets = self.targets.select(enabled=enabled)

            else:
                raise

        return targets
