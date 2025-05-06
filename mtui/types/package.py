from typing import final

from .rpmver import RPMVersion


@final
class Package:
    __slots__ = [
        "name",
        "_before",
        "_after",
        "_required",
        "_current",
    ]

    def __init__(self, name: str) -> None:
        self.name: str = name
        self._before: RPMVersion | None = None
        self._after: RPMVersion | None = None
        self._required: RPMVersion | None = None
        self._current: RPMVersion | None = None

    @property
    def before(self) -> RPMVersion | None:
        return self._before

    @before.setter
    def before(self, ver: str | RPMVersion | None) -> None:
        if isinstance(ver, str):
            self._before = RPMVersion(ver)
        elif isinstance(ver, RPMVersion):
            self._before = ver

    @property
    def after(self) -> RPMVersion | None:
        return self._after

    @after.setter
    def after(self, ver: str | RPMVersion | None) -> None:
        if isinstance(ver, str):
            self._after = RPMVersion(ver)
        elif isinstance(ver, RPMVersion):
            self._after = ver

    @property
    def required(self) -> RPMVersion | None:
        return self._required

    @required.setter
    def required(self, ver: str | RPMVersion | None) -> None:
        if isinstance(ver, str):
            self._required = RPMVersion(ver)
        elif isinstance(ver, RPMVersion):
            self._required = ver

    @property
    def current(self) -> RPMVersion | None:
        return self._current

    @current.setter
    def current(self, ver: str | RPMVersion | None) -> None:
        if isinstance(ver, str):
            self._current = RPMVersion(ver)
        elif isinstance(ver, RPMVersion):
            self._current = ver

    def __str__(self) -> str:
        return self.name

    def __repr__(self) -> str:
        return f"<Package: {self.name}>"

    def __hash__(self) -> int:
        return hash(self.name)
