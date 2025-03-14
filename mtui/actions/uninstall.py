from string import Template

from ..messages import MissingUninstallerError
from ..utils import DictWithInjections


zypper_uninstall = {"command": Template("zypper -n rm $packages")}
yum_uninstall = {"command": Template("yum -y remove $packages")}


uninstaller = DictWithInjections(
    {
        "11": zypper_uninstall,
        "12": zypper_uninstall,
        "15": zypper_uninstall,
        "YUM": yum_uninstall,
    },
    key_error=MissingUninstallerError,
)
