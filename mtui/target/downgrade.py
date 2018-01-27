# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2


import re

from qamlib.types.rpmver import RPMVersion
from mtui.target.actions import UpdateError
from mtui.target.actions import ThreadedMethod

from mtui.target.actions import queue
from mtui.target.actions import spinner


class Downgrade(object):

    def __init__(self, logger, targets, packages, testreport):
        self.log = logger
        self.targets = targets
        self.packages = packages
        self.testreport = testreport

        self.commands = {}
        self.install_command = None
        self.list_command = None
        self.pre_commands = []
        self.post_commands = []

    def run(self):
        if hasattr(self, 'type') and self.type == 'transactional':
            self._run_transactional()
        else:
            self._run()

    def _run_transactional(self):
        self.lock_hosts()
        try:
            for command in self.commands:
                self.targets.run(command)

            for t in list(self.targets.values()):
                if 'Error' in t.lasterr():
                    self.log.critical('{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(t.hostname, t.lastin(), t.lastout(), t.lasterr()))
                if 'reboot to finish rollback' in t.lastout():
                    self.log.warning('Please reboot the host {!s} to finish rollback'.format(t.hostname))
        except:
            raise
        finally:
            self.unlock_hosts()


    def _run(self, type=None):
        versions = {}
        lock = self.lock_hosts()
        try:
            for t in list(self.targets.values()):
                queue.put([t.set_repo, ['remove', self.testreport]])

            while queue.unfinished_tasks:
                spinner()

            queue.join()

            for t in list(self.targets.values()):
                if t.lasterr():
                    self.log.critical(
                        'failed to downgrade host {!s}. stopping.\n# {!s}\n{!s}'.format(
                            t.hostname, t.lastin(), t.lasterr()))
                    return

            self.targets.run(self.list_command)

            for hn, t in list(self.targets.items()):
                lines = t.lastout().split('\n')
                release = {}
                for line in lines:
                    match = re.search('(.*) = (.*)', line)
                    if match:
                        name = match.group(1)
                        version = match.group(2)
                        release.setdefault(name, []).append(version)

                for name in release:
                    version = sorted(
                        release[name],
                        key=RPMVersion,
                        reverse=True)[0]
                    versions.setdefault(hn, dict()).update({name: version})

            for command in self.pre_commands:
                self.targets.run(command)

            for package in self.packages:
                temp = self.targets.copy()
                for hn in self.targets:
                    try:
                        command = self.install_command.format(
                            package, package, versions[hn][package])
                        self.commands.update({hn: command})
                    except KeyError:
                        del temp[hn]
                temp.run(self.commands)

                for t in list(self.targets.values()):
                    self._check(
                        t,
                        t.lastin(),
                        t.lastout(),
                        t.lasterr(),
                        t.lastexit())

            for command in self.post_commands:
                self.targets.run(command)

        except:
            raise
        finally:
            self.unlock_hosts()

    # TODO: check if this work correctly -> maybe use re
    def _check(self, target, stdin, stdout, stderr, exitcode):
        if 'A ZYpp transaction is already in progress.' in stderr:
            self.log.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr))
            raise UpdateError(target.hostname, 'update stack locked')
        if 'System management is locked' in stderr:
            self.log.critical(
                '{s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr))
            raise UpdateError('update stack locked', target.hostname)
        if '(c): c' in stdout:
            self.log.critical(
                '{!s}: unresolved dependency problem. please resolve manually:\n{!s}'.format(
                    target.hostname, stdout))
            raise UpdateError('Dependency Error', target.hostname)
        if exitcode == 104:
            self.log.critical(
                '{!s}: zypper returned with errorcode 104:\n{!s}'.format(
                    target.hostname, stderr))
            raise UpdateError('Unspecified Error', target.hostname)
        if exitcode == 106:
            self.log.warning(
                "{!s}: zypper returned with errocode 106:\n{!s}".format(
                    target.hostname, stderr))

        return self.check(target, stdin, stdout, stderr, exitcode)

    def check(self, target, stdin, stdout, stderr, exitcode):
        """stub. needs to be overwritten by inherited classes"""
        return

    def lock_hosts(self):
        try:
            skipped = False
            for t in list(self.targets.values()):
                lock = t.locked()
                if lock.locked and not lock.own():
                    skipped = True
                    self.log.warning(
                        'host {!s} is locked since {!s} by {!s}. skipping.'.format(
                            t.hostname, lock.time(), lock.user))
                    if lock.comment:
                        self.log.info(
                            "{!s}'s comment: {!s}".format(
                                lock.user, lock.comment))
                else:
                    t.set_locked()
                    thread = ThreadedMethod(queue)
                    thread.setDaemon(True)
                    thread.start()

            if skipped:
                for t in list(self.targets.values()):
                    try:
                        t.remove_lock()
                    except AssertionError:
                        pass
                raise UpdateError('Hosts locked')
        except:
            raise
        finally:
            self.unlock_hosts()


    def unlock_hosts(self):
        for t in list(self.targets.values()):
            lock = t.locked()
            if lock.locked:  # wasn't locked earlier by set_host_lock
                try:
                    t.remove_lock()
                except AssertionError:
                    pass
