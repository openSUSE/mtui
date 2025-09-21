"""A null object implementation of the `TestReport` class."""

from pathlib import Path
from typing import Any

from ..target.hostgroup import HostsGroup
from ..types import Product
from .testreport import TestReport


class NullTestReport(TestReport):
    """A null object implementation of the `TestReport` class.

    This class is used when no test report is loaded.
    """

    @property
    def _type(self) -> str:
        """Returns the type of the test report."""
        return "No"

    def __init__(self, *a, **kw) -> None:
        """Initializes the `NullTestReport` object."""
        super().__init__(*a, **kw)
        self.path = Path.cwd() / "None"

    @property
    def id(self) -> str:
        """Returns the ID of the test report."""
        return ""

    def __bool__(self) -> bool:
        """Returns `False`."""
        return False

    def target_wd(self, *paths) -> Path:
        """Returns the working directory for the target.

        Args:
            *paths: The path components to join to the working directory.

        Returns:
            The path to the working directory.
        """
        return self.config.target_tempdir.joinpath(*paths)  # type: ignore

    def _parser(self) -> dict[str, Any]:
        """Returns an empty dictionary."""
        return {}

    def _update_repos_parser(self) -> dict[Product, str]:
        """Returns an empty dictionary."""
        return {}

    def list_update_commands(self, targets: HostsGroup, display) -> None:
        """Does nothing."""
        ...
