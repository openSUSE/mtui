from string import Template

from ..messages import MissingPreparerError
from ..utils import DictWithInjections


def zypper_prepare(force: bool = False, testing: bool = False) -> dict[str, Template]:
    parameter = "--force-resolution" if force else ""

    return {
        "command": Template(f"zypper -n in -y -l {parameter} $package"),
        "installed_only": Template(
            f"if $(rpm -q $package &>/dev/null); then zypper -n -y -l {parameter} $package ; fi"
        ),
    }


def yum_prepare(force: bool = False, testing: bool = False) -> dict[str, Template]:
    # TODO: check adding repos to RH based distros and check how really it looks
    # also if needed expand detection and look into dnf packager
    parameter = "" if testing else "--disablerepo=*testing*"
    return {
        "command": Template(f"yum -y {parameter} install $package"),
        "installed_only": Template(
            f"rpm -q $package &>/dev/null && yum {parameter} -y $package"
        ),
    }


def slm_prepare(force: bool = False, testing: bool = False) -> dict[str, Template]:
    parameter = "--force-resolution" if force else ""
    return {
        "command": Template(f"transactional-update -n pkg in -l {parameter} $package"),
        "installed_only": Template(
            f"if $(rpm -q $package &>/dev/null); then transactional-update -n pkg in -l {parameter} $package ; fi"
        ),
        "reboot": Template("systemctl reboot"),
    }


preparer = DictWithInjections(
    {
        ("11", False): zypper_prepare,
        ("12", False): zypper_prepare,
        ("15", False): zypper_prepare,
        ("YUM", False): yum_prepare,
        ("slmicro", True): slm_prepare,
    },
    key_error=MissingPreparerError,
)
