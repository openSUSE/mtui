

class UnknownSystemError(ValueError):
    pass


class System(object):
    """
    Store product information from refhost
    used by prettyprint for user and
    for correct update handling
    """

    def __init__(self, base, addons=set()):
        """
        base: type Product(name, version, arch)
        addons: type set of Product(name, version, arch)
        """
        # TODO: check for correctness of base and addons types
        self._data = {"base": base, 'addons': addons}

    def get_release(self):
        if self._data['base'].name == 'CAASP':
            return "CAASP"
        elif self._data['base'].name == 'rhel':
            return "YUM"
        elif self._data['base'].name in ('SLES', 'SLED', 'SUSE_SLES'):
            return self._data['base'].version[:2]
        elif self._data['base'].name == 'openSUSE':
            return "12"
        else:
            raise UnknownSystemError(self._data['base'].name)

    def __str__(self):
        addons = "-modules" if self._data['addons'] else ''
        msg = self._data['base'].name.lower()
        msg += addons
        msg += "-" + self._data['base'].version
        msg += "-" + self._data['base'].arch
        return msg

    def pretty(self):
        msg = ["  Base product: {}-{}-{}".format(self._data['base'].name,
                                                self._data['base'].version, self._data['base'].arch)]
        if self._data['addons']:
            msg += ['  Installed Extensions and Modules:']
            msg += ['      Addon: {:<53} - version: {}'.format(x.name, x.version) for x in self._data['addons']]
        return msg

    def __eq__(self, other):
        return self._data == other._data

    def get_addons(self):
        return(self._data['addons'])

    def get_base(self):
        return(self._data['base'])
