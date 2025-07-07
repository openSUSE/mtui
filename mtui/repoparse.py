import xml.etree.ElementTree as ET
from itertools import chain
from os.path import join
from pathlib import Path

from .template.products import normalize
from .types import Product


def _read_project(path: Path) -> ET.Element:
    xml = path.joinpath("project.xml").read_text()
    return ET.fromstringlist(xml)


def _xmlparse(xml):
    return (
        (x.find("releasetarget").attrib["project"].split(":")[-3:], x.attrib["name"])
        for x in xml.findall("repository/path[@repository='update']/..")
        if "DEBUG" not in x.attrib["name"]
    )


def obsrepoparse(repository: str, path: Path) -> dict[Product, str]:
    project = _xmlparse(_read_project(path))
    return {
        Product(x[0], x[1], x[2]): join(repository, y)
        for x, y in map(normalize, project)
    }


def _parse_product(product: str) -> list[Product]:
    b, a = product.split(" (")
    arch: list[str] = a.rstrip(")").split(", ")
    base: list[str] = b.split(" ")
    return [Product(base[0], base[1], x) for x in arch]


def slrepoparse(repository: str, products: list[str]) -> dict[Product, str]:
    return {
        x: join(repository, "images/repo", f"{x.name}-{x.version}-{x.arch}/")
        for x in chain.from_iterable(_parse_product(pd) for pd in products)
    }


def gitrepoparse(repository: str, products: list[str]) -> dict[Product, str]:
    return {
        x: join(repository, "standard")
        for x in chain.from_iterable(_parse_product(pd) for pd in products)
    }
