from collections.abc import Callable
from functools import wraps
from typing import Any

from .colors import blue, green, red, yellow  # noqa: F401  # re-exported for callers
from .completion import (  # noqa: F401  # re-exported for callers
    complete_choices,
    complete_choices_filelist,
)
from .fileops import (  # noqa: F401  # re-exported for callers
    atomic_write_file,
    chdir,
    ensure_dir_exists,
    timestamp,
)
from .messages import TestReportNotLoadedError
from .term import (  # noqa: F401  # re-exported for callers
    filter_ansi,
    page,
    prompt_user,
    termsize,
)


def requires_update(fn: Callable) -> Callable:
    """A decorator that checks if a test report is loaded before executing.

    Args:
        fn: The function to decorate.

    Returns:
        The decorated function.

    """

    @wraps(fn)
    def wrap(self, *a, **kw) -> Any:
        if not self.metadata:
            raise TestReportNotLoadedError()
        return fn(self, *a, **kw)

    return wrap


class DictWithInjections(dict):
    """A dictionary that allows for a custom error on key lookup failure."""

    def __init__(self, *args, **kw) -> None:
        """Initializes the dictionary.

        Args:
            *args: Arguments to pass to the dict constructor.
            **kw: Keyword arguments to pass to the dict constructor.
                'key_error' is a special keyword argument that specifies
                the exception to raise on a key error.

        """
        self.key_error = kw.pop("key_error", KeyError)

        super().__init__(*args, **kw)

    def __getitem__(self, x):
        try:
            return super().__getitem__(x)
        except KeyError:
            raise self.key_error(x) from None


class SUTParse:
    """Parses a comma-separated string of SUTs into a formatted string."""

    def __init__(self, args: str) -> None:
        """Initializes the parser.

        Args:
            args: A comma-separated string of SUTs.

        """
        suts = args.split(",")
        targets = [f"-t {i!s}" for i in suts]
        self.args = " ".join(targets)

    def print_args(self) -> str:
        """Returns the formatted string of SUTs.

        Returns:
            The formatted string.

        """
        return self.args
