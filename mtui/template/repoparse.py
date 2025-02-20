from pathlib import Path
import xml.etree.ElementTree as ET

from ..types import Product
from .products import normalize


def _read_project(path: Path) -> ET.Element:
    xml = path.joinpath("project.xml").read_text()
    return ET.fromstringlist(xml)


def _xmlparse(xml):
    return (
        (x.find("releasetarget").attrib["project"].split(":")[-3:], x.attrib["name"])
        for x in xml.findall("repository/path[@repository='update']/..")
        if "DEBUG" not in x.attrib["name"]
    )


def repoparse(path: Path) -> dict[Product, str]:
    project = _xmlparse(_read_project(path))
    return {Product(x[0], x[1], x[2]): y for x, y in map(normalize, project)}
