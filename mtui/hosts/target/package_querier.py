"""Per-target package-version querier.

This module is the home of the rpm-vs-dpkg branch logic and the
output-parsing loop that turn a list of package names into a mapping of
``name -> RPMVersion | None``. The logic used to live as
:meth:`Target.query_package_versions`; it has nothing to do with the
connection lifecycle on ``Target`` and is the only consumer of the
``ubuntu``-vs-everything-else system distinction, so it earns its own
collaborator.

``Target.query_package_versions`` is kept as a thin delegate so all
existing callers (``HostsGroup.query_versions``,
``Target.query_versions``) continue to work without churn.
"""

import re
from logging import getLogger
from typing import TYPE_CHECKING, final

from ...types.rpmver import RPMVersion

if TYPE_CHECKING:
    from .target import Target

logger = getLogger("mtui.target.package_querier")

# Matches the rpm-style "package X is not installed" line. The
# corresponding dpkg output ("no packages found matching X") is reported
# on stderr and does not appear in the line loop, so the matcher does
# not need to handle it.
_NOT_INSTALLED_RE = re.compile(r"package (.*) is not installed")


@final
class PackageQuerier:
    """Adapter that runs ``rpm -q`` / ``dpkg-query`` and parses the output."""

    def __init__(self, target: "Target") -> None:
        """Bind the querier to ``target``.

        ``target`` is used both as the call sink (``run`` / ``lastout``)
        and as the source of system information (rpm vs dpkg branch).
        """
        self.target = target

    def versions(self, packages: list[str]) -> dict[str, RPMVersion | None]:
        """Query the installed versions of ``packages``.

        Returns a mapping from package name to :class:`RPMVersion`, or
        to ``None`` when the package is not installed. Duplicate lines
        for the same package collapse to the highest version.
        """
        t = self.target
        joined = " ".join(packages)
        if t.system.get_base().name != "ubuntu":
            t.run(
                f'rpm -q --queryformat "%{{Name}} %{{Version}}-%{{Release}}\n" {joined}'
            )
        else:
            t.run(f"dpkg-query -W -f='${{package}} ${{version}}\n' {joined}")

        pkgs: dict[str, RPMVersion | None] = {}
        for line in t.lastout().splitlines():
            if match := _NOT_INSTALLED_RE.search(line):
                pkgs[match.group(1)] = None
                continue
            name, ver = line.split()
            new_ver = RPMVersion(ver)
            if name in pkgs:
                existing = pkgs[name]
                pkgs[name] = max(existing, new_ver) if existing is not None else new_ver
            else:
                pkgs[name] = new_ver
        return pkgs
