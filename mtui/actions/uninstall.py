"""Defines the commands for uninstalling packages on different systems."""

from string import Template

from ..messages import MissingUninstallerError
from ..utils import DictWithInjections


#: A dictionary of command templates for uninstalling packages using zypper.
zypper_uninstall = {"command": Template("zypper -n rm $packages")}

#: A dictionary of command templates for uninstalling packages using yum.
yum_uninstall = {"command": Template("yum -y remove $packages")}

#: A dictionary of command templates for uninstalling packages on slmicro.
slmicro_uninstall = {
    "command": Template("transactional-update -n pkg remove $packages"),
    "reboot": Template("systemctl reboot"),
}


#: A dictionary that maps system configurations to uninstall commands.
uninstaller = DictWithInjections(
    {
        ("11", False): zypper_uninstall,
        ("12", False): zypper_uninstall,
        ("15", False): zypper_uninstall,
        ("16", False): zypper_uninstall,
        ("YUM", False): yum_uninstall,
        ("slmicro", True): slmicro_uninstall,
    },
    key_error=MissingUninstallerError,
)
