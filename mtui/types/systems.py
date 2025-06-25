from . import Product


class UnknownSystemError(ValueError):
    pass


class System:
    """
    Store product information from refhost
    used by prettyprint for user and
    for correct update handling
    """

    def __init__(self, base: Product, addons: set[Product] = set()) -> None:
        """
        base: type Product(name, version, arch)
        addons: type set of Product(name, version, arch)
        """
        # TODO: check for correctness of base and addons types
        self.__base = base
        self.__addons = addons

    def get_release(self) -> str:
        name: str = self.__base.name  # noqa 'base' is always Product
        if name == "SUSE-Manager-Server":
            return "15"
        elif name == "rhel":
            return "YUM"
        elif name in (
            "SLES",
            "SLED",
            "SUSE_SLES",
            "SLES_SAP",
            "SUSE_SLES_SAP",
            "SLE_HPC",
            "SLES_TERADATA",
            "SLE_RT",
        ):
            return self.__base.version[:2]  # noqa base is always Product
        elif name == "openSUSE":
            return "15"
        elif name == "sle-studioonsite":
            return "11"
        elif name == "SL-Micro":
            return "slmicro"
        raise UnknownSystemError(name)

    def __str__(self) -> str:
        addons = "-modules" if self.__addons else ""
        msg: str = self.__base.name.lower()
        msg += addons
        msg += "-" + self.__base.version
        msg += "-" + self.__base.arch
        return msg

    def pretty(self) -> list[str]:
        msg = [
            "  Base product: {}-{}-{}".format(
                self.__base.name,
                self.__base.version,
                self.__base.arch,
            )
        ]
        if self.__addons:
            msg += ["  Installed Extensions and Modules:"]
            msg += [
                "      Addon: {:<53} - version: {}".format(x.name, x.version)
                for x in self.__addons
            ]
        return msg

    def __eq__(self, other) -> bool:
        return self.__base == other.__base and self.__addons == self.__addons

    def get_addons(self) -> set[Product]:
        return self.__addons

    def get_base(self) -> Product:
        return self.__base

    def flatten(self) -> set[Product]:
        flat = {self.__base}
        flat.update(self.__addons)
        return flat
