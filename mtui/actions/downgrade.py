from string import Template

from ..messages import MissingDowngraderError
from ..utils import DictWithInjections


def zypper() -> dict[str, Template]:
    list_command_template = r"""
for p in $packages; do \
zypper -n se -s --match-exact -t package $$p; \
done \
| grep -v "(System" \
| grep ^[iv] \
| sed "s, ,,g" \
| awk -F "|" '{{ print $2,"=",$4 }}'
"""

    cmd_template = "rpm -q $package &>/dev/null  && zypper -n in -C --force-resolution -y $package=$version"

    return {
        "list_command": Template(list_command_template),
        "command": Template(cmd_template),
    }


yum = {"command": Template("yum -y downgrade $package")}


downgrader = DictWithInjections(
    {"11": zypper(), "12": zypper(), "15": zypper(), "YUM": yum},
    key_error=MissingDowngraderError,
)
