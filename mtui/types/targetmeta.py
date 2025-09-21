"""A named tuple for representing metadata for a target host."""

from typing import NamedTuple

from . import CommandLog
from . import Package


class TargetMeta(NamedTuple):
    """A named tuple that represents metadata for a target host.

    Attributes:
        hostname: The hostname of the target.
        system: The system information of the target.
        packages: A dictionary of packages for the target.
        hostlog: A list of command log entries for the target.
    """

    hostname: str
    system: str
    packages: dict[str, Package]
    hostlog: list[CommandLog]
