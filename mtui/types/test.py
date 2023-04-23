from typing import Dict, NamedTuple


class Test(NamedTuple):
    name: str
    result: str
    test_id: int
    arch: str
    modules: Dict[str, str]
