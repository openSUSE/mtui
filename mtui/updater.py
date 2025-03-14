#
# update and software stack management
#

from logging import getLogger

from .checks import (
    EmptyCheck,
    ZypperDowngradeCheck,
    ZypperPrepareCheck,
    ZypperUpdateCheck,
)
from .exceptions import UpdateError
from .messages import MissingDowngraderError, MissingPreparerError, MissingUpdaterError
from .target.downgrade import Downgrade
from .target.prepare import Prepare
from .target.update import Update
from .utils import DictWithInjections


logger = getLogger("mtui.updater")


class ZypperUpdate(Update, ZypperUpdateCheck):
    def check(self, target, stdin, stdout, stderr, exitcode) -> None:
        if "Error:" in stderr:
            logger.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr,
            )
            raise UpdateError("RPM Error", target.hostname)
        if "The following package is not supported by its vendor" in stdout:
            logger.critical("%s: package support is uncertain:", target.hostname)
            marker = "The following package is not supported by its vendor:\n"
            start = stdout.find(marker)
            end = stdout.find("\n\n", start)
            print(stdout[start:end])


class ZypperOBSUpdate(ZypperUpdate):
    def __init__(self, *a, **kw) -> None:
        super().__init__(*a, **kw)
        repat = ":p={:d}"
        repo: str = repat.format(self.testreport.rrid.maintenance_id)

        self.commands: list[str] = [
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


# its weird, but still our base classes are designed for zypper :(
# TODO: broken, cant really work
class RedHatUpdate(Update, EmptyCheck):
    def __init__(self, *a, **kw) -> None:
        super().__init__(*a, **kw)

        self.commands: list[str] = [
            "export LANG=",
            "yum repolist",
            "yum -y update {!s}".format(" ".join(self.packages)),
        ]


# TODO deprecated
class CaaSPUpdate(Update, EmptyCheck):
    def __init__(self, *a, **kw):
        super().__init__(*a, **kw)
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


class ZypperPrepare(Prepare, ZypperPrepareCheck):
    def __init__(self, *a, **kw) -> None:
        super().__init__(*a, **kw)

        parameter = ""
        commands: list[str] = []

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

    def check(self, target, stdin, stdout, stderr, exitcode) -> None:
        if "Error:" in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("RPM Error", target.hostname)


class RedHatPrepare(Prepare, EmptyCheck):
    def __init__(self, *a, **kw) -> None:
        super().__init__(*a, **kw)

        parameter = ""
        commands: list[str] = []

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


class CaaSPPrepare(Prepare, EmptyCheck):
    def run(self) -> None:
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


class ZypperDowngrade(Downgrade, ZypperDowngradeCheck):
    def __init__(self, *a, **kw) -> None:
        super().__init__(*a, **kw)

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


class RedHatDowngrade(Downgrade, EmptyCheck):
    def __init__(self, *a, **kw) -> None:
        super().__init__(*a, **kw)
        self.commands = ["yum -y downgrade {!s}".format(" ".join(self.packages))]


class CaaSPDowngrade(Downgrade, EmptyCheck):
    def __init__(self, *a, **kw) -> None:
        super().__init__(*a, **kw)
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
