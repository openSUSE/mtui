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
        self._before: str | RPMVersion | None = None
        self._after: str | RPMVersion | None = None
        self._required: str | RPMVersion | None = None
        self._current: str | RPMVersion | None = None

    @property
    def before(self) -> str | RPMVersion | None:
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
        return self.name

    def __repr__(self) -> str:
        return f"<Package: {self.name}>"

    def __hash__(self) -> int:
        return hash(self.name)
