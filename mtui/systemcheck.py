import re

from paramiko import __version__ as paramiko_version

from mtui import __version__ as mtui_version


def detect_system():
    _distro = re.compile('NAME=["|](.*)["|]')
    _v_id = re.compile('VERSION_ID=["|](.*)["|]')
    try:
        with open("/etc/os-release", mode="r", encoding="utf-8") as f:
            for line in f:
                if _distro.match(line):
                    distro = _distro.match(line).group(1)
                    continue
                if _v_id.match(line):
                    verid = _v_id.match(line).group(1)
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


def system_info(distro, verid, kernel, user):
    string = f"## export MTUI:{mtui_version}, paramiko {paramiko_version} on {distro}-{verid} (kernel: {kernel}) by {user}\n"
    return string
