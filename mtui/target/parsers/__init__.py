

from mtui.types import Product
from mtui.types.systems import System
from mtui.target.parsers import product


def get_system(logger, connection):
    try:
        files = [x for x in connection.listdir('/etc/products.d') if x != 'qa.prod']
    except IOError:
        logger.debug("Not SUSE's system")
        suse = False
    else:
        suse = True
        try:
            files.remove('baseproduct')
        except ValueError:
            logger.debug("BaseProduct missing -> non SUSE sytem or wrongly installed")
            suse = False

    if not suse:
        with connection.open('/etc/os-release') as f:
            name, version, arch = product.parse_os_release(f)
        return System(Product(name, version, arch))

    basefile = connection.readlink('/etc/products.d/baseproduct')
    files.remove(basefile)
    with connection.open('/etc/products.d/{}'.format(basefile)) as f:
        name, version, arch = product.parse_product(f)
        base = Product(name, version, arch)

    addons = set()
    for x in files:
        with connection.open('/etc/products.d/{}'.format(x)) as f:
            name, version, arch = product.parse_product(f)
            addons.add(Product(name, version, arch))
    return System(base, addons)
