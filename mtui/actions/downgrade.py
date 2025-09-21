"""Defines the commands for downgrading packages on different systems."""

from string import Template

from ..messages import MissingDowngraderError
from ..utils import DictWithInjections


def zypper() -> dict[str, Template]:
    """Returns a dictionary of command templates for downgrading with zypper.

    Returns:
        A dictionary of command templates.
    """
    list_command_template = r"""
for p in $packages; do \
zypper -n se -s --match-exact -t package $$p; \
done \
| grep -v "(System" \
| grep ^[iv] \
| sed "s, ,,g" \
| awk -F "|" '{{ print $2,"=",$4 }}'
"""

    cmd_template = "rpm -q $package &>/dev/null  && zypper -n in -C --force-resolution -y $package=$version"

    return {
        "list_command": Template(list_command_template),
        "command": Template(cmd_template),
    }


def slmicro() -> dict[str, Template]:
    """Returns a dictionary of command templates for downgrading on slmicro.

    Returns:
        A dictionary of command templates.
    """
    list_command_template = r"""
for p in $packages; do \
zypper -n se -s --match-exact -t package $$p; \
done \
| grep -v "(System" \
| grep ^[iv] \
| sed "s, ,,g" \
| awk -F "|" '{{ print $2,"=",$4 }}'
"""

    cmd_template = "rpm -q $package &>/dev/null && transactional-update -c pkg in -C --force-resolution -y $package=$version"
    reboot = "systemctl reboot"
    init_snapshot = "transactional-update run true"

    return {
        "list_command": Template(list_command_template),
        "command": Template(cmd_template),
        "reboot": Template(reboot),
        "init_snapshot": Template(init_snapshot),
    }


#: A dictionary of command templates for downgrading packages using yum.
yum = {"command": Template("yum -y downgrade $package")}


#: A dictionary that maps system configurations to downgrade commands.
downgrader = DictWithInjections(
    {
        ("11", False): zypper(),
        ("12", False): zypper(),
        ("15", False): zypper(),
        ("16", False): zypper(),
        ("YUM", False): yum,
        ("slmicro", True): slmicro(),
    },
    key_error=MissingDowngraderError,
)
