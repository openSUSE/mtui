"""Functions for parsing product and OS release information from files."""

import xml.etree.ElementTree as ET

from paramiko import SFTPFile


def parse_product(prod: SFTPFile) -> tuple[str, str, str]:
    """Parses a product file.

    Args:
        prod: An SFTPFile object representing the product file.

    Returns:
        A tuple containing the product name, version, and architecture.
    """
    root = ET.fromstringlist(prod)
    name: str = root.findtext("./name", "")
    arch: str = root.findtext("./arch", "")

    if version := root.findtext("./baseversion"):
        sp = (
            root.findtext("./patchlevel", "")
            if root.findtext("./patchlevel") != "0"
            else ""
        )
        version += f"-SP{sp}" if sp else ""
    else:
        version = root.findtext("./version", "")

    return (name, version, arch)


def parse_os_release(f: SFTPFile) -> tuple[str, str, str]:
    """Parses an os-release file.

    Args:
        f: An SFTPFile object representing the os-release file.

    Returns:
        A tuple containing the OS ID, version ID, and architecture.
    """
    osinfo: dict[str, str] = {
        a.split("=")[0]: a.split("=")[1].rstrip("\n").translate({34: None})
        for a in f.readlines()
        if not (a.startswith("#") or a == "\n")
    }
    return (osinfo["ID"], osinfo["VERSION_ID"], "x86_64")
