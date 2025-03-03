from argparse import ArgumentTypeError
from collections.abc import Callable
from itertools import zip_longest

from ..utils import check_eq


class RequestReviewIDParseError(ValueError, ArgumentTypeError):
    # Note: need to inherit ArgumentTypeError so the custom exception
    # messages get shown to the users properly
    # by L{argparse.ArgumentParser._get_value}

    def __init__(self, message) -> None:
        super().__init__("OBS Request Review ID: " + message)


class TooManyComponentsError(RequestReviewIDParseError):
    limit = 4

    def __init__(self) -> None:
        super().__init__("Too many components (> {0})".format(self.limit))

    @classmethod
    def raise_if(cls, xs) -> None:
        if len(xs) > cls.limit:
            raise cls()


class InternalParseError(RequestReviewIDParseError):
    def __init__(self, f, cnt) -> None:
        super().__init__("Internal error: f: {0!r} cnt: {1!r}".format(f, cnt))


class MissingComponent(RequestReviewIDParseError):
    def __init__(self, index, expected) -> None:
        super().__init__(
            "Missing {0}. component. Expected: {1}".format(index, expected)
        )


class ComponentParseError(RequestReviewIDParseError):
    def __init__(self, index, expected, got) -> None:
        super().__init__(
            "Failed to parse {0}. component. Expected {1}. Got: {2!r}".format(
                index, expected, got
            )
        )


class RequestReviewID:
    def __init__(self, rrid: str) -> None:
        """
        :type rrid: str
        :param rrid: fully qualified Request Review ID
        """
        parsers: list[Callable[[str], str | int]] = [
            check_eq("SUSE", "openSUSE", "S"),
            check_eq("Maintenance", "M"),
            int,
            int,
        ]

        # filter empty entries
        xs: list[str] = [x for x in rrid.split(":") if x]
        TooManyComponentsError.raise_if(xs)
        # construct [(parser, input, index), ...]
        xs = [_apply_parser(*ys) for ys in zip_longest(parsers, xs, list(range(1, 5)))]

        self.project, self.kind, self.maintenance_id, self.review_id = xs

        if self.project == "S":
            self.project = "SUSE"

        if self.kind == "M":
            self.kind = "Maintenance"

    def __str__(self) -> str:
        return f"{self.project}:{self.kind}:{self.maintenance_id}:{self.review_id}"

    def __hash__(self) -> int:
        return hash(str(self))

    def __eq__(lhs, rhs) -> bool:
        return str(lhs) == str(rhs)

    def __ne__(lhs, rhs) -> bool:
        return not lhs.__eq__(rhs)


def _apply_parser(f, x, cnt):
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
