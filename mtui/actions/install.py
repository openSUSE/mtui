"""Defines the commands for installing packages on different systems."""

from string import Template

from ..messages import MissingInstallerError
from ..utils import DictWithInjections


#: A dictionary of command templates for installing packages using zypper.
zypper_install = {"command": Template("zypper -n in -y -l $packages")}

#: A dictionary of command templates for installing packages using yum.
yum_install = {"command": Template("yum -y install $packages")}

#: A dictionary of command templates for installing packages on slmicro.
slmicro_install = {
    "command": Template("transactional-update -n pkg install $packages"),
    "reboot": Template("systemctl reboot"),
}


#: A dictionary that maps system configurations to install commands.
installer = DictWithInjections(
    {
        ("11", False): zypper_install,
        ("12", False): zypper_install,
        ("15", False): zypper_install,
        ("16", False): zypper_install,
        ("YUM", False): yum_install,
        ("slmicro", True): slmicro_install,
    },
    key_error=MissingInstallerError,
)
