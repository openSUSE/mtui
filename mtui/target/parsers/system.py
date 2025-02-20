from logging import getLogger
from pathlib import Path

from . import product
from ...connection import Connection
from ...types import Product
from ...types.systems import System

logger = getLogger("mtui.targer.parsers.system")


def parse_system(connection: Connection) -> System:
    files: list[str] = []
    try:
        files = [
            x
            for x in connection.sftp_listdir(Path("/etc/products.d"))
            if x != "qa.prod" and x.endswith(".prod")
        ]
    except IOError:
        logger.debug("Not SUSE's system")
        suse = False
    else:
        suse = True

    if not suse:
        try:
            with connection.sftp_open(Path("/etc/os-release")) as f:
                name, version, arch = product.parse_os_release(f)
        except FileNotFoundError:
            # TODO: old RH systems have only /etc/redhat-release
            return System(Product("rhel", "6", "x86_64"))
        return System(Product(name, version, arch))

    if basefile := connection.sftp_readlink(Path("/etc/products.d/baseproduct")):
        files.remove(basefile)

    with connection.sftp_open(Path(f"/etc/products.d/{basefile}")) as f:
        logger.debug("Parsing basefile")
        name, version, arch = product.parse_product(f)
        base = Product(name, version, arch)

    addons: set[Product] = set()
    for x in files:
        with connection.sftp_open(Path(f"/etc/products.d/{x}")) as f:
            logger.debug("parsing - %s", x)
            name, version, arch = product.parse_product(f)
            addons.add(Product(name, version, arch))
    # SLE4SAP on sle12 contains also SLES repos :(
    if base.name == "SLES_SAP" and base.version.startswith("12"):
        addons.add(Product("SLES", base.version, base.arch))
        addons.add(Product("sle-ha", base.version, base.arch))
    return System(base, addons)
