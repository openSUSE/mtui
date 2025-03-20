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


def yum_prepare(force=False, testing=False) -> dict[str, Template]:
    # TODO: check adding repos to RH based distros and check how really it looks
    # also if needed expand detection and look into dnf packager
    parameter = "" if testing else "--disablerepo=*testing*"
    return {
        "command": Template(f"yum -y {parameter} install $package"),
        "installed_only": Template(
            f"rpm -q $package &>/dev/null && yum {parameter} -y $package"
        ),
    }


preparer = DictWithInjections(
    {
        "11": zypper_prepare,
        "12": zypper_prepare,
        "15": zypper_prepare,
        "YUM": yum_prepare,
    },
    key_error=MissingPreparerError,
)
