"""A named tuple for representing a product."""

from typing import NamedTuple


class Product(NamedTuple):
    """A named tuple that represents a product.

    Attributes:
        name: The name of the product.
        version: The version of the product.
        arch: The architecture of the product.
    """

    name: str
    version: str
    arch: str
