"""A class for representing a software package and its versions."""

from typing import final

from .rpmver import RPMVersion


@final
class Package:
    """Represents a software package and its versions."""

    __slots__ = [
        "name",
        "_before",
        "_after",
        "_required",
        "_current",
    ]

    def __init__(self, name: str) -> None:
        """Initializes the `Package` object.

        Args:
            name: The name of the package.
        """
        self.name: str = name
        self._before: str | RPMVersion | None = None
        self._after: str | RPMVersion | None = None
        self._required: str | RPMVersion | None = None
        self._current: str | RPMVersion | None = None

    @property
    def before(self) -> str | RPMVersion | None:
        """The version of the package before an update."""
        return self._before

    @before.setter
    def before(self, ver: str | RPMVersion | None) -> None:
        if isinstance(ver, str):
            self._before = RPMVersion(ver)
        elif isinstance(ver, RPMVersion):
            self._before = ver
        else:
            self._before = None

    @property
    def after(self) -> str | RPMVersion | None:
        """The version of the package after an update."""
        return self._after

    @after.setter
    def after(self, ver: str | RPMVersion | None) -> None:
        if isinstance(ver, str):
            self._after = RPMVersion(ver)
        elif isinstance(ver, RPMVersion):
            self._after = ver
        else:
            self._after = None

    @property
    def required(self) -> str | RPMVersion | None:
        """The required version of the package."""
        return self._required

    @required.setter
    def required(self, ver: str | RPMVersion | None) -> None:
        if isinstance(ver, str):
            self._required = RPMVersion(ver)
        elif isinstance(ver, RPMVersion):
            self._required = ver
        else:
            self._required = None

    @property
    def current(self) -> str | RPMVersion | None:
        """The current version of the package."""
        return self._current

    @current.setter
    def current(self, ver: str | RPMVersion | None) -> None:
        if isinstance(ver, str):
            self._current = RPMVersion(ver)
        elif isinstance(ver, RPMVersion):
            self._current = ver
        else:
            self._current = None

    def __str__(self) -> str:
        """Returns the name of the package."""
        return self.name

    def __repr__(self) -> str:
        """Returns a string representation of the `Package` object."""
        return f"<Package: {self.name}>"

    def __hash__(self) -> int:
        """Returns the hash of the package name."""
        return hash(self.name)
