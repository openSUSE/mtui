from enum import Enum, StrEnum, auto


class method(StrEnum):
    """Enumeration for HTTP request methods."""

    POST = auto()
    GET = auto()
    PATCH = auto()
    DELETE = auto()


class assignment(Enum):
    """Enumeration for the assignment state of a PR for a specific user."""

    ASSIGNED_USER = auto()
    UNASSIGNED = auto()
    ASSIGNED_OTHER = auto()
