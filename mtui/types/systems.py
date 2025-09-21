"""A class for representing the system information of a target host."""

from . import Product


class UnknownSystemError(ValueError):
    """Exception raised when the system is unknown."""

    pass


class System:
    """Represents the system information of a target host.

    This class stores product information from a reference host and is
    used for pretty-printing and for correct update handling.
    """

    def __init__(self, base: Product, addons: set[Product] = set()) -> None:
        """Initializes the `System` object.

        Args:
            base: The base product of the system.
            addons: A set of addons for the system.
        """
        # TODO: check for correctness of base and addons types
        self.__base = base
        self.__addons = addons

    def get_release(self) -> str:
        """Gets the release of the system.

        Returns:
            The release of the system.

        Raises:
            UnknownSystemError: If the system is unknown.
        """
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
        """Returns a string representation of the `System` object."""
        addons = "-modules" if self.__addons else ""
        msg: str = self.__base.name.lower()
        msg += addons
        msg += "-" + self.__base.version
        msg += "-" + self.__base.arch
        return msg

    def pretty(self) -> list[str]:
        """Returns a pretty-printed list of strings representing the system."""
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
        """Checks if two `System` objects are equal."""
        return self.__base == other.__base and self.__addons == self.__addons

    def get_addons(self) -> set[Product]:
        """Gets the addons of the system.

        Returns:
            A set of `Product` objects representing the addons.
        """
        return self.__addons

    def get_base(self) -> Product:
        """Gets the base product of the system.

        Returns:
            A `Product` object representing the base product.
        """
        return self.__base

    def flatten(self) -> set[Product]:
        """Returns a flattened set of all products in the system.

        Returns:
            A set of `Product` objects representing all products in the system.
        """
        flat = {self.__base}
        flat.update(self.__addons)
        return flat
