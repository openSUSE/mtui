import xml.etree.ElementTree as ET


def parse_product(prod):
    root = ET.fromstringlist(prod)
    name = root.find("./name").text
    arch = root.find("./arch").text

    try:
        version = root.find("./baseversion").text
        sp = (
            root.find("./patchlevel").text
            if root.find("./patchlevel").text != "0"
            else ""
        )
        version += "-SP{}".format(sp) if sp else ""
    except AttributeError:
        version = root.find("./version").text

    # CAASP uses ALL for update repos and there is only one supported version at time
    if name == "CAASP":
        version = ""
    return (name, version, arch)


def parse_os_release(f):
    osinfo = {
        a.split("=")[0]: a.split("=")[1].rstrip("\n").translate({34: None})
        for a in f.readlines()
        if not (a.startswith("#") or a == "\n")
    }
    return (osinfo["ID"], osinfo["VERSION_ID"], "x86_64")
