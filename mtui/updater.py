#
# update and software stack management
#

from logging import getLogger

from .messages import (
    MissingDowngraderError,
    MissingInstallerError,
    MissingPreparerError,
    MissingUninstallerError,
    MissingUpdaterError,
)
from .target.actions import UpdateError
from .target.downgrade import Downgrade
from .target.install import Install
from .target.prepare import Prepare
from .target.update import Update
from .utils import DictWithInjections

logger = getLogger("mtui.updater")


class ZypperUpdate(Update):
    def check(self, target, stdin, stdout, stderr, exitcode):
        if "Error:" in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("RPM Error", target.hostname)
        if "The following package is not supported by its vendor" in stdout:
            logger.critical(
                "{!s}: package support is uncertain:".format(target.hostname)
            )
            marker = "The following package is not supported by its vendor:\n"
            start = stdout.find(marker)
            end = stdout.find("\n\n", start)
            print(stdout[start:end])


class ZypperOBSUpdate(ZypperUpdate):
    def __init__(self, *a, **kw):
        super(ZypperOBSUpdate, self).__init__(*a, **kw)
        repat = ":p={:d}"
        repo = repat.format(self.testreport.rrid.maintenance_id)

        self.commands = [
            r"""export LANG=""",
            r"""zypper -n lr -puU""",
            r"""zypper -n refresh""",
            r"""zypper -n patches | grep {!s}""".format(repo),
            r"""zypper -n patches | awk -F "|" '/{!s}\>/ {{ print $2; }}' | while read p; do zypper -n install -l -y -t patch $p; done""".format(
                repo
            ),
            r"""zypper -n patches | grep {!s}""".format(repo),
            r"""zypper -n lr | awk -F "|" '/{!s}\>/ {{ print $2; }}' | while read r; do zypper rr $r; done""".format(
                repo
            ),
        ]


class RedHatUpdate(Update):
    def __init__(self, *a, **kw):
        super(RedHatUpdate, self).__init__(*a, **kw)

        self.commands = [
            "export LANG=",
            "yum repolist",
            "yum -y update {!s}".format(" ".join(self.packages)),
        ]


class CaaSPUpdate(Update):
    def __init__(self, *a, **kw):
        super(CaaSPUpdate, self).__init__(*a, **kw)
        self.type = "transactional"
        self.commands = ["export LANG=", "transactional-update cleanup dup"]

    def check(self, target, stdin, stdout, stderr, exitcode):
        if "Error:" in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("Transactional Update Error", target.hostname)


Updater = DictWithInjections(
    {
        "15": ZypperOBSUpdate,
        "12": ZypperOBSUpdate,
        "11": ZypperOBSUpdate,
        "YUM": RedHatUpdate,
        "CAASP": CaaSPUpdate,
    },
    key_error=MissingUpdaterError,
)


class ZypperPrepare(Prepare):
    def __init__(self, *a, **kw):
        super(ZypperPrepare, self).__init__(*a, **kw)

        parameter = ""
        commands = []

        if self.force:
            parameter = "--force-resolution"

        for package in self.packages:
            if "branding-upstream" in package:
                continue
            if self.installed_only:
                commands.append(
                    "rpm -q {!s} &>/dev/null && zypper -n in -y -l {!s} {!s}".format(
                        package, parameter, package
                    )
                )
            else:
                commands.append(
                    "zypper -n in -y -l {!s} {!s}".format(parameter, package)
                )

        self.commands = commands

    def check(self, target, stdin, stdout, stderr, exitcode):
        if "Error:" in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("RPM Error", target.hostname)


class RedHatPrepare(Prepare):
    def __init__(self, *a, **kw):
        super(RedHatPrepare, self).__init__(*a, **kw)

        parameter = ""
        commands = []

        if not self.testing:
            parameter = "--disablerepo=*testing*"

        for package in self.packages:
            if self.installed_only:
                commands.append(
                    "rpm -q {!s} &>/dev/null && yum -y {!s} install {!s}".format(
                        package, parameter, package
                    )
                )
            else:
                commands.append("yum -y {!s} install {!s}".format(parameter, package))

        self.commands = commands


class CaaSPPrepare(Prepare):
    def run(self):
        pass


Preparer = DictWithInjections(
    {
        "15": ZypperPrepare,
        "12": ZypperPrepare,
        "11": ZypperPrepare,
        "YUM": RedHatPrepare,
        "CAASP": CaaSPPrepare,
    },
    key_error=MissingPreparerError,
)


class ZypperDowngrade(Downgrade):
    def __init__(self, *a, **kw):
        super(ZypperDowngrade, self).__init__(*a, **kw)

        self.list_command = r"""
            for p in {!s}; do \
              zypper -n se -s --match-exact -t package $p; \
            done \
            | grep -v "(System" \
            | grep ^[iv] \
            | sed "s, ,,g" \
            | awk -F "|" '{{ print $2,"=",$4 }}'
        """.format(
            " ".join(self.packages)
        )
        self.install_command = "rpm -q {!s} &>/dev/null && zypper -n in -C --force-resolution -y -l {!s}={!s}"


class RedHatDowngrade(Downgrade):
    def __init__(self, *a, **kw):
        super(RedHatDowngrade, self).__init__(*a, **kw)
        self.commands = ["yum -y downgrade {!s}".format(" ".join(self.packages))]


class CaaSPDowngrade(Downgrade):
    def __init__(self, *a, **kw):
        super(CaaSPDowngrade, self).__init__(*a, **kw)
        self.kind = "transactional"
        self.commands = [
            'transactional-update rollback $(transactional-update rollback | cut -d" " -f 4)'
        ]


Downgrader = DictWithInjections(
    {
        "15": ZypperDowngrade,
        "12": ZypperDowngrade,
        "11": ZypperDowngrade,
        "YUM": RedHatDowngrade,
        "CAASP": CaaSPDowngrade,
    },
    key_error=MissingDowngraderError,
)


class ZypperInstall(Install):
    def __init__(self, *a, **kw):
        super(ZypperInstall, self).__init__(*a, **kw)

        commands = []

        commands.append("zypper -n in -y -l {!s}".format(" ".join(self.packages)))

        self.commands = commands


class RedHatInstall(Install):
    def __init__(self, *a, **kw):
        super(RedHatInstall, self).__init__(*a, **kw)

        commands = []

        commands.append("yum -y install {!s}".format(" ".join(self.packages)))

        self.commands = commands


Installer = DictWithInjections(
    {
        "15": ZypperInstall,
        "12": ZypperInstall,
        "11": ZypperInstall,
        "YUM": RedHatInstall,
        "CAASP": ZypperInstall,
    },
    key_error=MissingInstallerError,
)


class ZypperUninstall(Install):
    def __init__(self, *a, **kw):
        super(ZypperUninstall, self).__init__(*a, **kw)

        commands = []

        commands.append("zypper -n rm {!s}".format(" ".join(self.packages)))

        self.commands = commands


class RedHatUninstall(Install):
    def __init__(self, *a, **kw):
        super(RedHatUninstall, self).__init__(*a, **kw)

        commands = []

        commands.append("yum -y remove {!s}".format(" ".join(self.packages)))

        self.commands = commands


Uninstaller = DictWithInjections(
    {
        "15": ZypperUninstall,
        "12": ZypperUninstall,
        "11": ZypperUninstall,
        "YUM": RedHatUninstall,
        "CAASP": ZypperUninstall,
    },
    key_error=MissingUninstallerError,
)
