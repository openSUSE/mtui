# -*- coding: utf-8 -*-
#
# target host management. this is right above the ssh/transmission layer and
# below the abstractions layer (like updating, preparing, etc.)
#

from __future__ import print_function

import os
import sys
import re
import signal
from traceback import format_exc

from mtui.connection import *
from mtui.xmlout import *

from mtui import utils

from mtui.utils import *
from mtui.config import *
from mtui.rpmver import RPMVersion
from mtui import messages
from mtui.messages import HostIsNotConnectedError

from mtui.target.actions import FileDelete
from mtui.target.actions import FileDownload
from mtui.target.actions import FileUpload
from mtui.target.actions import RunCommand
from mtui.target.actions import ThreadedMethod
from mtui.target.actions import queue
from mtui.target.actions import spinner

from mtui.target.locks import Locked
from mtui.target.locks import LockedTargets
from mtui.target.locks import RemoteLock
from mtui.target.locks import TargetLock
from mtui.target.locks import TargetLockedError


class HostsGroupException(Exception):
    def __init__(self, es):
        self.es = es
        msg = "\n".join([str(x) for x in es])
        Exception.__init__(self, msg)

    def handle(self, xs):
        new = []
        for e in self.es:
            handled = False
            for x in xs:
                if x[0](e):
                    x[1](e)
                    handled = True
            if not handled:
                new.append(e)
        if new:
            raise HostsGroupException(new)

class HostsGroup(object):
    """
    Composite pattern for L{Target}

    doesn't deal with Target state as that would require too much work
    to support properly. so

    1. All the given hosts are expected to be enabled.

    2. Lifetime of the object should be the same as execution of one
       command given from user (to ensure 1.)
    """
    def __init__(self, hosts):
        """
        :param targets: list of L{Target}
        """
        self.hosts = dict([(h.host, h) for h in hosts])

    def select(self, hosts = [], enabled = None):
        if hosts == []:
            if enabled:
                return HostsGroup(filter(
                  lambda h: h.state != 'disabled'
                , self.hosts.values()
                ))
            return self

        for x in hosts:
            if not x in self.hosts:
                raise HostIsNotConnectedError(x)

        return HostsGroup([
            h for hn, h in self.hosts.items()
                if hn in hosts and ((not enabled) or h.state != 'disabled')
        ])

    def unlock(self, *a, **kw):
        es = []
        for x in self.hosts.values():
            try:
                x.unlock(*a, **kw)
            except Exception as e:
                es.append(e)

        if not es == []:
            raise HostsGroupException(es)

    def query_versions(self, packages):
        rs = {}
        for x in self.hosts.values():
            rs[x] = x.query_package_versions(packages)

        return rs

    def add_history(self, data):
        for tgt in self.hosts.values():
            tgt.add_history(data)

    def names(self):
        return list(self.hosts.keys())

    def get(self, remote, local):
        return FileDownload(self.hosts.values(), remote, local, True).run()

    def put(self, local, remote):
        return FileUpload(self.hosts.values(), local, remote).run()

    def remove(self, path):
        return FileDelete(self.hosts.values(), path).run()

    def run(self, cmd):
        return RunCommand(self.hosts, cmd).run()

    ## dict interface

    def __getitem__(self, x):
        return self.hosts[x]

    def __setitem__(self, k, v):
        self.hosts[k] = v

    def __iter__(self):
        return self.hosts.__iter__()

    def __len__(self):
        return len(self.hosts)

    def copy(self):
        return HostsGroup(self.hosts.values())

    def items(self):
        return self.hosts.items()

    def keys(self):
        return self.hosts.keys()

    def pop(self, *a, **kw):
        return self.hosts.pop(*a, **kw)

    def update(self, *a, **kw):
        return self.hosts.update(*a, **kw)

    def values(self):
        return self.hosts.values()


class Target(object):
    def __init__(self, hostname, system, packages=[], state='enabled',
        timeout=300, exclusive=False, connect=True, logger=None,
        lock=TargetLock, connection=Connection):
        """
            :type connect: bool
            :param connect:
                introduced in order to run unit tests witout
                having the target automatically connect
        """

        self.host, _, self.port = hostname.partition(':')
        self.hostname = hostname
        self.system = system
        self.packages = {}
        self.log = []
        self.TargetLock = lock
        self.Connection = connection

        self.logger = logger
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

        if connect:
            self.connect()

    def connect(self):
        try:
            self.logger.info('connecting to %s' % self.hostname)
            self.connection = self.Connection(self.host, self.port, self.timeout)
        except Exception as e:
            self.logger.critical(messages.ConnectingTargetFailedMessage(
                self.hostname, e
            ))
            raise

        self._lock = self.TargetLock(self.connection, config, self.logger)
        if self.is_locked():
            # NOTE: the condition was originally locked and lock.comment
            # idk why.
            self.logger.warning(self._lock.locked_by_msg())

    def __lt__(self, other):
        return sorted([self.system, other.system])[0] == self.system

    def __gt__(self, other):
        return sorted([self.system, other.system])[0] == other.system

    def __eq__(self, other):
        return self.system == other.system

    def __ne__(self, other):
        return self.system != other.system

    def query_versions(self, packages=None):
        versions = {}
        if packages is None:
            packages = list(self.packages.keys())

        if self.state == 'enabled':
            pvs = self.query_package_versions(packages)
            for p, v in pvs.items():
                if v:
                    self.packages[p].current = str(v)
                else:
                    self.packages[p].current = '0'
        elif self.state == 'dryrun':

            self.logger.info('dryrun: %s running "rpm -q %s"' % (self.hostname, packages))
            self.log.append(['rpm -q %s' % packages, 'dryrun\n', '', 0, 0])
        elif self.state == 'disabled':

            self.log.append(['', '', '', 0, 0])


    def query_package_versions(self, packages):
        """
        :type packages: [str]
        :param packages: packages to query versions for

        :return: {package: RPMVersion or None}
            where
              package = str
        """
        self.run('rpm -q {0}'.format(' '.join(packages)))

        packages = {}
        for line in re.split('\n+', self.lastout()):
            match = re.search(r"^([a-zA-Z0-9_\-\+\.]*)-([a-zA-Z0-9_\.+]*)-([a-zA-Z0-9_\.]*)", line)
            if match:
                packages[match.group(1)] = RPMVersion('{0}-{1}'.format(
                    match.group(2),
                    match.group(3)
                ))
                continue

            match = re.search('package (.*) is not installed', line)
            if match:
                packages[match.group(1)] = None

        return packages

    def query_version(self, package):
        self.logger.debug('%s: querying current %s version' % (self.hostname, package))
        self.query_versions(package)
        return self.packages[package].current

    def disable_repo(self, repo):
        self.logger.debug('%s: disabling repo %s' % (self.hostname, repo))
        self.run('zypper mr -d %s' % repo)

    def enable_repo(self, repo):
        self.logger.debug('%s: enabling repo %s' % (self.hostname, repo))
        self.run('zypper mr -e %s' % repo)

    def set_timeout(self, value):
        self.logger.debug('%s: setting timeout to %s' % (self.hostname, value))
        self.connection.timeout = value

    def get_timeout(self):
        return self.connection.timeout

    def _upload_repclean(self):
        """copy over local rep-clean script"""
        datadir = config.datadir
        tempdir = config.target_tempdir
        try:
            for item in ['rep-clean.sh', 'rep-clean.conf']:
                filename = os.path.join(
                    datadir, 'helper', 'rep-clean', item)
                destination = os.path.join(tempdir, item)
                self.put(filename, destination)
        except Exception as e:
            msg = "rep-clean uploading failed"
            msg += " please see BNC#860284"
            self.logger.error(msg)

        scriptfile = os.path.join(tempdir, 'rep-clean.sh')
        conffile = os.path.join(tempdir, 'rep-clean.conf')
        return (scriptfile, conffile)

    def set_repo(self, name, testreport = None):
        if name not in ["UPDATE", "TESTING"]:
            raise ValueError("invalid name `%s`" % name)

        command = config.repclean_path

        try:
            repclean = self.connection.open(command, 'r')
        except IOError:
            x = self._upload_repclean()
            command = '{0} -F {1}'.format(*x)
        else:
            repclean.close()

        if utils.get_release([self.system]) == '12':
            if name == "TESTING" and not testreport:
                raise RuntimeError("Target.set_repo can't be used without testreport on sle12 systems")

            cmd = "{repclean} -z"

            if name == "TESTING":
                cmd += "; {repclean} -i {incident_id}"

            cmd = cmd.format(
                repclean = command,
                incident_id = testreport.rrid.maintenance_id if testreport else None
            )

        else:
            params = dict(
                TESTING = '-t',
                UPDATE  = '-n'
            )

            cmd = "{0} {1}".format(command, params[name])

        self.logger.debug('{0}: enabling {1} repos'.format(self.hostname, name))
        self.run(cmd)

    def run(self, command, lock=None):
        if self.state == 'enabled':
            self.logger.debug('%s: running "%s"' % (self.hostname, command))
            time_before = timestamp()
            try:
                exitcode = self.connection.run(command, lock)
            except CommandTimeout:
                self.logger.critical('%s: command "%s" timed out' % (self.hostname, command))
                exitcode = -1
            except AssertionError:
                self.logger.debug('zombie command terminated')
                self.logger.debug(format_exc())
                return
            except Exception:
                # failed to run command
                self.logger.error('%s: failed to run command "%s"' % (self.hostname, command))
                exitcode = -1

            time_after = timestamp()
            runtime = int(time_after) - int(time_before)
            self.log.append([command, self.connection.stdout, self.connection.stderr, exitcode, runtime])
        elif self.state == 'dryrun':

            self.logger.info('dryrun: %s running "%s"' % (self.hostname, command))
            self.log.append([command, 'dryrun\n', '', 0, 0])
        elif self.state == 'disabled':

            self.log.append(['', '', '', 0, 0])

    def shell(self):
        self.logger.debug('%s: spawning shell' % self.hostname)

        try:
            self.connection.shell()
        except Exception:
            # failed to spawn shell
            self.logger.error('%s: failed to spawn shell')

    def put(self, local, remote):
        if self.state == 'enabled':
            self.logger.debug('%s: sending "%s"' % (self.hostname, local))
            try:
                return self.connection.put(local, remote)
            except EnvironmentError as error:
                self.logger.error('%s: failed to send %s: %s' % (self.hostname, local, error.strerror))
        elif self.state == 'dryrun':
            self.logger.info('dryrun: put %s %s:%s' % (local, self.hostname, remote))

    def get(self, remote, local):
        if self.state == 'enabled':
            self.logger.debug('%s: receiving "%s"' % (self.hostname, remote))
            try:
                return self.connection.get(remote, local)
            except EnvironmentError as error:
                self.logger.error('%s: failed to get %s: %s' % (self.hostname, remote, error.strerror))
        elif self.state == 'dryrun':
            self.logger.info('dryrun: get %s:%s %s' % (self.hostname, remote, local))

    def lastin(self):
        try:
            return self.log[-1][0]
        except:
            return ''

    def lastout(self):
        try:
            return self.log[-1][1]
        except:
            return ''

    def lasterr(self):
        try:
            return self.log[-1][2]
        except:
            return ''

    def lastexit(self):
        try:
            return self.log[-1][3]
        except:
            return ''

    def lastruntime(self):
        try:
            return self.log[-1][4]
        except:
            return ''

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
        self._lock.unlock(force)

    def locked(self):
        """
        :deprecated: by is_locked method
        """
        self.logger.debug('%s: getting mtui lock state' % self.hostname)
        lock = Locked(self.logger, False)

        if self.state != 'enabled':
            return lock

        try:
            lock.locked = self._lock.is_locked()
        except Exception:
            self.logger.error("Reading remote lock failed for {0}".\
                format(self.host))
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
        if self.state == 'enabled':
            try:
                self._lock.lock(comment)
            except:
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
            self.logger.debug('unable to remove lock from %s. lock is probably not held by this session' % self.hostname)
        except:
            pass

    def add_history(self, comment):
        if self.state == 'enabled':
            self.logger.debug('%s: adding history entry' % self.hostname)
            try:
                filename = os.path.join('/', 'var', 'log', 'mtui.log')
                historyfile = self.connection.open(filename, 'a+')
            except Exception as error:
                self.logger.error('failed to open history file: %s' % error)
                return

            now = timestamp()
            user = config.session_user
            try:
                historyfile.write('%s:%s:%s\n' % (now, user, ':'.join(comment)))
                historyfile.close()
            except Exception:
                pass

    def listdir(self, path):
        try:
            return self.connection.listdir(path)
        except IOError as error:
            if error.errno == errno.ENOENT:
                self.logger.debug('%s: directory %s does not exist' % (self.hostname, path))
            return []

    def remove(self, path):
        try:
            self.connection.remove(path)
        except IOError as error:
            if error.errno == errno.ENOENT:
                self.logger.debug('%s: path %s does not exist' % (self.hostname, path))
            else:
                try:
                    # might be a directory
                    self.connection.rmdir(path)
                except IOError:
                    self.logger.warning('unable to remove %s on %s' % (path, self.hostname))

    def close(self, action=None):
        def alarm_handler(signum, frame):
            self.logger.warning('timeout reached on %s' % self.hostname)
            raise CommandTimeout('close')

        handler = signal.signal(signal.SIGALRM, alarm_handler)
        signal.alarm(15)

        try:
            assert(self.connection)

            if self.connection.is_active():
                self.add_history(['disconnect'])
                self.remove_lock()
        except Exception:
            # ignore if the connection seems to be lost
            pass
        else:
            if action == 'reboot':
                self.logger.info('rebooting %s' % self.hostname)
                self.run('reboot')
            elif action == 'poweroff':
                self.logger.info('powering off %s' % self.hostname)
                self.run('halt')
            else:
                self.logger.info('closing connection to %s' % self.hostname)

        if self.connection:
            self.connection.close()
            self.connection = None

        # restoring signal handler
        signal.alarm(0)
        signal.signal(signal.SIGALRM, handler)

        return


class Package(object):

    def __init__(self, name):
        self.name = name
        self.before = None
        self.after = None
        self.required = None
        self.current = None

    def set_versions(self, before=None, after=None, required=None):
        if before is not None:
            self.before = before
        if after is not None:
            self.after = after
        if required is not None:
            self.required = required


