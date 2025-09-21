"""A named tuple for representing a test."""

from typing import NamedTuple


class Test(NamedTuple):
    """A named tuple that represents a test.

    Attributes:
        name: The name of the test.
        result: The result of the test.
        test_id: The ID of the test.
        arch: The architecture of the test.
        modules: A dictionary of modules for the test.
    """

    name: str
    result: str
    test_id: int
    arch: str
    modules: dict[str, str]
