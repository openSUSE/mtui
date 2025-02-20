from typing import NamedTuple


class Test(NamedTuple):
    name: str
    result: str
    test_id: int
    arch: str
    modules: dict[str, str]
