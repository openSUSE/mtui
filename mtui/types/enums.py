"""Enumerations for HTTP request methods and pull request assignment states."""

from enum import Enum, StrEnum, auto


class method(StrEnum):
    """An enumeration for HTTP request methods."""

    POST = auto()
    GET = auto()


class assignment(Enum):
    """An enumeration for the assignment state of a pull request."""

    ASSIGNED_USER = auto()
    UNASSIGNED = auto()
    ASSIGNED_OTHER = auto()
