# -*- coding: utf-8 -*-
# vim: et sw=2 sts=2

from __future__ import print_function

from mtui.target.actions import UpdateError
from mtui.target.actions import ThreadedMethod

from mtui.target.actions import queue


class Install(object):

    def __init__(self, logger, targets, packages=None):
        self.log = logger
        self.targets = targets
        self.packages = packages

    def run(self):
        skipped = False

        try:
            for t in self.targets.values():
                lock = t.locked()
                if lock.locked and not lock.own():
                    skipped = True
                    self.log.warning(
                        'host %s is locked since %s by %s. skipping.' %
                        (t.hostname, lock.time(), lock.user))
                    if lock.comment:
                        self.log.info(
                            "%s's comment: %s" %
                            (lock.user, lock.comment))
                else:
                    t.set_locked()
                    thread = ThreadedMethod(queue)
                    thread.setDaemon(True)
                    thread.start()

            if skipped:
                for t in self.targets.values():
                    try:
                        t.remove_lock()
                    except AssertionError:
                        pass
                raise UpdateError('Hosts locked')

            for command in self.commands:
                self.targets.run(command)

                for t in self.targets.values():
                    self._check(
                        t,
                        t.lastin(),
                        t.lastout(),
                        t.lasterr(),
                        t.lastexit())
        except:
            raise
        finally:
            for t in self.targets.values():
                if not lock.locked:  # wasn't locked earlier by set_host_lock
                    try:
                        t.remove_lock()
                    except AssertionError:
                        pass

    def _check(self, target, stdin, stdout, stderr, exitcode):
        if 'zypper' in stdin and exitcode == 104:
            self.log.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr)
            raise UpdateError(target.hostname, 'package not found')
        if 'A ZYpp transaction is already in progress.' in stderr:
            self.log.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr)
            raise UpdateError(target.hostname, 'update stack locked')
        if 'System management is locked' in stderr:
            self.log.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr)
            raise UpdateError('update stack locked', target.hostname)
        if 'Error:' in stderr:
            self.log.critical(
                '%s: command "%s" failed:\nstdin:\n%s\nstderr:\n%s',
                target.hostname,
                stdin,
                stdout,
                stderr)
            raise UpdateError(target.hostname, 'RPM Error')
        if '(c): c' in stdout:
            self.log.critical(
                '%s: unresolved dependency problem. please resolve manually:\n%s',
                target.hostname,
                stdout)
            raise UpdateError('Dependency Error', target.hostname)

        return self.check(target, stdin, stdout, stderr, exitcode)

    def check(self, target, stdin, stdout, stderr, exitcode):
        """stub. needs to be overwritten by inherited classes"""
        return
