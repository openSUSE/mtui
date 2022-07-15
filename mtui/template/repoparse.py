import xml.etree.ElementTree as ET
from mtui.types import Product

from mtui.template.products import normalize


def _read_project(path):
    with path.joinpath("project.xml").open(mode="r") as f:
        return ET.fromstringlist(f)


def _xmlparse(xml):
    return (
        (x.find("releasetarget").attrib["project"].split(":")[-3:], x.attrib["name"])
        for x in xml.findall("repository/path[@repository='update']/..")
        if "DEBUG" not in x.attrib["name"]
    )


def repoparse(path):
    project = _xmlparse(_read_project(path))
    return {Product(x[0], x[1], x[2]): y for x, y in map(normalize, project)}
