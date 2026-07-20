"""Defines the commands for downgrading packages on different systems."""

from string import Template

from ...support.messages import MissingDowngraderError
from ...support.misc import DictWithInjections


def zypper() -> dict[str, Template]:
    """Returns a dictionary of command templates for downgrading with zypper.

    Returns:
        A dictionary of command templates.

    """
    # ONE zypper invocation for the whole package list. A per-package ``for``
    # loop loads the repo metadata once per package and, piped through a
    # block-buffered awk (no PTY), emits nothing until the last iteration --
    # on a slow host a long package list blows the SSH no-output timeout
    # (``connection_timeout``, default 300s) and the probe dies with no
    # versions resolved.
    list_command_template = r"""
zypper -n se -s --match-exact -t package $packages \
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
    # One invocation for the whole list -- see ``zypper()`` above for why a
    # per-package loop is a timeout trap on slow hosts.
    list_command_template = r"""
zypper -n se -s --match-exact -t package $packages \
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
