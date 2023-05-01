from typing import Dict, List, NamedTuple

from .commandlog import CommandLog
from .package import Package


class TargetMeta(NamedTuple):
    hostname: str
    system: str
    packages: Dict[str, Package]
    hostlog: List[CommandLog]
