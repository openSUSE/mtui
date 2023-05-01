from typing import NamedTuple


class CommandLog(NamedTuple):
    command: str
    stdout: str
    stderr: str
    exitcode: int
    runtime: int
