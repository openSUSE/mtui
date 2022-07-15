from typing import Optional


class Package:
    __slots__ = [
        "name",
        "_before",
        "_after",
        "_required",
        "_current",
    ]

    def __init__(self, name):
        self.name = name
        self._before = None
        self._after = None
        self._required = None
        self._current = None

    @property
    def before(self) -> Optional[str]:
        return self._before

    @before.setter
    def before(self, ver: Optional[str]) -> None:
        self._before = ver

    @property
    def after(self) -> Optional[str]:
        return self._after

    @after.setter
    def after(self, ver: Optional[str]) -> None:
        self._after = ver

    @property
    def required(self) -> Optional[str]:
        return self._required

    @required.setter
    def required(self, ver: Optional[str]) -> None:
        self._required = ver

    @property
    def current(self) -> Optional[str]:
        return self._current

    @current.setter
    def current(self, ver: Optional[str]) -> None:
        self._current = ver
