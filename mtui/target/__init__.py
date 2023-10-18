#
# target host management. this is right above the ssh/transmission layer and
# below the abstractions layer (like updating, preparing, etc.)
#

from logging import getLogger
import re
from traceback import format_exc
from typing import Dict, Optional

from .. import messages
from ..connection import CommandTimeout, Connection, errno
from ..target.locks import LockedTargets, RemoteLock, TargetLock, TargetLockedError  # noqa: F401
from ..target.parsers import parse_system
from ..types.hostlog import HostLog
from ..types.package import Package
from ..types.rpmver import RPMVersion
from ..utils import timestamp

logger = getLogger("mtui.target")


class Target:
    def __init__(
        self,
        config,
        hostname,
        packages=None,
        state="enabled",
        timeout=300,
        exclusive=False,
        lock=TargetLock,
        connection=Connection,
    ):
        """
        :type connect: bool
        :param connect:
            introduced in order to run unit tests witout
            having the target automatically connect
        """

        self.config = config
        self.host, _, self.port = hostname.partition(":")
        self.hostname = hostname
        self.system = None
        self.packages = {}
        self.out = HostLog()
        self.TargetLock = lock
        self.Connection = connection

        self.state = state
        """
        :param state:
        :type state: str either "enabled" or "disabled"
        :deprecated:
        """
        self.timeout = timeout
        self.exclusive = exclusive
        self.connection = None

        # helper for packages before system analysis
        self._pkgs = packages

    def _parse_packages(self):
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

    def _parse_system(self):
        logger.debug("get and parse target installed products")
        if self.connection:
            return parse_system(self.connection)

    def connect(self):
        try:
            logger.info("connecting to {}".format(self.hostname))
            self.connection = self.Connection(self.host, self.port, self.timeout)
        except Exception as e:
            logger.critical(messages.ConnectingTargetFailedMessage(self.hostname, e))
            raise e

        self._lock = self.TargetLock(self.connection, self.config)
        if self.is_locked():
            logger.warning(self._lock.locked_by_msg())

        # get system
        self.system = self._parse_system()

        # parse packages
        self.packages = self._parse_packages()

    def __lt__(self, other):
        return sorted([self.system, other.system])[0] == self.system

    def __gt__(self, other):
        return sorted([self.system, other.system])[0] == other.system

    def __eq__(self, other):
        return self.system == other.system

    def __ne__(self, other):
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

            logger.info(
                'dryrun: {} running "rpm -q {}"'.format(self.hostname, packages)
            )
            self.out.append(["rpm -q {}".format(packages), "dryrun\n", "", 0, 0])
        elif self.state == "disabled":

            self.out.append(["", "", "", 0, 0])

    def query_package_versions(self, packages) -> Optional[Dict[str, RPMVersion]]:
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
            match = re.search("package (.*) is not installed", line)
            if match:
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
        logger.debug("{}: disabling repo {}".format(self.hostname, repo))
        self.run("zypper mr -d {}".format(repo))

    def enable_repo(self, repo: str) -> None:
        logger.debug("{}: enabling repo {}".format(self.hostname, repo))
        self.run("zypper mr -e {}".format(repo))

    def set_timeout(self, value: int) -> None:
        logger.debug("{}: setting timeout to {}".format(self.hostname, value))
        self.connection.timeout = value

    def get_timeout(self) -> int:
        return self.connection.timeout

    def get_system(self):
        return str(self.system)

    def set_repo(self, operation, testreport) -> None:
        logger.debug("{}: enabling {} repos".format(self.hostname, operation))
        testreport.set_repo(self, operation)

    def run_zypper(self, cmd, repos, rrid) -> None:
        # ur - generator returning tuple with product, repopart
        ur = ((x, y) for x, y in repos.items() if x in self.system.flatten())

        def name(product, rrid):
            return "issue-{}:{}:p={}".format(
                product.name, product.version, rrid.maintenance_id
            )

        def fullpath(path, rrid):
            # TODO: confiruable download path?
            dl_path = "http://download.suse.de/ibs/"
            return dl_path + ":/".join(str(rrid).split(":")[:-1]) + "/" + path

        for x, y in ur:
            if "ar" in cmd:
                logger.info("Adding repo {} on {}".format(y, self.hostname))
                self.run(
                    "zypper {0} {1} {2} {1}".format(
                        cmd, name(x, rrid), fullpath(y, rrid)
                    )
                )
            elif "rr" in cmd:
                logger.info("Removing repo {} on {}".format(y, self.hostname))
                self.run("zypper {0} {1}".format(cmd, fullpath(y, rrid)))
            else:
                self.unlock(force=True)
                raise ValueError

        self.run("zypper -n ref")

    def run(self, command, lock=None) -> None:
        if self.state == "enabled":
            logger.debug('{}: running "{}"'.format(self.hostname, command))
            time_before = timestamp()
            try:
                exitcode = self.connection.run(command, lock)
            except CommandTimeout:
                logger.critical(
                    '{}: command "{}" timed out'.format(self.hostname, command)
                )
                exitcode = -1
            except AssertionError:
                logger.debug("zombie command terminated")
                logger.debug(format_exc())
                return
            except Exception:
                # failed to run command
                logger.error(
                    '{}: failed to run command "{}"'.format(self.hostname, command)
                )
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

            logger.info('dryrun: {} running "{}"'.format(self.hostname, command))
            self.out.append([command, "dryrun\n", "", 0, 0])
        elif self.state == "disabled":

            self.out.append(["", "", "", 0, 0])

    def shell(self):
        logger.debug("{}: spawning shell".format(self.hostname))

        try:
            self.connection.shell()
        except Exception:
            # failed to spawn shell
            logger.error("{}: failed to spawn shell".format(self.hostname))

    def put(self, local, remote):
        if self.state == "enabled":
            logger.debug('{}: sending "{}"'.format(self.hostname, local))
            try:
                return self.connection.put(local, remote)
            except EnvironmentError as error:
                logger.error(
                    "{}: failed to send {}: {}".format(
                        self.hostname, local, error.strerror
                    )
                )
        elif self.state == "dryrun":
            logger.info("dryrun: put {} {}:{}".format(local, self.hostname, remote))

    def get(self, remote, local):

        if remote.endswith("/"):
            f = self.connection.get_folder
            s = "folder"
            local = str(local) + "/"
        else:
            f = self.connection.get
            s = "file"
            local = "{}.{}".format(local, self.hostname)

        if self.state == "enabled":
            logger.debug(
                '{}: receiving {} "{}" into "{}'.format(self.hostname, s, remote, local)
            )
            try:
                return f(remote, local)
            except EnvironmentError as error:
                logger.error(
                    "{}: failed to get {} {}: {}".format(
                        self.hostname, s, remote, error.strerror
                    )
                )
        elif self.state == "dryrun":
            logger.info(
                "dryrun: get {} {}:{} {}".format(self.hostname, s, remote, local)
            )

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

    def lock(self, comment=None):
        """
        :returns None:
        """
        self._lock.lock(comment)

    def unlock(self, force=False):
        try:
            self._lock.unlock(force)
        except TargetLockedError as e:
            logger.warning(e)
            raise

    def add_history(self, comment) -> None:
        if self.state == "enabled":
            logger.debug("{}: adding history entry".format(self.hostname))
            try:
                filename = "/var/log/mtui.log"
                historyfile = self.connection.open(filename, "a+")
            except Exception as error:
                logger.error("failed to open history file: {}".format(error))
                return

            now = timestamp()
            user = self.config.session_user
            try:
                historyfile.write("{}:{}:{}\n".format(now, user, ":".join(comment)))
                historyfile.close()
            except Exception:
                pass

    def listdir(self, path):
        try:
            return self.connection.listdir(path)
        except IOError as error:
            if error.errno == errno.ENOENT:
                logger.debug(
                    "{}: directory {} does not exist".format(self.hostname, path)
                )
            return []

    def remove(self, path):
        try:
            self.connection.remove(path)
        except IOError as error:
            if error.errno == errno.ENOENT:
                logger.debug("{}: path {} does not exist".format(self.hostname, path))
            else:
                try:
                    # might be a directory
                    self.connection.rmdir(path)
                except IOError:
                    logger.warning(
                        "unable to remove {} on {}".format(path, self.hostname)
                    )

    def close(self, action=None):
        self.timeout = 15
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
                logger.info("rebooting {}".format(self.hostname))
                self.run("reboot")
            elif action == "poweroff":
                logger.info("powering off {}".format(self.hostname))
                self.run("halt")
            else:
                logger.info("closing connection to {}".format(self.hostname))

        if self.connection:
            self.connection.close()
            self.connection = None

    def report_self(self, sink):
        return sink(self.hostname, self.system, self.state, self.exclusive)

    def report_history(self, sink):
        return sink(self.hostname, self.system, self.lastout().split("\n"))

    def report_locks(self, sink):
        return sink(self.hostname, self.system, self._lock)

    def report_timeout(self, sink):
        return sink(self.hostname, self.system, self.get_timeout())

    def report_sessions(self, sink):
        return sink(self.hostname, self.system, self.lastout())

    def report_log(self, sink, arg):
        return sink(self.hostname, self.out, arg)

    def report_testsuites(self, sink, suitedir):
        return sink(self.hostname, self.system, self.listdir(suitedir))

    def report_testsuite_results(self, sink, suitename):
        return sink(
            self.hostname, self.lastexit(), self.lastout(), self.lasterr(), suitename
        )

    def report_products(self, sink):
        return sink(self.hostname, self.system)
