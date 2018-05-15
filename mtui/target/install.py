
from logging import getLogger

from mtui.target.actions import UpdateError
from mtui.target.actions import ThreadedMethod

from mtui.target.actions import queue

logger = getLogger('mtui.target.install')


class Install(object):

    def __init__(self, targets, packages=None):
        self.targets = targets
        self.packages = packages

    def run(self):
        skipped = False

        try:
            for t in list(self.targets.values()):
                lock = t.locked()
                if lock.locked and not lock.own():
                    skipped = True
                    logger.warning(
                        'host {!s} is locked since {!s} by {!s}. skipping.'.format(
                            t.hostname, lock.time(), lock.user))
                    if lock.comment:
                        logger.info(
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

            for command in self.commands:
                self.targets.run(command)

                for t in list(self.targets.values()):
                    self._check(
                        t,
                        t.lastin(),
                        t.lastout(),
                        t.lasterr(),
                        t.lastexit())
        except BaseException:
            raise
        finally:
            for t in list(self.targets.values()):
                if not lock.locked:  # wasn't locked earlier by set_host_lock
                    try:
                        t.remove_lock()
                    except AssertionError:
                        pass

    def _check(self, target, stdin, stdout, stderr, exitcode):
        if 'zypper' in stdin and exitcode == 104:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr))
            raise UpdateError('package not found', target.hostname)
        if 'A ZYpp transaction is already in progress.' in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr))
            raise UpdateError('update stack locked', target.hostname)
        if 'System management is locked' in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr))
            raise UpdateError('update stack locked', target.hostname)
        if 'Error:' in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr))
            raise UpdateError('RPM Error', target.hostname)
        if '(c): c' in stdout:
            logger.critical(
                '{!s}: unresolved dependency problem. please resolve manually:\n{!s}'.format(
                    target.hostname, stdout))
            raise UpdateError('Dependency Error', target.hostname)

        return self.check(target, stdin, stdout, stderr, exitcode)

    def check(self, target, stdin, stdout, stderr, exitcode):
        """stub. needs to be overwritten by inherited classes"""
        return
