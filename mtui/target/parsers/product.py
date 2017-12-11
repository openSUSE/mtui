
import xml.etree.ElementTree as ET


def parse_product(prod):
    root = ET.fromstringlist(prod)
    name = root.find('./name').text
    arch = root.find('./arch').text

    version = root.find('./baseversion').text
    if version:
        sp = root.find('./patchlevel').text if root.find('./patchlevel').text != '0' else ""
        version += "SP{}".format(sp) if sp else ""
    else:
        version = root.find('/version').text

    return (name, version, arch)


def parse_os_release(f):
    # TODO : ...
    return ("rhel", "7", "x86_64")
