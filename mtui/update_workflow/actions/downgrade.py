"""Defines the commands for downgrading packages on different systems."""

from string import Template

from ...support.messages import MissingDowngraderError
from ...support.misc import DictWithInjections


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

    cmd_template = "rpm -q $package &>/dev/null  && zypper -n in -C --force-resolution --oldpackage -y $package=$version"

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

    # Downgrade ALL packages in a single fresh snapshot (no --continue, no
    # separate init snapshot): ``$package`` carries the whole "name=version ..."
    # spec list so one transactional-update call lands them together and one
    # reboot activates them. The previous per-package ``-c ... -C`` chain plus a
    # ``transactional-update run true`` init snapshot opened a snapshot per
    # package, which did not accumulate -- the downgrade "succeeded" yet the
    # packages stayed at the test version after reboot (same failure mode as
    # prepare/newpackage).
    cmd_template = (
        "transactional-update -n pkg in --force-resolution --oldpackage -y $package"
    )
    reboot = "systemctl reboot"

    return {
        "list_command": Template(list_command_template),
        "command": Template(cmd_template),
        "reboot": Template(reboot),
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
