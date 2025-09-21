"""The `Target` class, which represents a single target host."""

import errno
import re
from logging import getLogger
from pathlib import Path
from string import Template
from traceback import format_exc
from typing import Any, Callable, Literal, final

from .. import messages
from ..actions import downgrader, installer, preparer, uninstaller, updater
from ..checks import downgrade_checks, install_checks, prepare_checks, update_checks
from ..config import Config
from ..connection import CommandTimeout, Connection
from ..target.parsers import parse_system
from ..types import HostLog, Package, System
from ..types.rpmver import RPMVersion
from ..utils import timestamp
from . import TargetLock, TargetLockedError

logger = getLogger("mtui.target")


def _no_checks(*args: tuple[Any, ...]) -> None:
    return None


@final
class Target:
    """Represents a single target host.

    This class provides methods for interacting with the host, such as
    running commands, transferring files, and managing locks.
    """

    def __init__(
        self,
        config: Config,
        hostname: str,
        packages: dict[str, dict[str, str]] | None = None,
        state: Literal["enabled", "disabled", "serial", "parallel"] = "enabled",
        timeout: int = 300,
        exclusive: bool = False,
        lock: type[TargetLock] = TargetLock,
        connection: type[Connection] = Connection,
    ) -> None:
        """Initializes the `Target` object.

        Args:
            config: The application configuration.
            hostname: The hostname of the target.
            packages: A dictionary of packages for the target.
            state: The initial state of the target.
            timeout: The command timeout for the target.
            exclusive: Whether the target is in exclusive mode.
            lock: The lock class to use for the target.
            connection: The connection class to use for the target.
        """
        self.config = config
        self.host, _, self.port = hostname.partition(":")
        self.hostname = hostname
        self.system: System
        self.packages: dict[str, Package] = {}
        self.out = HostLog()
        self.TargetLock = lock
        self.Connection = connection

        self.state = state
        # default timeout for target, used only on connecting/reconnecting Target
        self._timeout = timeout
        self.exclusive = exclusive
        self.connection: Connection
        self.transactional = False
        # helper for packages before system analysis
        self._pkgs = packages

    def _parse_packages(self) -> dict[str, Package]:
        """Parses the packages for the target host.

        Returns:
            A dictionary of `Package` objects.
        """
        ret: dict[str, Package] = {}
        base_version = self.system.get_base().version
        if self._pkgs:
            if "standard" in self._pkgs and len(self._pkgs) == 1:
                packages = self._pkgs["standard"]
            else:
                packages = self._pkgs.get(base_version, {})
            if base_version.startswith("12"):
                packages.update(self._pkgs.get("12", {}))

            for key, value in packages.items():
                package = Package(key)
                package.required = value
                ret[key] = package
        return ret

    def connect(self) -> None:
        """Connects to the target host."""
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
        self.system, self.transactional = parse_system(self.connection)

        # parse packages
        self.packages = self._parse_packages()
        self.query_versions()

    def reconnect(self, retry, backoff) -> None:
        """Reconnects to the target host.

        Args:
            retry: The number of times to retry the connection.
            backoff: Whether to use exponential backoff for retries.
        """
        self.connection.reconnect(retry, backoff)

    def reload_system(self) -> None:
        """Reloads the system information for the target host."""
        self.system, self.transactional = parse_system(self.connection)

    def __eq__(self, other) -> bool:
        """Checks if two `Target` objects are equal."""
        return self.system == other.system

    def __ne__(self, other) -> bool:
        """Checks if two `Target` objects are not equal."""
        return self.system != other.system

    def query_versions(self, packages=None) -> None:
        """Queries the package versions for the target host.

        Args:
            packages: A list of packages to query. If None, all packages
                for the target are queried.
        """
        if packages is None:
            packages = list(self.packages.keys())
        if self.state == "enabled":
            pvs = self.query_package_versions(packages)
            for p, v in pvs.items():
                self.packages[p].current = v
        elif self.state == "dryrun":
            logger.info('dryrun: %s running "rpm -q %s"', self.hostname, packages)
            self.out.append(["rpm -q {}".format(packages), "dryrun\n", "", 0, 0])
        elif self.state == "disabled":
            self.out.append(["", "", "", 0, 0])

    def query_package_versions(
        self, packages: list[str]
    ) -> dict[str, RPMVersion | None]:
        """Queries the versions of a list of packages.

        Args:
            packages: A list of packages to query.

        Returns:
            A dictionary mapping package names to `RPMVersion` objects.
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

        pkgs: dict[str, RPMVersion | None] = {}
        for line in self.lastout().splitlines():
            if match := re.search("package (.*) is not installed", line):
                pkgs[match.group(1)] = None
                continue
            p, v = line.split()
            # Make sure that it shows to the user the highest version
            if p in pkgs:
                if RPMVersion(v) > pkgs[p]:
                    pkgs[p] = RPMVersion(v)
            else:
                pkgs[p] = RPMVersion(v)
        return pkgs

    def disable_repo(self, repo: str) -> None:
        """Disables a repository on the target host.

        Args:
            repo: The name of the repository to disable.
        """
        logger.debug("%s: disabling repo %s", self.hostname, repo)
        self.run(f"zypper mr -d {repo}")

    def enable_repo(self, repo: str) -> None:
        """Enables a repository on the target host.

        Args:
            repo: The name of the repository to enable.
        """
        logger.debug("%s: enabling repo %s", self.hostname, repo)
        self.run(f"zypper mr -e {repo}")

    def set_timeout(self, value: int) -> None:
        """Sets the command timeout for the target host.

        Args:
            value: The timeout in seconds.
        """
        logger.debug("%s: setting timeout to %d", self.hostname, value)
        self.connection.timeout = value
        self._timeout = value

    def set_repo(self, operation, testreport) -> None:
        """Adds or removes a repository on the target host.

        Args:
            operation: The operation to perform ("add" or "remove").
            testreport: The test report object.
        """
        logger.debug("%s: changing %s repos", self.hostname, operation)
        testreport.set_repo(self, operation)

    def run_zypper(self, cmd, repos, rrid) -> None:
        """Runs a `zypper` command on the target host.

        Args:
            cmd: The `zypper` command to run.
            repos: A dictionary of repositories.
            rrid: The RequestReviewID of the current update.
        """
        # ur - generator returning tuple with product, repopart
        ur = ((x, y) for x, y in repos.items() if x in self.system.flatten())

        def name(product, rrid) -> str:
            return f"issue-{product.name}:{product.version}:p={rrid.maintenance_id}:{rrid.review_id}"

        for x, y in ur:
            if "ar" in cmd:
                logger.info("Adding repo %s on %s", y, self.hostname)
                self.run(f"zypper {cmd} {name(x, rrid)} {y} {name(x, rrid)}")
            elif "rr" in cmd:
                logger.info("Removing repo %s on %s", y, self.hostname)
                self.run(f"zypper {cmd} {y}")
            else:
                self.unlock(force=True)
                raise ValueError

        self.run("zypper -n ref")

    def run(self, command: str, lock=None) -> None:
        """Runs a command on the target host.

        Args:
            command: The command to run.
            lock: An optional lock to use.
        """
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
        """Spawns a shell on the target host."""
        logger.debug("%s: spawning shell", self.hostname)

        try:
            self.connection.shell()
        except Exception:
            # failed to spawn shell
            logger.error("%s: failed to spawn shell", self.hostname)

    def sftp_put(self, local: Path, remote: Path) -> None:
        """Uploads a file to the target host.

        Args:
            local: The local path to the file to upload.
            remote: The remote path to upload the file to.
        """
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
        """Downloads a file from the target host.

        Args:
            remote: The remote path to the file to download.
            local: The local path to save the downloaded file to.
        """
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
        """Returns the last command that was run.

        Returns:
            The last command that was run.
        """
        try:
            return self.out[-1][0]
        except BaseException:
            return ""

    def lastout(self) -> str:
        """Returns the last stdout from a command.

        Returns:
            The last stdout from a command.
        """
        try:
            return self.out[-1][1]
        except BaseException:
            return ""

    def lasterr(self) -> str:
        """Returns the last stderr from a command.

        Returns:
            The last stderr from a command.
        """
        try:
            return self.out[-1][2]
        except BaseException:
            return ""

    def lastexit(self) -> str:
        """Returns the last exit code from a command.

        Returns:
            The last exit code from a command.
        """
        try:
            return self.out[-1][3]
        except BaseException:
            return ""

    def is_locked(self) -> bool:
        """Checks if the target is locked.

        Returns:
            True if the target is locked, False otherwise.
        """
        return self._lock.is_locked()

    def lock(self, comment: str = "") -> None:
        """Locks the target.

        Args:
            comment: An optional comment for the lock.
        """
        self._lock.lock(comment)

    def unlock(self, force: bool = False) -> None:
        """Unlocks the target.

        Args:
            force: If True, unlocks the target even if it is locked
                by another user.
        """
        try:
            self._lock.unlock(force)
        except TargetLockedError as e:
            logger.warning(e)
            raise

    def add_history(self, comment: str) -> None:
        """Adds a history entry to the target.

        Args:
            comment: The history entry to add.
        """
        if self.state == "enabled":
            logger.debug("%s: adding history entry", self.hostname)
            try:
                filename = Path("/var/log/mtui.log")
                historyfile = self.connection.sftp_open(filename, "a+")
            except Exception as error:
                logger.error("failed to open history file: %s", error)
                return

            now = timestamp()
            user: str = self.config.session_user  # type: ignore
            try:
                historyfile.write("{}:{}:{}\n".format(now, user, ":".join(comment)))
                historyfile.close()
            except Exception:
                pass

    def sftp_listdir(self, path: Path) -> list[str]:
        """Lists the contents of a directory on the target.

        Args:
            path: The path to the directory to list.

        Returns:
            A list of filenames in the directory.
        """
        try:
            return self.connection.sftp_listdir(path)
        except IOError as error:
            if error.errno == errno.ENOENT:
                logger.debug("%s: directory %s does not exist", self.hostname, path)
            return []

    def sftp_remove(self, path: Path) -> None:
        """Deletes a file or directory on the target.

        Args:
            path: The path to the file or directory to delete.
        """
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
        """Closes the connection to the target.

        Args:
            action: An optional action to perform before closing the
                connection ("reboot" or "poweroff").
        """
        try:
            if self.connection and self.connection.is_active():
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

    def report_self(self, sink: Callable[[str, System, bool, str, bool], None]) -> None:
        """Reports the status of the target.

        Args:
            sink: The function to use for reporting.
        """
        sink(self.hostname, self.system, self.transactional, self.state, self.exclusive)

    def report_history(self, sink: Callable[[str, System, list[str]], None]) -> None:
        """Reports the history of the target.

        Args:
            sink: The function to use for reporting.
        """
        sink(self.hostname, self.system, self.lastout().split("\n"))

    def report_locks(self, sink: Callable[[str, System, TargetLock], None]) -> None:
        """Reports the lock state of the target.

        Args:
            sink: The function to use for reporting.
        """
        sink(self.hostname, self.system, self._lock)

    def report_timeout(self, sink: Callable[[str, System, int], None]) -> None:
        """Reports the timeout of the target.

        Args:
            sink: The function to use for reporting.
        """
        sink(self.hostname, self.system, self.connection.timeout)

    def report_sessions(self, sink: Callable[[str, System, str], None]) -> None:
        """Reports the sessions of the target.

        Args:
            sink: The function to use for reporting.
        """
        sink(self.hostname, self.system, self.lastout())

    def report_log(self, sink: Callable, arg) -> None:
        """Reports the log of the target.

        Args:
            sink: The function to use for reporting.
            arg: An additional argument to pass to the reporting function.
        """
        sink(self.hostname, self.out, arg)

    def report_products(self, sink: Callable[[str, System], None]) -> None:
        """Reports the products of the target.

        Args:
            sink: The function to use for reporting.
        """
        sink(self.hostname, self.system)

    def get_installer(self) -> dict[str, Template]:
        """Gets the installer for the target.

        Returns:
            A dictionary of installer command templates.
        """
        return installer[(self.system.get_release(), self.transactional)]

    def get_installer_check(self) -> Callable:
        """Gets the installer check function for the target.

        Returns:
            The installer check function.
        """
        return install_checks.get(
            (self.system.get_release(), self.transactional), _no_checks
        )

    def get_uninstaller(self) -> dict[str, Template]:
        """Gets the uninstaller for the target.

        Returns:
            A dictionary of uninstaller command templates.
        """
        return uninstaller[(self.system.get_release(), self.transactional)]

    def get_uninstaller_check(self) -> Callable:
        """Gets the uninstaller check function for the target.

        Returns:
            The uninstaller check function.
        """
        return install_checks.get(
            (self.system.get_release(), self.transactional), _no_checks
        )

    def get_downgrader(self) -> dict[str, Template]:
        """Gets the downgrader for the target.

        Returns:
            A dictionary of downgrader command templates.
        """
        return downgrader[(self.system.get_release(), self.transactional)]

    def get_downgrader_check(self) -> Callable:
        """Gets the downgrader check function for the target.

        Returns:
            The downgrader check function.
        """
        return downgrade_checks.get(
            (self.system.get_release(), self.transactional), _no_checks
        )

    def get_updater(self) -> dict[str, Template]:
        """Gets the updater for the target.

        Returns:
            A dictionary of updater command templates.
        """
        return updater[(self.system.get_release(), self.transactional)]

    def get_updater_check(self) -> Callable:
        """Gets the updater check function for the target.

        Returns:
            The updater check function.
        """
        return update_checks.get(
            (self.system.get_release(), self.transactional), _no_checks
        )

    def get_preparer(
        self, force: bool = False, testing: bool = False
    ) -> dict[str, Template]:
        """Gets the preparer for the target.

        Args:
            force: Whether to force the preparation.
            testing: Whether to include testing repositories.

        Returns:
            A dictionary of preparer command templates.
        """
        return preparer[(self.system.get_release(), self.transactional)](force, testing)

    def get_preparer_check(self) -> Callable:
        """Gets the preparer check function for the target.

        Returns:
            The preparer check function.
        """
        return prepare_checks.get(
            (self.system.get_release(), self.transactional), _no_checks
        )

    def __repr__(self) -> str:
        """Returns a string representation of the `Target` object."""
        return f"<Target - {self.hostname}>"

    def __str__(self) -> str:
        """Returns the hostname of the target."""
        return self.hostname
