"""Defines the commands for preparing packages on different systems."""

from string import Template

from ...support.messages import MissingPreparerError
from ...support.misc import DictWithInjections


def zypper_prepare(force: bool = False, testing: bool = False) -> dict[str, Template]:
    """Returns a dictionary of command templates for preparing with zypper.

    Args:
        force: Whether to force the resolution of dependencies.
        testing: Whether to include testing repositories.

    Returns:
        A dictionary of command templates.

    """
    parameter = "--force-resolution" if force else ""

    return {
        "command": Template(f"zypper -n in -y -l {parameter} $package"),
        "installed_only": Template(
            f"if $(rpm -q $package &>/dev/null); then zypper -n in -y -l {parameter} $package ; fi"
        ),
    }


def yum_prepare(force: bool = False, testing: bool = False) -> dict[str, Template]:
    """Returns a dictionary of command templates for preparing with yum.

    Args:
        force: Whether to force the resolution of dependencies.
        testing: Whether to include testing repositories.

    Returns:
        A dictionary of command templates.

    """
    # TODO: check adding repos to RH based distros and check how really it looks
    # also if needed expand detection and look into dnf packager
    parameter = "" if testing else "--disablerepo=*testing*"
    return {
        "command": Template(f"yum -y {parameter} install $package"),
        "installed_only": Template(
            f"rpm -q $package &>/dev/null && yum {parameter} -y install $package"
        ),
    }


def slm_prepare(force: bool = False, testing: bool = False) -> dict[str, Template]:
    """Returns a dictionary of command templates for preparing on slmicro.

    Args:
        force: Whether to force the resolution of dependencies.
        testing: Whether to include testing repositories.

    Returns:
        A dictionary of command templates.

    """
    parameter = "--force-resolution" if force else ""
    # Install in a SINGLE fresh snapshot (no --continue): the caller passes all
    # packages to one invocation, so they land together and a single reboot
    # activates them. The previous `--continue` + per-package "start operation"
    # canary chained a separate snapshot per package, which did not accumulate --
    # the packages "succeeded" but were missing after reboot. This mirrors the
    # working single-call slm_update patch path.
    return {
        "command": Template(f"transactional-update -n pkg in -l {parameter} $package"),
        "installed_only": Template(
            f"if $(rpm -q $package &>/dev/null); then transactional-update -n pkg in -l {parameter} $package ; fi"
        ),
        "reboot": Template("systemctl reboot"),
    }


#: A dictionary that maps system configurations to prepare commands.
preparer = DictWithInjections(
    {
        ("11", False): zypper_prepare,
        ("12", False): zypper_prepare,
        ("15", False): zypper_prepare,
        ("16", False): zypper_prepare,
        ("YUM", False): yum_prepare,
        ("slmicro", True): slm_prepare,
    },
    key_error=MissingPreparerError,
)
