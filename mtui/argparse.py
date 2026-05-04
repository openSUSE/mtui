"""A custom argument parser that avoids calling `sys.exit` on error."""

import argparse
import sys
from typing import NoReturn

try:  # optional 'completion' extra
    import argcomplete as _argcomplete  # ty: ignore[unresolved-import]
except ImportError:  # pragma: no cover - exercised only without the extra
    _argcomplete = None


class ArgsParseFailureError(RuntimeError):
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
        kw.pop("sys_", None)

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

    def parse_args(self, args=None, namespace=None):  # noqa: D401, ANN001
        """Run argcomplete (if installed) before delegating to argparse.

        ``argcomplete.autocomplete`` is a no-op unless the special
        ``_ARGCOMPLETE`` environment variable is set by the calling
        shell, so it is safe to call unconditionally.
        """
        if _argcomplete is not None:
            _argcomplete.autocomplete(self)
        return super().parse_args(args=args, namespace=namespace)

    def exit(self, status: int = 0, message: str | None = None) -> NoReturn:
        """Overrides the default exit behavior to raise an exception.

        This method raises an `ArgsParseFailureError` exception instead of
        calling `sys.exit`.

        Args:
            status: The exit status code.
            message: The error message to print.

        """
        if message:
            self._print_message(message, self.sys.stderr)

        raise ArgsParseFailureError(status)
