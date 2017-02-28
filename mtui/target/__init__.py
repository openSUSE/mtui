# -*- coding: utf-8 -*-
#
# target host management. this is right above the ssh/transmission layer and
# below the abstractions layer (like updating, preparing, etc.)
#



import os
import re
import signal
import subprocess
from traceback import format_exc

from mtui.connection import Connection
from mtui.connection import errno
from mtui.connection import CommandTimeout

from mtui.rpmver import RPMVersion
from mtui import messages
from mtui.messages import HostIsNotConnectedError

from mtui.target.actions import FileDelete
from mtui.target.actions import FileDownload
from mtui.target.actions import FileUpload
from mtui.target.actions import RunCommand

from mtui.target.locks import Locked

from mtui.target.locks import TargetLock
from mtui.target.locks import TargetLockedError

# Import for other modules -- not used directly here
from mtui.target.locks import LockedTargets
from mtui.target.locks import RemoteLock

from mtui.utils import timestamp


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

    def select(self, hosts=[], enabled=None):
        if hosts == []:
            if enabled:
                return HostsGroup([h for h in list(self.hosts.values()) if h.state != 'disabled'])
            return self

        for x in hosts:
            if x not in self.hosts:
                raise HostIsNotConnectedError(x)

        return HostsGroup([
            h for hn, h in list(self.hosts.items())
            if hn in hosts and ((not enabled) or h.state != 'disabled')
        ])

    def unlock(self, *a, **kw):
        for x in list(self.hosts.values()):
            try:
                x.unlock(*a, **kw)
            except TargetLockedError:
                pass  # logged in Target#unlock

    def lock(self, *a, **kw):
        for x in list(self.hosts.values()):
            try:
                x.lock(*a, **kw)
            except TargetLockedError:
                pass

    def query_versions(self, packages):
        rs = []
        for x in list(self.hosts.values()):
            rs.append((x, x.query_package_versions(packages)))

        return rs

    def add_history(self, data):
        for tgt in list(self.hosts.values()):
            tgt.add_history(data)

    def names(self):
        return list(self.hosts.keys())

    def get(self, remote, local):
        return FileDownload(list(self.hosts.values()), remote, local).run()

    def put(self, local, remote):
        return FileUpload(list(self.hosts.values()), local, remote).run()

    def remove(self, path):
        return FileDelete(list(self.hosts.values()), path).run()

    def run(self, cmd):
        return self._run(cmd)

    def _run(self, cmd):
        return RunCommand(self.hosts, cmd).run()

    def report_self(self, sink):
        for hn in sorted(self.hosts.keys()):
            self.hosts[hn].report_self(sink)

    def report_history(self, sink, count, events):
        if events:
            self._run('tac /var/log/mtui.log | grep -m {} {} | tac'.format(
                count,
                ' '.join([('-e ":{}"'.format(e)) for e in events]),
            ))
        else:
            self._run('tail -n {} /var/log/mtui.log'.format(count))

        for hn in sorted(self.hosts.keys()):
            self.hosts[hn].report_history(sink)

    def report_locks(self, sink):
        for hn in sorted(self.hosts.keys()):
            self.hosts[hn].report_locks(sink)

    def report_timeout(self, sink):
        for hn in sorted(self.hosts.keys()):
            self.hosts[hn].report_timeout(sink)

    def report_sessions(self, sink):
        for hn in sorted(self.hosts.keys()):
            self.hosts[hn].report_sessions(sink)

    def report_log(self, sink, arg):
        for hn in sorted(self.hosts.keys()):
            self.hosts[hn].report_log(sink, arg)

    def report_testsuites(self, sink, arg):
        for hn in sorted(self.hosts.keys()):
            self.hosts[hn].report_testsuites(sink, arg)

    def report_testsuite_results(self, sink, arg):
        for hn in sorted(self.hosts.keys()):
            self.hosts[hn].report_testsuite_results(sink, arg)

    # dict interface

    def __contains__(self, k):
        return k in self.hosts

    def __delitem__(self, x):
        del self.hosts[x]

    def __getitem__(self, x):
        return self.hosts[x]

    def __setitem__(self, k, v):
        self.hosts[k] = v

    def __iter__(self):
        return self.hosts.__iter__()

    def __len__(self):
        return len(self.hosts)

    def copy(self):
        return HostsGroup(list(self.hosts.values()))

    def items(self):
        return list(self.hosts.items())

    def keys(self):
        return list(self.hosts.keys())

    def pop(self, *a, **kw):
        return self.hosts.pop(*a, **kw)

    def update(self, *a, **kw):
        return self.hosts.update(*a, **kw)

    def values(self):
        return list(self.hosts.values())


class Target(object):

    def __init__(self, config, hostname, system, packages=[], state='enabled',
                 timeout=300, exclusive=False, connect=True, logger=None,
                 lock=TargetLock, connection=Connection):
        """
            :type connect: bool
            :param connect:
                introduced in order to run unit tests witout
                having the target automatically connect
        """

        self.config = config
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
            self.logger.info('connecting to {}'.format(self.hostname))
            self.connection = self.Connection(
                self.logger,
                self.host,
                self.port,
                self.timeout)
        except Exception as e:
            self.logger.critical(messages.ConnectingTargetFailedMessage(
                self.hostname, e
            ))
            raise

        self._lock = self.TargetLock(self.connection, self.config, self.logger)
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
        if packages is None:
            packages = list(self.packages.keys())

        if self.state == 'enabled':
            pvs = self.query_package_versions(packages)
            for p, v in list(pvs.items()):
                if v:
                    self.packages[p].current = str(v)
                else:
                    self.packages[p].current = '0'
        elif self.state == 'dryrun':

            self.logger.info(
                'dryrun: {} running "rpm -q {}"'.format(self.hostname, packages))
            self.log.append(
                ['rpm -q {}'.format(packages), 'dryrun\n', '', 0, 0])
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
        self.run(
            'rpm -q --queryformat "%{{Name}} %{{Version}}-%{{Release}}\n" {}'.
            format(' '.join(packages)))

        packages = {}
        for line in self.lastout().splitlines():
            match = re.search('package (.*) is not installed', line)
            if match:
                packages[match.group(1)] = None
                continue
            p, v = line.split()
            packages[p] = RPMVersion(v)
        return packages

    def disable_repo(self, repo):
        self.logger.debug('{}: disabling repo {}'.format(self.hostname, repo))
        self.run('zypper mr -d {}'.format(repo))

    def enable_repo(self, repo):
        self.logger.debug('{}: enabling repo {}'.format(self.hostname, repo))
        self.run('zypper mr -e {}'.format(repo))

    def set_timeout(self, value):
        self.logger.debug(
            '{}: setting timeout to {}'.format(
                self.hostname,
                value))
        self.connection.timeout = value

    def get_timeout(self):
        return self.connection.timeout

    def set_repo(self, operation, testreport):
        self.logger.debug(
            '{}: enabling {} repos'.format(
                self.hostname,
                operation))
        testreport.set_repo(self, operation)

    def run_repose(self, cmd, arg):
        cmdline = [
            'repose',
            cmd,
            ('root@{}'.format(str(self.hostname),)),
            '--',
            arg,
        ]
        self.logger.info(
            "local/:{} {}".format(self.hostname, ' '.join(cmdline)))
        cld = subprocess.Popen(
            cmdline,
            close_fds=True,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        cld.stdin.close()
        ex = cld.wait()
        logger = self.logger.info if ex == 0 else self.logger.error
        for label, data in (('stdout', cld.stdout), ('stderr', cld.stderr)):
            for l in data:
                logger(
                    "local/{} {}: {}".format(self.hostname, label, l.rstrip()))

    def run(self, command, lock=None):
        if self.state == 'enabled':
            self.logger.debug(
                '{}: running "{}"'.format(
                    self.hostname,
                    command))
            time_before = timestamp()
            try:
                exitcode = self.connection.run(command, lock)
            except CommandTimeout:
                self.logger.critical(
                    '{}: command "{}" timed out'.format(
                        self.hostname,
                        command))
                exitcode = -1
            except AssertionError:
                self.logger.debug('zombie command terminated')
                self.logger.debug(format_exc())
                return
            except Exception:
                # failed to run command
                self.logger.error(
                    '{}: failed to run command "{}"'.format(
                        self.hostname,
                        command))
                exitcode = -1

            time_after = timestamp()
            runtime = int(time_after) - int(time_before)
            self.log.append([command,
                             self.connection.stdout,
                             self.connection.stderr,
                             exitcode,
                             runtime])
        elif self.state == 'dryrun':

            self.logger.info(
                'dryrun: {} running "{}"'.format(self.hostname, command))
            self.log.append([command, 'dryrun\n', '', 0, 0])
        elif self.state == 'disabled':

            self.log.append(['', '', '', 0, 0])

    def shell(self):
        self.logger.debug('{}: spawning shell'.format(self.hostname))

        try:
            self.connection.shell()
        except Exception:
            # failed to spawn shell
            self.logger.error(
                '{}: failed to spawn shell'.format(
                    self.hostname))

    def put(self, local, remote):
        if self.state == 'enabled':
            self.logger.debug('{}: sending "{}"'.format(self.hostname, local))
            try:
                return self.connection.put(local, remote)
            except EnvironmentError as error:
                self.logger.error(
                    '{}: failed to send {}: {}'.format(
                        self.hostname,
                        local,
                        error.strerror))
        elif self.state == 'dryrun':
            self.logger.info(
                'dryrun: put {} {}:{}'.format(local, self.hostname, remote))

    def get(self, remote, local):

        if remote.endswith('/'):
            f = self.connection.get_folder
            s = 'folder'
        else:
            f = self.connection.get
            s = 'file'
            local = '{}.{}'.format(local, self.hostname)

        if self.state == 'enabled':
            self.logger.debug(
                '{}: receiving {} "{}" into "{}'.format(
                    self.hostname,
                    s,
                    remote,
                    local))
            try:
                return f(remote, local)
            except EnvironmentError as error:
                self.logger.error(
                    '{}: failed to get {} {}: {}'.format(
                        self.hostname,
                        s,
                        remote,
                        error.strerror))
        elif self.state == 'dryrun':
            self.logger.info(
                'dryrun: get {} {}:{} {}'.format(
                    self.hostname,
                    s,
                    remote,
                    local))

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
            self.logger.warning(e)
            raise

    def locked(self):
        """
        :deprecated: by is_locked method
        """
        self.logger.debug('{!s}: getting mtui lock state'.format(self.hostname))
        lock = Locked(self.logger, self.config.session_user, False)

        if self.state != 'enabled':
            return lock

        try:
            lock.locked = self._lock.is_locked()
        except Exception:
            self.logger.error("Reading remote lock failed for {0}".
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
            self.logger.debug(
                'unable to remove lock from {}. lock is probably not held by this session'. format(
                    self.hostname))
        except:
            pass

    def add_history(self, comment):
        if self.state == 'enabled':
            self.logger.debug('{}: adding history entry'.format(self.hostname))
            try:
                filename = os.path.join('/', 'var', 'log', 'mtui.log')
                historyfile = self.connection.open(filename, 'a+')
            except Exception as error:
                self.logger.error(
                    'failed to open history file: {}'.format(error))
                return

            now = timestamp()
            user = self.config.session_user
            try:
                historyfile.write(
                    '{}:{}:{}\n'.format(now, user, ':'.join(comment)))
                historyfile.close()
            except Exception:
                pass

    def listdir(self, path):
        try:
            return self.connection.listdir(path)
        except IOError as error:
            if error.errno == errno.ENOENT:
                self.logger.debug(
                    '{}: directory {} does not exist'.format(
                        self.hostname,
                        path))
            return []

    def remove(self, path):
        try:
            self.connection.remove(path)
        except IOError as error:
            if error.errno == errno.ENOENT:
                self.logger.debug(
                    '{}: path {} does not exist'.format(self.hostname, path))
            else:
                try:
                    # might be a directory
                    self.connection.rmdir(path)
                except IOError:
                    self.logger.warning(
                        'unable to remove {} on {}'.format(
                            path,
                            self.hostname))

    def close(self, action=None):
        def alarm_handler(signum, frame):
            self.logger.warning('timeout reached on {}'.format(self.hostname))
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
                self.logger.info('rebooting {}'.format(self.hostname))
                self.run('reboot')
            elif action == 'poweroff':
                self.logger.info('powering off {}'.format(self.hostname))
                self.run('halt')
            else:
                self.logger.info(
                    'closing connection to {}'.format(
                        self.hostname))

        if self.connection:
            self.connection.close()
            self.connection = None

        # restoring signal handler
        signal.alarm(0)
        signal.signal(signal.SIGALRM, handler)

        return

    def report_self(self, sink):
        return sink(self.hostname, self.system, self.state, self.exclusive)

    def report_history(self, sink):
        return sink(self.hostname, self.system, self.lastout().split('\n'))

    def report_locks(self, sink):
        return sink(self.hostname, self.system, self.locked())

    def report_timeout(self, sink):
        return sink(self.hostname, self.system, self.get_timeout())

    def report_sessions(self, sink):
        return sink(self.hostname, self.system, self.lastout())

    def report_log(self, sink, arg):
        return sink(self.hostname, self.log, arg)

    def report_testsuites(self, sink, suitedir):
        return sink(self.hostname, self.system, self.listdir(suitedir))

    def report_testsuite_results(self, sink, suitename):
        return sink(
            self.hostname,
            self.lastexit(),
            self.lastout(),
            self.lasterr(),
            suitename)


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
