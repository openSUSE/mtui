from typing import NamedTuple

from . import CommandLog
from . import Package


class TargetMeta(NamedTuple):
    hostname: str
    system: str
    packages: dict[str, Package]
    hostlog: list[CommandLog]
