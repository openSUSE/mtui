"""Functions for gathering system information."""

import re

from paramiko import __version__ as paramiko_version  # type: ignore

from mtui import __version__ as mtui_version


def detect_system() -> tuple[str, str, str]:
    """Detects the operating system, version, and kernel.

    Returns:
        A tuple containing the distribution, version ID, and kernel version.
    """
    _distro = re.compile(r'NAME=["|](.*)["|]')
    _v_id = re.compile(r'VERSION_ID=["|](.*)["|]')
    distro = ""
    verid = ""
    kernel = ""

    try:
        with open("/etc/os-release", mode="r", encoding="utf-8") as f:
            for line in f:
                if d := _distro.match(line):
                    distro = d.group(1)
                    continue
                if v := _v_id.match(line):
                    verid = v.group(1)
                    continue
    except Exception:
        verid = "None"
        distro = "Unknown"

    try:
        with open("/proc/version", mode="r", encoding="utf-8") as f:
            kernel = f.readline().split(" ")[2]
    except Exception:
        kernel = "Unknown"

    return distro, verid, kernel


def system_info(distro: str, verid: str, kernel: str, user: str) -> str:
    """Formats system information into a string.

    Args:
        distro: The operating system distribution.
        verid: The version ID of the distribution.
        kernel: The kernel version.
        user: The current user.

    Returns:
        A formatted string containing the system information.
    """
    string = f"## export MTUI:{mtui_version}, paramiko {paramiko_version} on {distro}-{verid} (kernel: {kernel}) by {user}\n"
    return string
