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
zypper -n lr -puU
zypper -n refresh
zypper -n patches | grep $repa
zypper -n in -l -y -t patch $(zypper -n patches | awk -F "|" '/$repa\>/ {{ print $2; }}')
zypper -n patches | grep $repa
zypper -n lr | awk -F "|" '/$repa\>/ {{ print $2; }}' | while read r; do zypper -n rr $$r; done
"""
    )
}


slm_update = {
    "command": Template(
        r"""
export LANG=
zypper -n lr -puU
zypper -n patches | grep $repa
transactional-update -n pkg in -l -y -t patch $(zypper -n patches | awk -F "|" '/$repa\>/ {{ print $2; }}')
zypper -n patches | grep $repa
zypper -n lr | awk -F "|" '/$repa\>/ {{ print $2; }}' | while read r; do zypper -n rr $$r; done
"""
    ),
    "reboot": Template("systemctl reboot"),
}

updater = DictWithInjections(
    {
        ("YUM", False): yum_update,
        ("11", False): zypper_update,
        ("12", False): zypper_update,
        ("15", False): zypper_update,
        ("slmicro", True): slm_update,
    },
    key_error=MissingUpdaterError,
)
