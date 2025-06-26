from string import Template

from ..messages import MissingUninstallerError
from ..utils import DictWithInjections


zypper_uninstall = {"command": Template("zypper -n rm $packages")}
yum_uninstall = {"command": Template("yum -y remove $packages")}
slmicro_uninstall = {
    "command": Template("transactional-update -n pkg remove $packages"),
    "reboot": Template("systemctl reboot"),
}


uninstaller = DictWithInjections(
    {
        ("11", False): zypper_uninstall,
        ("12", False): zypper_uninstall,
        ("15", False): zypper_uninstall,
        ("YUM", False): yum_uninstall,
        ("slmicro", True): slmicro_uninstall,
    },
    key_error=MissingUninstallerError,
)
