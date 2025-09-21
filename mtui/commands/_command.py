"""The base class for all commands in mtui."""

from abc import ABC, abstractmethod
from argparse import Namespace, RawDescriptionHelpFormatter
from logging import getLogger

from ..argparse import ArgumentParser
from ..messages import HostIsNotConnectedError
from ..target.hostgroup import HostsGroup

logger = getLogger("mtui.commands.command")


class Command(ABC):
    """An abstract base class for all commands in mtui."""

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
        """Initializes the command object.

        Args:
            args: The command-line arguments for the command.
            config: The application configuration.
            sys: The sys module.
            prompt: The command prompt object.
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
        """Parses the command-line arguments for the command.

        Args:
            args: The command-line arguments to parse.
            sys: The sys module.

        Returns:
            A namespace containing the parsed arguments.
        """
        arg = [] if args == "" else args.split()
        p = cls.argparser(sys)
        pa = p.parse_args(arg)

        if cls._check_subparser and not hasattr(pa, cls._check_subparser):
            p.error("too few arguments")

        return pa

    # in reality abstract method which implemenatation is optional
    @classmethod
    def _add_arguments(cls, parser: ArgumentParser) -> None:
        """A hook for adding arguments to the command's argument parser.

        Args:
            parser: The argument parser.
        """
        ...

    @classmethod
    def argparser(cls, sys) -> ArgumentParser:
        """Returns the argument parser for the command.

        Args:
            sys: The sys module.

        Returns:
            The argument parser for the command.
        """
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
        """Provides tab completion for the command.

        Args:
            state: A dictionary of prompt instance states.
            text: The text to complete.
            line: The current input line.
            begidx: The beginning index of the text to complete.
            endidx: The ending index of the text to complete.

        Returns:
            A list of possible completions.
        """

        return []

    @abstractmethod
    def __call__(self) -> None:
        """An abstract method that is called when the command is executed."""
        ...

    def println(self, xs: str = "") -> None:
        """A replacement for the `print` function that can be easily tested.

        Args:
            xs: The string to print.
        """

        self.sys.stdout.write(xs + "\n")
        self.sys.stdout.flush()

    @classmethod
    def _add_hosts_arg(cls, parser: ArgumentParser) -> None:
        """Adds the `-t`/`--target` argument to the argument parser.

        Args:
            parser: The argument parser.
        """
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
        """Parses the `hosts` argument and returns a `HostsGroup` object.

        By default, this method selects only enabled hosts.

        Args:
            enabled: Whether to select only enabled hosts.

        Returns:
            A `HostsGroup` object containing the selected hosts.
        """
        try:
            if self.args.hosts:
                targets = self.targets.select(self.args.hosts)
            else:
                targets = self.targets.select(enabled=enabled)
        except HostIsNotConnectedError as e:
            if e.host == "all":
                logger.error(e)
                logger.info("Using all hosts. Warning: option 'all' is deprecated")

                targets = self.targets.select(enabled=enabled)

            else:
                raise

        return targets
