from itertools import zip_longest
from typing import Any, Callable, Sequence, final

from ..exceptions import (
    ComponentParseError,
    InternalParseError,
    MissingComponent,
    TooManyComponentsError,
)


def apply_parser(f, x, cnt):
    if not f or not cnt:
        raise InternalParseError(f, cnt)

    if not x:
        raise MissingComponent(cnt, f)

    try:
        return f(x)
    except Exception as e:
        new = ComponentParseError(cnt, f, x)
        new.__cause__ = e
        raise new


class check_eq:
    """
    Usage: check_eq(x)(y)
    :return: y for y if (x == y) is True otherwise raises
    :raises: ValueError
    """

    def __init__(self, *x) -> None:
        self.x: Sequence[Any] = x

    def __call__(self, y: Any) -> Any:
        if y not in self.x:
            raise ValueError(f"Expected: {self.x!r}, got: {y!r}")
        return y

    def __repr__(self) -> str:
        return f"<{self.__class__.__module__}.{self.__class__.__name__} {self.x!r}>"

    def __str__(self) -> str:
        return f"{self.x!r}"


class check_type:
    """
    Usage: check_type(x)(y)
    :return: y for y if x(y) otherwise raises
    :raises: ValueError
    """

    def __init__(self, *x) -> None:
        self.x: Sequence[Any] = x

    def __call__(self, y: Any) -> Any:
        err = False
        for f in self.x:
            err = False
            try:
                return f(y)
            except ValueError:
                err = True

        if err:
            raise ValueError(f"Expected {self.x!r}, got: {y!r}")

    def __repr__(self) -> str:
        return f"<{self.__class__.__module__}.{self.__class__.__name__} {self.x!r}>"

    def __str__(self) -> str:
        return f"convertible to {self.x!r}"


@final
class RequestReviewID:
    def __init__(self, rrid: str) -> None:
        """
        :type rrid: str
        :param rrid: fully qualified Request Review ID
        """
        xs: list[str] = [x for x in rrid.split(":") if x]
        parsers: list[Callable[[str], str | int]] = [
            check_eq("SUSE", "S"),
            check_eq("SLFO", "S", "Maintenance", "M", "PI", "P"),
            check_type(int, str),
            check_type(int),
        ]

        TooManyComponentsError.raise_if(xs, 4)

        xs = [
            apply_parser(*ys)
            for ys in zip_longest(parsers, xs, range(1, len(parsers) + 1))
        ]
        self.project, self.kind, self.maintenance_id, self.review_id = xs

        if self.project == "S":
            self.project = "SUSE"

        if self.kind == "M":
            self.kind = "Maintenance"
        elif self.kind == "S":
            self.kind = "SLFO"
        elif self.kind == "P":
            self.kind = "PI"

    def __str__(self) -> str:
        return f"{self.project}:{self.kind}:{self.maintenance_id}:{self.review_id}"

    def __repr__(self) -> str:
        return f"<RRID - {self.project}:{self.kind}:{self.maintenance_id}:{self.review_id}>"

    def __hash__(self) -> int:
        return hash(str(self))

    def __eq__(self, other: object) -> bool:
        return str(self) == str(other)

    def __ne__(self, other: object) -> bool:
        return not self.__eq__(other)
