"""Functions for parsing repository information from different sources.

This module contains functions for parsing repository information from
various sources, such as OBS (Open Build Service), SUSE Linux, and Git.
These functions extract product and repository data and return it in a
structured format.
"""

import xml.etree.ElementTree as ET
from itertools import chain
from os.path import join
from pathlib import Path

from .template.products import normalize, normalize_16
from .types import Product


def _read_project(path: Path) -> ET.Element:
    """Reads and parses a `project.xml` file.

    Args:
        path: The path to the directory containing the `project.xml` file.

    Returns:
        An XML element representing the project.
    """
    xml = path.joinpath("project.xml").read_text()
    return ET.fromstringlist(xml)


def _xmlparse(xml):
    """Parses the XML element to extract repository information.

    Args:
        xml: The XML element to parse.

    Returns:
        A generator of repository information.
    """
    return (
        (x.find("releasetarget").attrib["project"].split(":")[-3:], x.attrib["name"])
        for x in xml.findall("repository/path[@repository='update']/..")
        if "DEBUG" not in x.attrib["name"]
    )


def obsrepoparse(repository: str, path: Path) -> dict[Product, str]:
    """Parses OBS repository information.

    Args:
        repository: The base repository URL.
        path: The path to the directory containing the `project.xml` file.

    Returns:
        A dictionary mapping `Product` objects to repository URLs.
    """
    project = _xmlparse(_read_project(path))
    return {
        Product(x[0], x[1], x[2]): join(repository, y)
        for x, y in map(normalize, project)
    }


def _parse_product(product: str) -> list[Product]:
    """Parses a product string into a list of `Product` objects.

    Args:
        product: The product string to parse.

    Returns:
        A list of `Product` objects.
    """
    b, a = product.split(" (")
    arch: list[str] = a.rstrip(")").split(", ")
    base: list[str] = b.split(" ")
    return [Product(base[0], base[1], x) for x in arch]


def slrepoparse(repository: str, products: list[str]) -> dict[Product, str]:
    """Parses SUSE Linux repository information.

    Args:
        repository: The base repository URL.
        products: A list of product strings.

    Returns:
        A dictionary mapping `Product` objects to repository URLs.
    """
    return {
        x: join(repository, "images/repo", f"{x.name}-{x.version}-{x.arch}/")
        for x in chain.from_iterable(_parse_product(pd) for pd in products)
    }


def gitrepoparse(repository: str, products: list[str]) -> dict[Product, str]:
    """Parses Git repository information.

    Args:
        repository: The base repository URL.
        products: A list of product strings.

    Returns:
        A dictionary mapping `Product` objects to repository URLs.
    """
    return {
        x: join(repository, "standard")
        for x in chain.from_iterable(_parse_product(pd) for pd in products)
    }


def reporepoparse(
    repositories: frozenset[str], products: list[str]
) -> dict[Product, str]:
    """Parses repository information from a set of repositories.

    Args:
        repositories: A set of repository URLs.
        products: A list of product strings.

    Returns:
        A dictionary mapping `Product` objects to repository URLs.
    """
    return {
        normalize_16(ps): repo
        for pd in products
        for ps in _parse_product(pd)
        for repo in repositories
        if f"{ps.name}-{ps.version}-{ps.arch}" in repo
    }
