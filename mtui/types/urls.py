"""A named tuple for representing a set of URLs."""

from typing import NamedTuple


class URLs(NamedTuple):
    """A named tuple that represents a set of URLs.

    Attributes:
        distri: The distribution.
        arch: The architecture.
        version: The version.
        url: The URL.
    """

    distri: str
    arch: str
    version: str
    url: str
