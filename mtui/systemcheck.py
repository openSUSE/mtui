import re

from paramiko import __version__ as paramiko_version  # type: ignore

from mtui import __version__ as mtui_version
from typing import Tuple


def detect_system() -> Tuple[str, str, str]:
    _distro = re.compile('NAME=["|](.*)["|]')
    _v_id = re.compile('VERSION_ID=["|](.*)["|]')
    try:
        with open("/etc/os-release", mode="r", encoding="utf-8") as f:
            for line in f:
                # TODO: python 3.8 walrus operator
                d = _distro.match(line)
                if d:
                    distro = d.group(1)
                    continue
                v = _v_id.match(line)
                if v:
                    verid = v.group(1)
                    continue
    except:
        verid = "None"
        distro = "Unknown"

    try:
        with open("/proc/version", mode="r", encoding="utf-8") as f:
            kernel = f.readline().split(" ")[2]
    except:
        kernel = "Unknown"

    return distro, verid, kernel


def system_info(distro: str, verid: str, kernel: str, user: str) -> str:
    string = f"## export MTUI:{mtui_version}, paramiko {paramiko_version} on {distro}-{verid} (kernel: {kernel}) by {user}\n"
    return string
