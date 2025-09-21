"""A class for comparing RPM version strings."""

from typing import final

import rpm  # type: ignore


@final
class RPMVersion:
    """Holds an RPM version-release string for version comparison.

    This class is used for RPM version arithmetic, such as comparing
    if a specific RPM version is lower or higher than another one.
    """

    _arch_suffixes = [
        "noarch",
        "x86_64",
        "s390x",
        "ppc64le",
        "aarch64",
        "ia64",
        "ppc64",
    ]
    """
    :param _arch_suffixes: arch suffixes we get in addition to version on sle12
    """

    def __init__(self, ver: str) -> None:
        """Initializes the `RPMVersion` object.

        Args:
            ver: The version string to parse.
        """
        if not ver:
            raise ValueError

        for x in self._arch_suffixes:
            ver = ver.replace("." + x, "")

        if "-" in ver:
            # split rpm version string into version and release string
            (self.ver, self.rel) = ver.rsplit("-")
        else:
            self.ver = ver
            self.rel = "0"

    def __lt__(self, other: "RPMVersion") -> bool:
        """Checks if this version is less than another."""
        return (
            rpm.labelCompare(("1", self.ver, self.rel), ("1", other.ver, other.rel)) < 0
        )

    def __gt__(self, other: "RPMVersion") -> bool:
        """Checks if this version is greater than another."""
        return (
            rpm.labelCompare(("1", self.ver, self.rel), ("1", other.ver, other.rel)) > 0
        )

    def __eq__(self, other: "RPMVersion") -> bool:
        """Checks if this version is equal to another."""
        return (
            rpm.labelCompare(("1", self.ver, self.rel), ("1", other.ver, other.rel))
            == 0
        )

    def __le__(self, other: "RPMVersion") -> bool:
        """Checks if this version is less than or equal to another."""
        return (
            rpm.labelCompare(("1", self.ver, self.rel), ("1", other.ver, other.rel))
            <= 0
        )

    def __ge__(self, other: "RPMVersion") -> bool:
        """Checks if this version is greater than or equal to another."""
        return (
            rpm.labelCompare(("1", self.ver, self.rel), ("1", other.ver, other.rel))
            >= 0
        )

    def __ne__(self, other: "RPMVersion") -> bool:
        """Checks if this version is not equal to another."""
        return (
            rpm.labelCompare(("1", self.ver, self.rel), ("1", other.ver, other.rel))
            != 0
        )

    def __str__(self) -> str:
        """Returns a string representation of the version."""
        s = str(self.ver)
        if self.rel != "0":
            s += "-" + str(self.rel)
        return s

    def __repr__(self) -> str:
        """Returns a string representation of the `RPMVersion` object."""
        return f"<RPMVersion: {self}>"
