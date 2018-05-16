
from logging import getLogger

from mtui.types import Product
from mtui.types.systems import System
from mtui.target.parsers import product

logger = getLogger('mtui.target.parsers')


def parse_system(connection):
    try:
        files = [x for x in connection.listdir('/etc/products.d') if x != 'qa.prod' and x.endswith(".prod")]
    except IOError:
        logger.debug("Not SUSE's system")
        suse = False
    else:
        suse = True

    if not suse:
        try:
            with connection.open('/etc/os-release') as f:
                name, version, arch = product.parse_os_release(f)
        except FileNotFoundError:
            # TODO: old RH systems have only /etc/redhat-release
            return System(Product("rhel", "6", "x86_64"))
        return System(Product(name, version, arch))

    basefile = connection.readlink('/etc/products.d/baseproduct')
    files.remove(basefile)
    with connection.open('/etc/products.d/{}'.format(basefile)) as f:
        logger.debug("Parsing basefile")
        name, version, arch = product.parse_product(f)
        base = Product(name, version, arch)

    addons = set()
    for x in files:
        with connection.open('/etc/products.d/{}'.format(x)) as f:
            logger.debug("parsing - {}".format(x))
            name, version, arch = product.parse_product(f)
            addons.add(Product(name, version, arch))
    return System(base, addons)
