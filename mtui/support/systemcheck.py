"""Functions for gathering system information."""

import re

from paramiko import __version__ as paramiko_version

from .. import __version__ as mtui_version


def detect_system() -> tuple[str, str, str]:
    """Detects the operating system, version, and kernel.

    Returns:
        A tuple containing the distribution, version ID, and kernel version.

    """
    # os-release values may be double-quoted, single-quoted or bare
    # (NAME=Fedora and VERSION_ID=15.6 are spec-legal). The quoted
    # alternatives use a greedy ``.*`` with no trailing anchor -- same as
    # the old mandatory-quote regex -- so quoted parsing (including
    # spec-legal backslash-escaped quotes embedded in the value) stays
    # byte-identical to before. Only the bare alternative is anchored
    # with ``\s*$``: unlike a quote character, there is nothing in the
    # bare value itself to mark where it ends, so without the anchor a
    # multi-word or otherwise invalid unquoted value (e.g. a stray
    # ``NAME=SUSE Linux``, which the spec forbids) would silently
    # truncate at the first space instead of failing to match.
    _value = r"""(?:"(.*)"|'(.*)'|([^\s"']*)\s*$)"""
    _distro = re.compile("NAME=" + _value)
    _v_id = re.compile("VERSION_ID=" + _value)
    distro = ""
    verid = ""
    kernel = ""

    try:
        with open("/etc/os-release", encoding="utf-8") as f:
            for line in f:
                if d := _distro.match(line):
                    distro = d.group(1) or d.group(2) or d.group(3) or ""
                    continue
                if v := _v_id.match(line):
                    verid = v.group(1) or v.group(2) or v.group(3) or ""
                    continue
    except Exception:
        verid = "None"
        distro = "Unknown"

    try:
        with open("/proc/version", encoding="utf-8") as f:
            kernel = f.readline().split(" ")[2]
    except Exception:
        kernel = "Unknown"

    return distro, verid, kernel


def system_info(
    distro: str, verid: str, kernel: str, user: str, prefix: str = "## export"
) -> str:
    """Formats system information into a string.

    Args:
        distro: The operating system distribution.
        verid: The version ID of the distribution.
        kernel: The kernel version.
        user: The current user.
        prefix: Leading text before the version details. Defaults to
            ``"## export"`` (the testreport export footer); the ``commit``
            command reuses this with ``"committed from"``.

    Returns:
        A formatted string containing the system information.

    """
    string = f"{prefix} MTUI:{mtui_version}, paramiko {paramiko_version} on {distro}-{verid} (kernel: {kernel}) by {user}\n"
    return string
