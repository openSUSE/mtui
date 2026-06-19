"""A parser for the system information of a target host."""

from logging import getLogger

from ....types import Product
from ....types.systems import System
from ...connection import Connection
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
    # Batch every SFTP op against a single session instead of paying
    # one full handshake per call (~6 in the SUSE path, 1-2 on the
    # fallback path).
    with connection.sftp_session() as sftp:
        transactional = False
        files: list[str] = []
        try:
            files = [
                x
                for x in sftp.listdir("/etc/products.d")
                if x != "qa.prod" and x.endswith(".prod")
            ]
        except OSError:
            logger.debug("Not SUSE's system")
            suse = False
        else:
            suse = True

        if not suse:
            try:
                with sftp.open("/etc/os-release") as f:
                    name, version, arch = product.parse_os_release(f)
            except FileNotFoundError:
                # TODO: old RH systems have only /etc/redhat-release
                return (System(Product("rhel", "6", "x86_64")), False)
            return (System(Product(name, version, arch)), False)

        basefile = sftp.readlink("/etc/products.d/baseproduct")
        if basefile and basefile in files:
            files.remove(basefile)

        dangling_base = False
        if not basefile:
            # /etc/products.d/baseproduct is missing or not a symlink.
            logger.warning(
                "%s: /etc/products.d/baseproduct is missing or not a symlink",
                connection.hostname,
            )
            dangling_base = True
            base = Product("unknown", "", "")
        else:
            try:
                with sftp.open(f"/etc/products.d/{basefile}") as f:
                    logger.debug("Parsing basefile")
                    name, version, arch = product.parse_product(f)
                    base = Product(name, version, arch)
            except OSError:
                # Dangling symlink: the target product file is gone. Don't
                # crash the connect; warn and fall back to a best-effort
                # base derived from the symlink target name.
                logger.warning(
                    "%s: /etc/products.d/baseproduct -> %s is a dangling "
                    "symlink (target product file missing)",
                    connection.hostname,
                    basefile,
                )
                dangling_base = True
                base = Product(basefile.removesuffix(".prod"), "", "")

        addons: set[Product] = set()
        for x in files:
            with sftp.open(f"/etc/products.d/{x}") as f:
                logger.debug("parsing - %s", x)
                name, version, arch = product.parse_product(f)
                addons.add(Product(name, version, arch))
        # SLE4SAP on sle12 contains also SLES repos :(
        if base.name == "SLES_SAP" and base.version.startswith("12"):
            addons.add(Product("SLES", base.version, base.arch))
            addons.add(Product("sle-ha", base.version, base.arch))

        # workaround for SLES_SAP 16.0x mismatch between products and
        # repositories
        if base.name == "SLES_SAP" and base.version.startswith("16"):
            addons.add(Product("sle-ha", base.version, base.arch))

        # transactional-update ships its config in /usr/etc on newer
        # systems (SL-Micro 6.x), but older transactional systems
        # (SLE Micro 5.x, openSUSE MicroOS) keep it in /etc. Probe both so
        # the older layout is not misdetected as non-transactional.
        transactional = False
        for conf in (
            "/usr/etc/transactional-update.conf",
            "/etc/transactional-update.conf",
        ):
            try:
                sftp.open(conf)
            except FileNotFoundError:
                continue
            transactional = True
            logger.info("Host: %s is transactional system", connection.hostname)
            break

    return (System(base, addons, dangling_base=dangling_base), transactional)
