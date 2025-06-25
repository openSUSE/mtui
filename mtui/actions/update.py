from string import Template

from ..messages import MissingUpdaterError
from ..utils import DictWithInjections


yum_update = {
    "command": Template(
        """
export LANG=
yum repolist
yum -y update $packages
"""
    )
}


zypper_update = {
    "command": Template(
        r"""
export LANG=
zypper -b lr -puU
zypper -n refresh
zypper -n patches | grep $repa
zypper -n patches | awk -F "|" '/$repa\>/ {{ print $2; }}' | while read p; do zypper -n in -l -y -t patch $$p; done
zypper -n patches | grep $repa
zypper -n lr | awk -F "|" '/$repa\>/ {{ print $2; }}' | while read r; do zypper -n rr $$r; done
"""
    )
}


updater = DictWithInjections(
    {
        ("YUM", False): yum_update,
        ("11", False): zypper_update,
        ("12", False): zypper_update,
        ("15", False): zypper_update,
    },
    key_error=MissingUpdaterError,
)
