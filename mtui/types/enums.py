"""Enumerations used across mtui.

Includes HTTP request methods, pull-request assignment states, and the
domain enums introduced by the Phase 5b/C9 refactor:

* :class:`TargetState` -- per-host execution state (enabled/dryrun/disabled).
* :class:`ExecutionMode` -- whether a host runs commands in parallel with
  the rest of its group or holds the group in a serial barrier.
* :class:`RequestKind` -- kind component of an OBS Request Review ID.
"""

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


class TargetState(StrEnum):
    """Per-host execution state.

    StrEnum so that legacy string comparisons such as
    ``target.state == "enabled"`` continue to work byte-identically with
    the existing CLI, config, and test surface.
    """

    ENABLED = "enabled"
    DRYRUN = "dryrun"
    DISABLED = "disabled"


class ExecutionMode(Enum):
    """Whether a host runs commands in parallel or under a serial barrier.

    Plain :class:`Enum` (not :class:`StrEnum`): this concept was previously
    encoded as ``Target.exclusive: bool`` so there is no legacy string
    surface to preserve.
    """

    PARALLEL = "parallel"
    SERIAL = "serial"


class RequestKind(Enum):
    """Kind component of an OBS Request Review ID.

    Plain :class:`Enum`: production code branches on the kind in seven
    places and migrating those to enum members catches typos that a
    string field would silently swallow. The wire form is preserved via
    :attr:`Enum.value` (e.g. ``RequestKind.SLFO.value == "SLFO"``).
    """

    SLFO = "SLFO"
    MAINTENANCE = "Maintenance"
    PI = "PI"

    @classmethod
    def from_token(cls, raw: str) -> "RequestKind":
        """Parse the short or long form of a request kind.

        Accepts both the single-letter aliases (``S``/``M``/``P``) used by
        users on the command line and the canonical long forms
        (``SLFO``/``Maintenance``/``PI``) used in the wire format.

        Raises:
            ValueError: if ``raw`` is not a recognised kind.

        """
        aliases = {
            "S": cls.SLFO,
            "SLFO": cls.SLFO,
            "M": cls.MAINTENANCE,
            "Maintenance": cls.MAINTENANCE,
            "P": cls.PI,
            "PI": cls.PI,
        }
        try:
            return aliases[raw]
        except KeyError:
            raise ValueError(f"unknown request kind: {raw!r}") from None
