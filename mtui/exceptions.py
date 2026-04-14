"""Custom exceptions used throughout the mtui application."""

from argparse import ArgumentTypeError
from collections.abc import Sequence
from typing import TYPE_CHECKING, Any

from .types import assignment

if TYPE_CHECKING:
    from .types import RequestReviewID


class RequestReviewIDParseError(ValueError, ArgumentTypeError):
    """Base exception for errors parsing OBS Request Review IDs."""

    # Note: need to inherit ArgumentTypeError so the custom exception
    # messages get shown to the users properly
    # by L{argparse.ArgumentParser._get_value}

    def __init__(self, message: str) -> None:
        """Initializes the exception.

        Args:
            message: The error message.

        """
        super().__init__("OBS Request Review ID: " + message)


class TooManyComponentsError(RequestReviewIDParseError):
    """Raised when an OBS Request Review ID has too many components."""

    def __init__(self, limit: int) -> None:
        """Initializes the exception.

        Args:
            limit: The maximum number of components allowed.

        """
        super().__init__(f"Too many components (> {limit})")

    @classmethod
    def raise_if(cls, xs: Sequence[Any], limit: int) -> None:
        """Raises the exception if the sequence has too many components.

        Args:
            xs: The sequence to check.
            limit: The maximum number of components allowed.

        """
        if len(xs) > limit:
            raise cls(limit)


class InternalParseError(RequestReviewIDParseError):
    """Raised for internal errors during parsing."""

    def __init__(self, f, cnt) -> None:
        """Initializes the exception.

        Args:
            f: The function where the error occurred.
            cnt: The context of the error.

        """
        super().__init__(f"Internal error: f: {f!r} cnt: {cnt!r}")


class MissingComponentError(RequestReviewIDParseError):
    """Raised when a component of an OBS Request Review ID is missing."""

    def __init__(self, index, expected) -> None:
        """Initializes the exception.

        Args:
            index: The index of the missing component.
            expected: The expected component.

        """
        super().__init__(f"Missing {index}. component. Expected: {expected}")


class ComponentParseError(RequestReviewIDParseError):
    """Raised when a component of an OBS Request Review ID cannot be parsed."""

    def __init__(self, index, expected, got) -> None:
        """Initializes the exception.

        Args:
            index: The index of the component that failed to parse.
            expected: The expected component.
            got: The component that was received.

        """
        super().__init__(
            f"Failed to parse {index}. component. Expected {expected}. Got: {got!r}"
        )


class UpdateError(Exception):
    """Base exception for errors that occur during an update."""

    def __init__(self, reason: str, host: str | None = None) -> None:
        """Initializes the exception.

        Args:
            reason: The reason for the update error.
            host: The host where the error occurred.

        """
        self.reason: str = reason
        self.host: str | None = host

    def __str__(self) -> str:
        """Returns the string representation of the exception."""
        if self.host is None:
            return self.reason
        return f"{self.host!s}: {self.reason!s}"


class GiteaError(BaseException):
    """Base exception for Gitea-related errors."""


class MissingGiteaTokenError(GiteaError):
    """Raised when a Gitea token is missing."""


class FailedGiteaCallError(GiteaError):
    """Raised when a call to the Gitea API fails."""


class GiteaNoReviewError(GiteaError):
    """Raised when a Gitea pull request has no review."""


class InvalidGiteaHashError(GiteaError):
    """Raised when Gitea has different hash than testreport metadata."""

    def __init__(self, rrid: "RequestReviewID | str", old: str, new: str):
        self.id = rrid
        self.old = old
        self.new = new

    def __str__(self) -> str:
        return f"Testreport for {self.id} has different hash than GiteaPR. Testreport: {self.old} - repo {self.new}"


class GiteaAssignInvalidError(GiteaError):
    """Raised when there is an issue with Gitea pull request assignment."""

    def __init__(self, assign_status: assignment, user: str):
        """Initializes the exception.

        Args:
            assign_status: The assignment status.
            user: The user involved in the assignment.

        """
        self.assign_status = assign_status
        self.user = user

    def __str__(self) -> str:
        """Returns the string representation of the exception."""
        if self.assign_status == assignment.ASSIGNED_OTHER:
            return f"Gitea PR has assigned different user than {self.user}"
        if self.assign_status == assignment.ASSIGNED_USER:
            return f"Gitea PR has already assigned user: {self.user}"
        return f"User {self.user} isnt assigned to Gitea PR"
