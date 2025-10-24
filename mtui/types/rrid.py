"""A class for parsing and representing OBS Request Review IDs."""

from itertools import zip_longest
from typing import Any, Callable, Sequence, final

from ..exceptions import (
    ComponentParseError,
    InternalParseError,
    MissingComponent,
    TooManyComponentsError,
)


def apply_parser(f, x, cnt):
    """A helper function that applies a parser to a value.

    Args:
        f: The parser function to apply.
        x: The value to parse.
        cnt: The component count.

    Returns:
        The parsed value.

    Raises:
        InternalParseError: If the parser or count is not valid.
        MissingComponent: If the value is missing.
        ComponentParseError: If the value cannot be parsed.
    """
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
    """A helper class that checks if a value is equal to an expected value.

    This class raises a `ValueError` if the value is not equal to any
    of the expected values.
    """

    def __init__(self, *x) -> None:
        """Initializes the `check_eq` object.

        Args:
            *x: The expected values.
        """
        self.x: Sequence[Any] = x

    def __call__(self, y: Any) -> Any:
        """Checks if the given value is equal to one of the expected values.

        Args:
            y: The value to check.

        Returns:
            The value if it is equal to one of the expected values.

        Raises:
            ValueError: If the value is not equal to any of the
                expected values.
        """
        if y not in self.x:
            raise ValueError(f"Expected: {self.x!r}, got: {y!r}")
        return y

    def __repr__(self) -> str:
        """Returns a string representation of the `check_eq` object."""
        return f"<{self.__class__.__module__}.{self.__class__.__name__} {self.x!r}>"

    def __str__(self) -> str:
        """Returns a string representation of the expected values."""
        return f"{self.x!r}"


class check_type:
    """A helper class that checks if a value can be converted to a type.

    This class raises a `ValueError` if the value cannot be converted
    to any of the expected types.
    """

    def __init__(self, *x) -> None:
        """Initializes the `check_type` object.

        Args:
            *x: The expected types.
        """
        self.x: Sequence[Any] = x

    def __call__(self, y: Any) -> Any:
        """Checks if the given value can be converted to one of the expected types.

        Args:
            y: The value to check.

        Returns:
            The converted value.

        Raises:
            ValueError: If the value cannot be converted to any of the
                expected types.
        """
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
        """Returns a string representation of the `check_type` object."""
        return f"<{self.__class__.__module__}.{self.__class__.__name__} {self.x!r}>"

    def __str__(self) -> str:
        """Returns a string representation of the expected types."""
        return f"convertible to {self.x!r}"


@final
class RequestReviewID:
    """Represents an OBS Request Review ID."""

    def __init__(self, rrid: str) -> None:
        """Initializes the `RequestReviewID` object.

        Args:
            rrid: The fully qualified Request Review ID string.
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
        """Returns a string representation of the `RequestReviewID` object."""
        return f"{self.project}:{self.kind}:{self.maintenance_id}:{self.review_id}"

    def __repr__(self) -> str:
        """Returns a string representation of the `RequestReviewID` object."""
        return f"<RRID - {self.project}:{self.kind}:{self.maintenance_id}:{self.review_id}>"

    def __hash__(self) -> int:
        """Returns the hash of the `RequestReviewID` object."""
        return hash(str(self))

    def __eq__(self, other: object) -> bool:
        """Checks if two `RequestReviewID` objects are equal."""
        return str(self) == str(other)

    def __ne__(self, other: object) -> bool:
        """Checks if two `RequestReviewID` objects are not equal."""
        return not self.__eq__(other)
