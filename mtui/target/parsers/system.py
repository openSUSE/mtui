"""A parser for the system information of a target host."""

from logging import getLogger
from pathlib import Path

from ...connection import Connection
from ...types import Product
from ...types.systems import System
from . import product

logger = getLogger("mtui.targer.parsers.system")


def parse_system(connection: Connection) -> tuple[System, bool]:
    """Parses the system information of a target host.

    Args:
        connection: A `Connection` object for the target host.

    Returns:
        A tuple containing a `System` object and a boolean indicating
        whether the system is transactional.
    """
    transactional = False
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
            return (System(Product("rhel", "6", "x86_64")), False)
        return (System(Product(name, version, arch)), False)

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

    # workaround for SLES_SAP 16.0x mismatch between products and repositories
    if base.name == "SLES_SAP" and base.version.startswith("16"):
        addons.add(Product("SLES-SAP", base.version, base.arch))

    try:
        _ = connection.sftp_open(Path("/usr/etc/transactional-update.conf"))
        transactional = True
        logger.info(f"Host: {connection.hostname} is transactional system")
    except FileNotFoundError:
        transactional = False

    return (System(base, addons), transactional)
