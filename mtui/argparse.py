"""A custom argument parser that avoids calling `sys.exit` on error."""

import argparse
import sys


class ArgsParseFailure(RuntimeError):
    """Exception raised when argument parsing fails."""

    def __init__(self, status: int = 0) -> None:
        """Initializes the exception.

        Args:
            status: The exit status code.
        """
        self.status = status
        super().__init__()


class ArgumentParser(argparse.ArgumentParser):
    """A custom argument parser that avoids calling `sys.exit` on error."""

    def __init__(self, *a, **kw) -> None:
        """Initializes the parser.

        Args:
            *a: Arguments to pass to the parent constructor.
            **kw: Keyword arguments to pass to the parent constructor.
        """
        self.sys = kw.get("sys_", sys)
        if "sys_" in kw:
            del kw["sys_"]

        super().__init__(*a, **kw)

    def print_help(self, file=None) -> None:
        """Prints the help message to stdout.

        Args:
            file: This argument is ignored.
        """
        # also takes care of default _HelpAction calling
        # print_help
        super().print_help(self.sys.stdout)

    def print_usage(self, file=None) -> None:
        """Prints the usage message to stdout.

        Args:
            file: This argument is ignored.
        """
        super().print_usage(self.sys.stdout)

    def exit(self, status: int = 0, message: str | None = None) -> None:  # type: ignore
        """Overrides the default exit behavior to raise an exception.

        This method raises an `ArgsParseFailure` exception instead of
        calling `sys.exit`.

        Args:
            status: The exit status code.
            message: The error message to print.
        """
        if message:
            self._print_message(message, self.sys.stderr)

        raise ArgsParseFailure(status)
