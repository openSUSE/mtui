import xml.etree.ElementTree as ET

from paramiko import SFTPFile


def parse_product(prod: SFTPFile) -> tuple[str, str, str]:
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
    osinfo: dict[str, str] = {
        a.split("=")[0]: a.split("=")[1].rstrip("\n").translate({34: None})
        for a in f.readlines()
        if not (a.startswith("#") or a == "\n")
    }
    return (osinfo["ID"], osinfo["VERSION_ID"], "x86_64")
