from string import Template

from ..messages import MissingInstallerError
from ..utils import DictWithInjections


zypper_install = {"command": Template("zypper -n in -y -l $packages")}
yum_install = {"command": Template("yum -y install $packages")}
slmicro_install = {
    "command": Template("transactional-update -n pkg install $packages"),
    "reboot": Template("systemctl reboot"),
}


installer = DictWithInjections(
    {
        ("11", False): zypper_install,
        ("12", False): zypper_install,
        ("15", False): zypper_install,
        ("YUM", False): yum_install,
        ("slmicro", True): slmicro_install,
    },
    key_error=MissingInstallerError,
)
