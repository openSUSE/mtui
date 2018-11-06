# -*- coding: utf-8 -*-
#
# target host management. this is right above the ssh/transmission layer and
# below the abstractions layer (like updating, preparing, etc.)
#

import re
import signal
from traceback import format_exc
from logging import getLogger

from mtui.connection import Connection
from mtui.connection import errno
from mtui.connection import CommandTimeout

from qamlib.types.rpmver import RPMVersion
from mtui import messages


from mtui.target.locks import Locked

from mtui.target.locks import TargetLock
from mtui.target.locks import TargetLockedError

# Import for other modules -- not used directly here
from mtui.target.locks import LockedTargets
from mtui.target.locks import RemoteLock

from qamlib.utils import timestamp

from mtui.target.parsers import parse_system

logger = getLogger("mtui.target")


class Target(object):
    def __init__(
        self,
        config,
        hostname,
        packages=[],
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
        self.out = []
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

        for package in packages:
            self.packages[package] = Package(package)

    def _parse_system(self):
        logger.debug("get and parse target installed products")
        if self.connection:
            self.system = parse_system(self.connection)

    def connect(self):
        try:
            logger.info("connecting to {}".format(self.hostname))
            self.connection = self.Connection(self.host, self.port, self.timeout)
        except Exception as e:
            logger.critical(messages.ConnectingTargetFailedMessage(self.hostname, e))
            raise

        self._lock = self.TargetLock(self.connection, self.config)
        if self.is_locked():
            # NOTE: the condition was originally locked and lock.comment
            # idk why.
            logger.warning(self._lock.locked_by_msg())

        # get system
        self._parse_system()

    def __lt__(self, other):
        return sorted([self.system, other.system])[0] == self.system

    def __gt__(self, other):
        return sorted([self.system, other.system])[0] == other.system

    def __eq__(self, other):
        return self.system == other.system

    def __ne__(self, other):
        return self.system != other.system

    def query_versions(self, packages=None):
        if packages is None:
            packages = list(self.packages.keys())

        if self.state == "enabled":
            pvs = self.query_package_versions(packages)
            for p, v in list(pvs.items()):
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

    def query_package_versions(self, packages):
        """
        :type packages: [str]
        :param packages: packages to query versions for

        :return: {package: RPMVersion or None}
            where
              package = str
        """
        self.run(
            'rpm -q --queryformat "%{{Name}} %{{Version}}-%{{Release}}\n" {}'.format(
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

    def disable_repo(self, repo):
        logger.debug("{}: disabling repo {}".format(self.hostname, repo))
        self.run("zypper mr -d {}".format(repo))

    def enable_repo(self, repo):
        logger.debug("{}: enabling repo {}".format(self.hostname, repo))
        self.run("zypper mr -e {}".format(repo))

    def set_timeout(self, value):
        logger.debug("{}: setting timeout to {}".format(self.hostname, value))
        self.connection.timeout = value

    def get_timeout(self):
        return self.connection.timeout

    def get_system(self):
        return str(self.system)

    def set_repo(self, operation, testreport):
        logger.debug("{}: enabling {} repos".format(self.hostname, operation))
        testreport.set_repo(self, operation)

    def run_zypper(self, cmd, repos, rrid):
        # ur - generator returning tuple with product, repopart
        ur = ((x, y) for x, y in repos.items() if x in self.system.flatten())

        def name(product, rrid):
            return "issue-{}:{}:p={}".format(
                product.name, product.version, rrid.maintenance_id
            )

        def fullpath(path, rrid):
            # TODO: confiruable download path?
            dl_path = "http://download.suse.de/ibs/"
            return dl_path + "/" + ":/".join(str(rrid).split(":")[:-1]) + "/" + path

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

    def run(self, command, lock=None):
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

    def lastin(self):
        try:
            return self.out[-1][0]
        except BaseException:
            return ""

    def lastout(self):
        try:
            return self.out[-1][1]
        except BaseException:
            return ""

    def lasterr(self):
        try:
            return self.out[-1][2]
        except BaseException:
            return ""

    def lastexit(self):
        try:
            return self.out[-1][3]
        except BaseException:
            return ""

    def is_locked(self):
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

    def locked(self):
        """
        :deprecated: by is_locked method
        """
        logger.debug("{!s}: getting mtui lock state".format(self.hostname))
        lock = Locked(self.config.session_user, False)

        if self.state != "enabled":
            return lock

        try:
            lock.locked = self._lock.is_locked()
        except Exception:
            logger.error("Reading remote lock failed for {0}".format(self.host))
            return lock

        if lock.locked:
            rl = self._lock.locked_by()
            lock.timestamp = rl.timestamp
            lock.user = rl.user
            lock.pid = str(rl.pid)
            lock.comment = rl.comment

        return lock

    def set_locked(self, comment=None):
        """
        :deprecated: by lock method
        """
        if self.state == "enabled":
            try:
                self._lock.lock(comment)
            except BaseException:
                return

    def remove_lock(self):
        """
        :deprecated:
        """
        if self.state != "enabled":
            return

        try:
            self.unlock()
        except TargetLockedError:
            logger.debug(
                "unable to remove lock from {}. lock is probably not held by this session".format(
                    self.hostname
                )
            )
        except BaseException:
            pass

    def add_history(self, comment):
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
                self.add_history(["disconnect"])
                self.remove_lock()
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
        return sink(self.hostname, self.system, self.locked())

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


class Package(object):
    def __init__(self, name):
        self.name = name
        self.before = None
        self.after = None
        self.required = None
        self.current = None

    def set_versions(self, before=None, after=None, required=None):
        if before:
            self.before = before
        if after:
            self.after = after
        if required:
            self.required = required
