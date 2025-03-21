#
# update and software stack management
#

from logging import getLogger

from .checks import EmptyCheck, ZypperUpdateCheck
from .exceptions import UpdateError
from .messages import MissingUpdaterError
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
