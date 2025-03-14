from string import Template

from ..messages import MissingInstallerError
from ..utils import DictWithInjections


zypper_install = {"command": Template("zypper -n in -y -l $packages")}
yum_install = {"command": Template("yum -y install $packages")}


installer = DictWithInjections(
    {
        "11": zypper_install,
        "12": zypper_install,
        "15": zypper_install,
        "YUM": yum_install,
    },
    key_error=MissingInstallerError,
)
