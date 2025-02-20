class Package:
    __slots__ = [
        "name",
        "_before",
        "_after",
        "_required",
        "_current",
    ]

    def __init__(self, name: str) -> None:
        self.name = name
        self._before: str | None = None
        self._after: str | None = None
        self._required: str | None = None
        self._current: str | None = None

    @property
    def before(self) -> str | None:
        return self._before

    @before.setter
    def before(self, ver: str | None) -> None:
        self._before = ver

    @property
    def after(self) -> str | None:
        return self._after

    @after.setter
    def after(self, ver: str | None) -> None:
        self._after = ver

    @property
    def required(self) -> str | None:
        return self._required

    @required.setter
    def required(self, ver: str | None) -> None:
        self._required = ver

    @property
    def current(self) -> str | None:
        return self._current

    @current.setter
    def current(self, ver: str | None) -> None:
        self._current = ver
