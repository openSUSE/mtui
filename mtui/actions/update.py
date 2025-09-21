"""Defines the commands for updating packages on different systems."""

from string import Template

from ..messages import MissingUpdaterError
from ..utils import DictWithInjections

#: A dictionary of command templates for updating packages using yum.
yum_update = {
    "command": Template(
        """
export LANG=
yum repolist
yum -y update $packages
"""
    )
}


#: A dictionary of command templates for updating packages using zypper.
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


#: A dictionary of command templates for updating packages on slmicro.
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

#: A dictionary that maps system configurations to update commands.
updater = DictWithInjections(
    {
        ("YUM", False): yum_update,
        ("11", False): zypper_update,
        ("12", False): zypper_update,
        ("15", False): zypper_update,
        ("16", False): zypper_update,
        ("slmicro", True): slm_update,
    },
    key_error=MissingUpdaterError,
)
