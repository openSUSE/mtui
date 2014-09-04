# -*- coding: utf-8 -*-
#
# target host management. this is right above the ssh/transmission layer and
# below the abstractions layer (like updating, preparing, etc.)
#

from __future__ import print_function

import os
import sys
import re
import threading
try:
    from queue import Queue
except ImportError:
    from Queue import Queue
import signal
import logging
import getpass
from traceback import format_exc

from mtui.connection import *
from mtui.xmlout import *
from mtui.utils import *
from mtui.config import *
from mtui import messages

out = logging.getLogger('mtui')

queue = Queue()

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
        self.hosts = hosts

    def select(self, hosts):
        if hosts == []:
            return self

        available = [x.host for x in self.hosts]
        for x in hosts:
            if not x in available:
                m = "Host {0} is not connected".format(repr(x))
                raise ValueError(m)

        return HostsGroup([x for x in self.hosts if x.host in hosts])

    def unlock(self, *a, **kw):
        es = []
        for x in self.hosts:
            try:
                x.unlock(*a, **kw)
            except Exception as e:
                es.append(e)

        if not es == []:
            raise HostsGroupException(es)

    def __getitem__(self, x):
        return self.hosts[x]

    def names(self):
        return [x.hostname for x in self]

class TargetLockedError(Exception):
    pass

class RemoteLock(object):
    """
    Localy represent the state of remote lock
    """
    def __init__(self):
        self.user = None
        """
        :param user: user owning the lock
        :type user: str or None
        """
        self.timestamp = None
        """
        :param timestamp: timestamp when the lock was set
        :type timestamp: str or None
        """
        self.pid = None
        """
        :param pid: pid of owning the lock
        :type pid: int or None
        """
        self.comment = None
        """
        :param comment: comment why the lock was set
        :type comment: str or None
        """

    def to_lockfile(self):
        """
        :return: str representation of self to be written in the
            lockfile
        """
        xs = [self.timestamp, self.user, str(self.pid)]
        if self.comment:
            xs.append(self.comment)
        return ":".join(xs)

    def __str__(self):
        if self.comment:
            comment = " (%s)" % self.comment
        else:
            comment = ""

        user = self.user
        return "locked by {0}{1}.".format(user, comment)

    @classmethod
    def from_lockfile(cls, line):
        """
        :return: L{RemoteLock} instance
        """
        self = cls()

        if line=="":
            return self

        line = line.strip()
        line = line.split(":")
        if len(line) is 4:
            self.comment = line.pop()

        self.pid = int(line.pop())
        self.user = line.pop()
        self.timestamp = line.pop()

        if line:
            raise ValueError('got weird format in lockfile')

        return self

class LockedTargets(object):
    def __init__(self, targets):
        self.targets = targets

    def __enter__(self):
        for target in self.targets:
            target.lock()

    def __exit__(self, type_, value, tb):
        for target in self.targets:
            target.unlock()

class TargetLock(object):
    """
        This class is not supposted to be used directly but via
        L{Target} methods

        If the lock has comment, it is considered to be an `exclusive`
        lock. Only place that takes this into consideration is `run`
        command.
    """
    # FIXME: use netstrings to ensure proper (de)serialization
    # NOTE: the user name is not guaranteed not to collide.
    # Unfortunately, I don't see a way to do this without unreasonably
    # raising the logic complexity and usability

    filename = os.path.join('/', 'var', 'lock', 'mtui.lock')

    def __init__(self, connection, config, out=None):
        self.connection = connection

        if not out:
            out = logging.getLogger("mtui")

        self.out = out
        self.connection = connection

        self.i_am_user = config.session_user
        self.i_am_pid  = os.getpid()
        self.timestamp_factory = timestamp
        """
        :type timestampFactory: callable
        """

        self._lock = RemoteLock()

    def load(self):
        """
        :returns None:
        """
        self.out.debug('%s: getting mtui lock state' %
            self.connection.hostname)

        self._lock = RemoteLock() # make sure lock is reset.

        try:
            lockfile = self.connection.open(self.filename)
        except EnvironmentError as error:
            if error.errno != errno.ENOENT:
                raise
            data = ""
        else:
            data = lockfile.readline()
            lockfile.close()

        self._lock = RemoteLock.from_lockfile(data)

    def is_locked(self):
        """
        :returns: bool True if target system is locked by someone else

        If possible use `try: lock.lock(); ...` as this introduces race
        condition that's fundamentally impossible to remove.
        """
        self.load()
        return bool(self._lock.user)

    def lock(self, comment=None):
        """
        Locks the target system

        :returns: None
        :raises TargetLockedError: if target is already locked.
        """
        if self.is_locked():
            # NOTE: there is a slight race between between getting the
            # state of the lock on target host and setting the lock.
            # However, that has always been here afaik.
            # TODO: test if using sftpclient.mkdir can be used to make
            # the locking really atomic.
            if not self.is_mine():
                # NOTE: let the code pass through if is_mine() as
                # setting a different comment may be desired.
                raise TargetLockedError(self.locked_by_msg())

        out.debug('%s: setting lock' % self.connection.hostname)

        rl = RemoteLock()
        rl.user = self.i_am_user
        rl.timestamp = self.timestamp_factory()
        rl.pid = self.i_am_pid
        rl.comment = comment

        try:
            lockfile = self.connection.open(self.filename, 'w+')
        except Exception as e:
            out.error('failed to open lockfile: %s' % e)
            raise

        lockfile.write(rl.to_lockfile())
        lockfile.close()
        self._lock = rl

    def locked_by_msg(self):
        """
        :returns str: locked by message suitable for display to user
        """
        host = self.connection.hostname
        return "{0} is {1}".format(host, str(self._lock))

    def locked_by(self):
        return self._lock

    def unlock(self, force=False):
        """
        Unlocks target system

        :param force: bool if False (default) removes only locks owned
            by current user. If True removes locks owned by anyone
            Usefull when mtui crashes (and therefore you don't own your
            locks anymore due to different pid) or someone elses mtui
            hangs and you need to access the systems
        """
        if not self.is_locked():
            return

        if not self.is_mine() and not force:
            raise TargetLockedError(self.locked_by_msg())

        try:
            self.connection.remove(self.filename)
        except IOError as e:
            if e.errno == errno.ENOENT:
                pass
        except Exception as e:
            out.error('failed to remove lockfile: %s' % e)
            raise

        self._lock = RemoteLock()

    def is_mine(self):
        """
        :returns bool: True if the lock is owned by user running this
        """
        if not self._lock.user:
            raise RuntimeError("not locked")

        if self._lock.user != self.i_am_user:
            return False
        if self._lock.pid != self.i_am_pid:
            # NOTE: checking pid handles the case where one user is
            # running multiple mtui instances against the same hosts
            return False

        return True

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

        if logger is None:
            # for backwards compatibility
            logger = out
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
            packages = self.packages.keys()

        if isinstance(packages, list):
            packages = ' '.join(packages)

        if self.state == 'enabled':
            self.run('rpm -q %s' % packages)

            for line in re.split('\n+', self.lastout()):
                match = re.search(r"^([a-zA-Z0-9_\-\+\.]*)-([a-zA-Z0-9_\.+]*)-([a-zA-Z0-9_\.]*)", line)
                if match:
                    self.packages[match.group(1)].current = '%s-%s' % (match.group(2), match.group(3))
                else:
                    match = re.search('package (.*) is not installed', line)
                    if match:
                        self.packages[match.group(1)].current = '0'
                        out.debug('%s: package %s is not installed' % (self.hostname, match.group(1)))
        elif self.state == 'dryrun':

            out.info('dryrun: %s running "rpm -q %s"' % (self.hostname, packages))
            self.log.append(['rpm -q %s' % packages, 'dryrun\n', '', 0, 0])
        elif self.state == 'disabled':

            self.log.append(['', '', '', 0, 0])

    def query_version(self, package):
        out.debug('%s: querying current %s version' % (self.hostname, package))
        self.query_versions(package)
        return self.packages[package].current

    def disable_repo(self, repo):
        out.debug('%s: disabling repo %s' % (self.hostname, repo))
        self.run('zypper mr -d %s' % repo)

    def enable_repo(self, repo):
        out.debug('%s: enabling repo %s' % (self.hostname, repo))
        self.run('zypper mr -e %s' % repo)

    def set_timeout(self, value):
        out.debug('%s: setting timeout to %s' % (self.hostname, value))
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

    def set_repo(self, name):
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

        if name == 'TESTING':
            out.debug('%s: enabling TESTING repos' % self.hostname)
            parameter = '-t'
        elif name == 'UPDATE':
            out.debug('%s: enabling UPDATE repos' % self.hostname)
            parameter = '-n'

        self.run('%s %s' % (command, parameter))

    def run(self, command, lock=None):
        if self.state == 'enabled':
            out.debug('%s: running "%s"' % (self.hostname, command))
            time_before = timestamp()
            try:
                exitcode = self.connection.run(command, lock)
            except CommandTimeout:
                out.critical('%s: command "%s" timed out' % (self.hostname, command))
                exitcode = -1
            except AssertionError:
                out.debug('zombie command terminated')
                out.debug(format_exc())
                return
            except Exception:
                # failed to run command
                out.error('%s: failed to run command "%s"' % (self.hostname, command))
                exitcode = -1

            time_after = timestamp()
            runtime = int(time_after) - int(time_before)
            self.log.append([command, self.connection.stdout, self.connection.stderr, exitcode, runtime])
        elif self.state == 'dryrun':

            out.info('dryrun: %s running "%s"' % (self.hostname, command))
            self.log.append([command, 'dryrun\n', '', 0, 0])
        elif self.state == 'disabled':

            self.log.append(['', '', '', 0, 0])

    def shell(self):
        out.debug('%s: spawning shell' % self.hostname)

        try:
            self.connection.shell()
        except Exception:
            # failed to spawn shell
            out.error('%s: failed to spawn shell')

    def put_file(self, local, remote):
        msg = '{target}: put_file: {local} -> {remote}'
        msg = msg.format({
            'target' : self,
            'local'  : local,
            'remote' : remote,
        })
        out.debug(msg)
        try:
            return self.connection.put(local, remote)
        except Exception as e:
            msg += "failed: {0}".format(str(e))
            out.error(msg)
            raise

    def put(self, local, remote):
        """
        :deprecated: by Target.put_file
        """
        if self.state == 'enabled':
            out.debug('%s: sending "%s"' % (self.hostname, local))
            try:
                return self.connection.put(local, remote)
            except EnvironmentError as error:
                out.error('%s: failed to send %s: %s' % (self.hostname, local, error.strerror))
        elif self.state == 'dryrun':
            out.info('dryrun: put %s %s:%s' % (local, self.hostname, remote))

    def get(self, remote, local):
        if self.state == 'enabled':
            out.debug('%s: receiving "%s"' % (self.hostname, remote))
            try:
                return self.connection.get(remote, local)
            except EnvironmentError as error:
                out.error('%s: failed to get %s: %s' % (self.hostname, remote, error.strerror))
        elif self.state == 'dryrun':
            out.info('dryrun: get %s:%s %s' % (self.hostname, remote, local))

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
        out.debug('%s: getting mtui lock state' % self.hostname)
        lock = Locked(False)

        if self.state != 'enabled':
            return lock

        try:
            lock.locked = self._lock.is_locked()
        except Exception:
            out.error("Reading remote lock failed for {0}".\
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
            out.debug('unable to remove lock from %s. lock is probably not held by this session' % self.hostname)
        except:
            pass

    def add_history(self, comment):
        if self.state == 'enabled':
            out.debug('%s: adding history entry' % self.hostname)
            try:
                filename = os.path.join('/', 'var', 'log', 'mtui.log')
                historyfile = self.connection.open(filename, 'a+')
            except Exception as error:
                out.error('failed to open history file: %s' % error)
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
                out.debug('%s: directory %s does not exist' % (self.hostname, path))
            return []

    def remove(self, path):
        try:
            self.connection.remove(path)
        except IOError as error:
            if error.errno == errno.ENOENT:
                out.debug('%s: path %s does not exist' % (self.hostname, path))
            else:
                try:
                    # might be a directory
                    self.connection.rmdir(path)
                except IOError:
                    out.warning('unable to remove %s on %s' % (path, self.hostname))

    def close(self, action=None):
        def alarm_handler(signum, frame):
            out.warning('timeout reached on %s' % self.hostname)
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
                out.info('rebooting %s' % self.hostname)
                self.run('reboot')
            elif action == 'poweroff':
                out.info('powering off %s' % self.hostname)
                self.run('halt')
            else:
                out.info('closing connection to %s' % self.hostname)

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

    def set_versions(self, before=None, after=None, required=None, current=None, versions=[]):
        if before is not None:
            self.before = before
        if after is not None:
            self.after = after
        if required is not None:
            self.required = required
        if current is not None:
            self.current = current
        if versions:
            self.before = versions[0]
            self.after = versions[1]
            self.required = versions[2]

    def get_versions(self):
        return [self.before, self.after, self.required]

class ThreadedMethod(threading.Thread):

    def __init__(self, queue):
        threading.Thread.__init__(self)
        self.queue = queue

    def run(self):
        while True:
            try:
                (method, parameter) = self.queue.get(timeout=10)
            except:
                return

            out.debug('running method %s(%s)' % (method.__name__, parameter))

            try:
                method(*parameter)
            except:
                raise
            finally:
                try:
                    self.queue.task_done()
                except ValueError:
                    pass  # already removed by ctrl+c


class FileDelete(object):

    def __init__(self, targets, path):
        self.targets = targets
        self.path = path

    def run(self):
        for target in self.targets:
            thread = ThreadedMethod(queue)
            thread.setDaemon(True)
            thread.start()

        for target in self.targets:
            try:
                queue.put([self.targets[target].remove, [self.path]])
            except KeyboardInterrupt:
                pass

        while queue.unfinished_tasks:
            spinner()

        queue.join()


class FileUpload(object):

    def __init__(self, targets, local, remote):
        self.targets = targets
        self.local = local
        self.remote = remote

    def run(self):
        for target in self.targets:
            thread = ThreadedMethod(queue)
            thread.setDaemon(True)
            thread.start()

        for target in self.targets:
            try:
                queue.put([self.targets[target].put, [self.local, self.remote]])
            except KeyboardInterrupt:
                pass

        while queue.unfinished_tasks:
            spinner()

        queue.join()


class FileDownload(object):

    def __init__(self, targets, remote, local, postfix=False):
        self.targets = targets
        self.remote = remote
        self.local = local
        self.postfix = postfix

    def run(self):
        for target in self.targets:
            thread = ThreadedMethod(queue)
            thread.setDaemon(True)
            thread.start()

        for target in self.targets:
            try:
                if self.postfix:
                    queue.put([self.targets[target].get, [self.remote, '%s.%s' % (self.local, target)]])
                else:
                    queue.put([self.targets[target].get, [self.remote, self.local]])
            except KeyboardInterrupt:
                pass

        while queue.unfinished_tasks:
            spinner()

        queue.join()


class RunCommand(object):

    def __init__(self, targets, command):
        self.targets = targets
        self.command = command

    def run(self):
        parallel = {}
        serial = {}
        lock = threading.Lock()

        for target in self.targets:
            if self.targets[target].exclusive:
                serial[target] = self.targets[target]
            else:
                parallel[target] = self.targets[target]

        try:
            for target in parallel:
                thread = ThreadedMethod(queue)
                thread.setDaemon(True)
                thread.start()
                if type(self.command) == dict:
                    queue.put([parallel[target].run, [self.command[target], lock]])
                elif type(self.command) == str:
                    queue.put([parallel[target].run, [self.command, lock]])

            while queue.unfinished_tasks:
                spinner(lock)

            queue.join()

            for target in serial:
                input('press Enter key to proceed with %s' % serial[target].hostname, '')
                thread = ThreadedMethod(queue)
                thread.setDaemon(True)
                thread.start()
                queue.put([serial[target].run, [self.command, lock]])
                while queue.unfinished_tasks:
                    spinner(lock)

                queue.join()
        except KeyboardInterrupt:
            print('stopping command queue, please wait.')
            try:
                while queue.unfinished_tasks:
                    spinner(lock)
            except KeyboardInterrupt:
                for target in self.targets:
                    try:
                        self.targets[target].connection.close_session()
                    except Exception:
                        pass
                try:
                    thread.queue.task_done()
                except ValueError:
                    pass

            queue.join()
            print()
            raise

class Locked(object):

    def __init__(self, locked=False, user='nobody', timestamp=0, pid=0, comment=None):
        self.locked = locked
        self.user = user
        self.timestamp = timestamp
        self.pid = pid
        self.comment = comment

    def own(self):
        u = self._getuser()
        if not self.user == u:
            out.debug("user: %s != %s",self.user, u)
            return False

        p = str(self._getpid())
        if not self.pid == p:
            out.debug("pid: %s != %s", self.pid, p)
            return False

        return True

    def _getuser(self):
        return config.session_user

    def _getpid(self):
        return os.getpid()

    def time(self, style=None):
        from datetime import datetime

        if style is None:
            style = '%A, %d.%m.%Y %H:%M'

        time = datetime.fromtimestamp(float(self.timestamp))

        return time.strftime(style)


def spinner(lock=None):
    """simple spinner to show some process"""

    for pos in ['|', '/', '-', '\\']:
        if lock is not None:
            lock.acquire()

        try:
            sys.stdout.write('processing... [%s]\r' % pos)
            sys.stdout.flush()
        finally:
            if lock is not None:
                lock.release()

        time.sleep(0.3)


