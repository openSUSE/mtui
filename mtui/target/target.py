#
# target host management. this is right above the ssh/transmission layer and
# below the abstractions layer (like updating, preparing, etc.)
#

from logging import getLogger
from pathlib import Path
import re
from string import Template
from traceback import format_exc
from typing import Any, Callable

from . import TargetLock, TargetLockedError
from .. import messages
from ..actions import downgrader, installer, preparer, uninstaller, updater
from ..checks import downgrade_checks, install_checks, prepare_checks, update_checks
from ..connection import CommandTimeout, Connection, errno
from ..target.parsers import parse_system
from ..types.hostlog import HostLog
from ..types.package import Package
from ..types.rpmver import RPMVersion
from ..types.systems import System
from ..utils import timestamp

logger = getLogger("mtui.target")


def _no_checks(*args) -> None:
    return None


class Target:
    def __init__(
        self,
        config,
        hostname: str,
        packages=None,
        state="enabled",
        timeout=300,
        exclusive: bool = False,
        lock=TargetLock,
        connection=Connection,
    ) -> None:
        self.config = config
        self.host, _, self.port = hostname.partition(":")
        self.hostname = hostname
        self.system: System
        self.packages: dict[str, Any] = {}
        self.out = HostLog()
        self.TargetLock = lock
        self.Connection = connection

        self.state = state
        # default timeout for target, used only on connecting/reconnecting Target
        self._timeout = timeout
        self.exclusive = exclusive
        self.connection: Connection
        # helper for packages before system analysis
        self._pkgs = packages

    def _parse_packages(self) -> dict[str, Any]:
        ret = {}
        base_version = self.system.get_base().version
        if self._pkgs:
            packages = self._pkgs.get(base_version, {})
            if base_version.startswith("12"):
                packages.update(self._pkgs.get("12", {}))

            for key, value in packages.items():
                package = Package(key)
                package.required = value
                ret[key] = package
        return ret

    def connect(self) -> None:
        try:
            logger.info("connecting to %s", self.hostname)
            self.connection = self.Connection(self.host, self.port, self._timeout)
        except Exception as e:
            logger.critical(messages.ConnectingTargetFailedMessage(self.hostname, e))
            raise e

        self._lock = self.TargetLock(self.connection, self.config)
        if self.is_locked():
            logger.warning(self._lock.locked_by_msg())

        # get system
        self.system = parse_system(self.connection)

        # parse packages
        self.packages = self._parse_packages()

    def reload_system(self) -> None:
        self.system = parse_system(self.connection)

    def __eq__(self, other) -> bool:
        return self.system == other.system

    def __ne__(self, other) -> bool:
        return self.system != other.system

    def query_versions(self, packages=None) -> None:
        if packages is None:
            packages = self.packages.keys()

        if self.state == "enabled":
            pvs = self.query_package_versions(packages)
            for p, v in pvs.items():
                if v:
                    self.packages[p].current = str(v)
                else:
                    self.packages[p].current = None
        elif self.state == "dryrun":
            logger.info('dryrun: %s running "rpm -q %s"', self.hostname, packages)
            self.out.append(["rpm -q {}".format(packages), "dryrun\n", "", 0, 0])
        elif self.state == "disabled":
            self.out.append(["", "", "", 0, 0])

    def query_package_versions(self, packages) -> dict[str, RPMVersion]:
        """
        :type packages: [str]
        :param packages: packages to query versions for

        :return: {package: RPMVersion or None}
            where
              package = str
        """
        if self.system.get_base().name != "ubuntu":
            self.run(
                'rpm -q --queryformat "%{{Name}} %{{Version}}-%{{Release}}\n" {}'.format(
                    " ".join(packages)
                )
            )
        else:
            self.run(
                "dpkg-query -W -f='${{package}} ${{version}}\n' {}".format(
                    " ".join(packages)
                )
            )

        packages = {}
        for line in self.lastout().splitlines():
            if match := re.search("package (.*) is not installed", line):
                packages[match.group(1)] = None
                continue
            p, v = line.split()
            # Make sure that it shows to the user the highest version
            if p in packages:
                if RPMVersion(v) > packages[p]:
                    packages[p] = RPMVersion(v)
            else:
                packages[p] = RPMVersion(v)
        return packages

    def disable_repo(self, repo: str) -> None:
        logger.debug("%s: disabling repo %s", self.hostname, repo)
        self.run(f"zypper mr -d {repo}")

    def enable_repo(self, repo: str) -> None:
        logger.debug("%s: enabling repo %s", self.hostname, repo)
        self.run(f"zypper mr -e {repo}")

    def set_timeout(self, value: int) -> None:
        logger.debug("%s: setting timeout to %d", self.hostname, value)
        self.connection.timeout = value
        self._timeout = value

    def set_repo(self, operation, testreport) -> None:
        logger.debug("%s: changing %s repos", self.hostname, operation)
        testreport.set_repo(self, operation)

    def run_zypper(self, cmd, repos, rrid) -> None:
        # ur - generator returning tuple with product, repopart
        ur = ((x, y) for x, y in repos.items() if x in self.system.flatten())

        def name(product, rrid) -> str:
            return "issue-{}:{}:p={}".format(
                product.name, product.version, rrid.maintenance_id
            )

        for x, y in ur:
            if "ar" in cmd:
                logger.info("Adding repo %s on %s", y, self.hostname)
                self.run("zypper {0} {1} {2} {1}".format(cmd, name(x, rrid), y))
            elif "rr" in cmd:
                logger.info("Removing repo %s on %s", y, self.hostname)
                self.run("zypper {0} {1}".format(cmd, y))
            else:
                self.unlock(force=True)
                raise ValueError

        self.run("zypper -n ref")

    def run(self, command, lock=None) -> None:
        if self.state == "enabled":
            logger.debug('%s: running "%s"', self.hostname, command)
            time_before = timestamp()
            try:
                exitcode = self.connection.run(command, lock)
            except CommandTimeout:
                logger.critical('%s: command "%s" timed out', self.hostname, command)
                exitcode = -1
            except AssertionError:
                logger.debug("zombie command terminated")
                logger.debug(format_exc())
                return
            except Exception:
                # failed to run command
                logger.error('%s: failed to run command "%s"', self.hostname, command)
                exitcode = -1

            time_after = timestamp()
            runtime = int(time_after) - int(time_before)
            # this is wrong
            self.out.append(
                [
                    command,
                    self.connection.stdout,
                    self.connection.stderr,
                    exitcode,
                    runtime,
                ]
            )
        elif self.state == "dryrun":
            logger.info('dryrun: %s running "%s"', self.hostname, command)
            self.out.append([command, "dryrun\n", "", 0, 0])
        elif self.state == "disabled":
            self.out.append(["", "", "", 0, 0])

    def shell(self) -> None:
        logger.debug("%s: spawning shell", self.hostname)

        try:
            self.connection.shell()
        except Exception:
            # failed to spawn shell
            logger.error("%s: failed to spawn shell", self.hostname)

    def sftp_put(self, local: Path, remote: Path) -> None:
        if self.state == "enabled":
            logger.debug('%s: sending "%s"', self.hostname, local)
            try:
                return self.connection.sftp_put(local, remote)
            except EnvironmentError as error:
                logger.error(
                    "%s: failed to send %s: %s", self.hostname, local, error.strerror
                )
        elif self.state == "dryrun":
            logger.info("dryrun: put {} {}:{}".format(local, self.hostname, remote))

    def sftp_get(self, remote: Path, local: Path) -> None:
        if str(remote).endswith("/"):
            f = self.connection.sftp_get_folder
            s = "folder"
            local = Path(str(local) + "/")
        else:
            f = self.connection.sftp_get
            s = "file"
            local = Path(f"{local}.{self.hostname}")

        if self.state == "enabled":
            logger.debug(
                '%s: receiving %s "%s" into "%s', self.hostname, s, remote, local
            )
            try:
                f(remote, local)
            except EnvironmentError as error:
                logger.error(
                    "%s: failed to get %s %s: %s",
                    self.hostname,
                    s,
                    remote,
                    error.strerror,
                )
        elif self.state == "dryrun":
            logger.info("dryrun: get %s %s:%s %s", self.hostname, s, remote, local)

    def lastin(self) -> str:
        try:
            return self.out[-1][0]
        except BaseException:
            return ""

    def lastout(self) -> str:
        try:
            return self.out[-1][1]
        except BaseException:
            return ""

    def lasterr(self) -> str:
        try:
            return self.out[-1][2]
        except BaseException:
            return ""

    def lastexit(self) -> str:
        try:
            return self.out[-1][3]
        except BaseException:
            return ""

    def is_locked(self) -> bool:
        """
        :returns bool: True if target is locked by someone else
        """
        return self._lock.is_locked()

    def lock(self, comment: str = "") -> None:
        self._lock.lock(comment)

    def unlock(self, force: bool = False) -> None:
        try:
            self._lock.unlock(force)
        except TargetLockedError as e:
            logger.warning(e)
            raise

    def add_history(self, comment: str) -> None:
        if self.state == "enabled":
            logger.debug("%s: adding history entry", self.hostname)
            try:
                filename = Path("/var/log/mtui.log")
                historyfile = self.connection.sftp_open(filename, "a+")
            except Exception as error:
                logger.error("failed to open history file: %s", error)
                return

            now = timestamp()
            user: str = self.config.session_user
            try:
                historyfile.write("{}:{}:{}\n".format(now, user, ":".join(comment)))
                historyfile.close()
            except Exception:
                pass

    def sftp_listdir(self, path: Path) -> list[str]:
        try:
            return self.connection.sftp_listdir(path)
        except IOError as error:
            if error.errno == errno.ENOENT:
                logger.debug("%s: directory %s does not exist", self.hostname, path)
            return []

    def sftp_remove(self, path: Path) -> None:
        try:
            self.connection.sftp_remove(path)
        except IOError as error:
            if error.errno == errno.ENOENT:
                logger.debug("%s: path %s does not exist", self.hostname, path)
            else:
                try:
                    # might be a directory
                    self.connection.sftp_rmdir(path)
                except IOError:
                    logger.warning("unable to remove %s on %s", path, self.hostname)

    def close(self, action=None) -> None:
        try:
            assert self.connection

            if self.connection.is_active():
                self.connection.timeout = 15
                self.unlock()
        except Exception:
            # ignore if the connection seems to be lost
            pass
        else:
            if action == "reboot":
                logger.info("rebooting %s", self.hostname)
                self.run("reboot")
            elif action == "poweroff":
                logger.info("powering off %s", self.hostname)
                self.run("halt")
            else:
                logger.info("closing connection to %s", self.hostname)

        if self.connection:
            self.connection.close()

    def report_self(self, sink: Callable[[str, System, str, bool], None]) -> None:
        sink(self.hostname, self.system, self.state, self.exclusive)

    def report_history(self, sink: Callable[[str, System, list[str]], None]) -> None:
        sink(self.hostname, self.system, self.lastout().split("\n"))

    def report_locks(self, sink: Callable[[str, System, TargetLock], None]) -> None:
        sink(self.hostname, self.system, self._lock)

    def report_timeout(self, sink: Callable[[str, System, int], None]) -> None:
        sink(self.hostname, self.system, self.connection.timeout)

    def report_sessions(self, sink: Callable[[str, System, str], None]) -> None:
        sink(self.hostname, self.system, self.lastout())

    def report_log(self, sink: Callable, arg) -> None:
        sink(self.hostname, self.out, arg)

    def report_products(self, sink: Callable[[str, System], None]) -> None:
        sink(self.hostname, self.system)

    def get_installer(self) -> dict[str, Template]:
        return installer[self.system.get_release()]

    def get_installer_check(self) -> Callable:
        return install_checks.get(self.system.get_release(), _no_checks)

    def get_uninstaller(self) -> dict[str, Template]:
        return uninstaller[self.system.get_release()]

    def get_uninstaller_check(self) -> Callable:
        return install_checks.get(self.system.get_release(), _no_checks)

    def get_downgrader(self) -> dict[str, Template]:
        return downgrader[self.system.get_release()]

    def get_downgrader_check(self) -> Callable:
        return downgrade_checks.get(self.system.get_release(), _no_checks)

    def get_updater(self) -> dict[str, Template]:
        return updater[self.system.get_release()]

    def get_updater_check(self) -> Callable:
        return update_checks.get(self.system.get_release(), _no_checks)

    def get_preparer(
        self, force: bool = False, testing: bool = False
    ) -> dict[str, Template]:
        return preparer[self.system.get_release()](force, testing)

    def get_preparer_check(self) -> Callable:
        return prepare_checks.get(self.system.get_release(), _no_checks)

    def __repr__(self) -> str:
        return f"<Target - {self.hostname}>"

    def __str__(self) -> str:
        return self.hostname
