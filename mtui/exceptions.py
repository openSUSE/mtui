"""Custom exceptions used throughout the mtui application."""

from argparse import ArgumentTypeError
from collections.abc import Sequence
from typing import Any

from .types import assignment


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
        super().__init__("Internal error: f: {0!r} cnt: {1!r}".format(f, cnt))


class MissingComponent(RequestReviewIDParseError):
    """Raised when a component of an OBS Request Review ID is missing."""

    def __init__(self, index, expected) -> None:
        """Initializes the exception.

        Args:
            index: The index of the missing component.
            expected: The expected component.
        """
        super().__init__(
            "Missing {0}. component. Expected: {1}".format(index, expected)
        )


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
            "Failed to parse {0}. component. Expected {1}. Got: {2!r}".format(
                index, expected, got
            )
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
        return "{!s}: {!s}".format(self.host, self.reason)


class GiteaError(BaseException):
    """Base exception for Gitea-related errors."""

    pass


class MissingGiteaToken(GiteaError):
    """Raised when a Gitea token is missing."""

    pass


class FailedGiteaCall(GiteaError):
    """Raised when a call to the Gitea API fails."""

    pass


class GiteaNoReview(GiteaError):
    """Raised when a Gitea pull request has no review."""

    pass


class InvalidGiteaHash(GiteaError):
    """Raised when Gitea has different hash than testreport metadata"""

    def __init__(self, rrid):
        self.id = rrid

    def __str__(self) -> str:
        return f"Testreport for {self.id} has different hash than GiteaPR"


class GiteaAssignInvalid(GiteaError):
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
