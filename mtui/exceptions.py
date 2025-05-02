from argparse import ArgumentTypeError
from collections.abc import Sequence
from typing import Any


class RequestReviewIDParseError(ValueError, ArgumentTypeError):
    # Note: need to inherit ArgumentTypeError so the custom exception
    # messages get shown to the users properly
    # by L{argparse.ArgumentParser._get_value}

    def __init__(self, message: str) -> None:
        super().__init__("OBS Request Review ID: " + message)


class TooManyComponentsError(RequestReviewIDParseError):
    def __init__(self, limit: int) -> None:
        super().__init__(f"Too many components (> {limit})")

    @classmethod
    def raise_if(cls, xs: Sequence[Any], limit: int) -> None:
        if len(xs) > limit:
            raise cls(limit)


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


class UpdateError(Exception):
    def __init__(self, reason: str, host: str | None = None) -> None:
        self.reason: str = reason
        self.host: str | None = host

    def __str__(self) -> str:
        if self.host is None:
            return self.reason
        return "{!s}: {!s}".format(self.host, self.reason)
