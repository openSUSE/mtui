
from logging import getLogger

from mtui.target.actions import UpdateError
from mtui.target.actions import ThreadedMethod

from mtui.target.actions import queue
from mtui.target.actions import spinner

logger = getLogger('mtui.target.prepare')


class Prepare(object):

    def __init__(
            self,
            targets,
            packages,
            testreport,
            testing=False,
            force=False,
            installed_only=False):
        self.targets = targets
        self.packages = packages
        self.testreport = testreport
        self.testing = testing
        self.force = force
        self.installed_only = installed_only
        self.commands = []

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

            for t in list(self.targets.values()):
                if self.testing:
                    queue.put([t.set_repo, ['add', self.testreport]])
                else:
                    queue.put([t.set_repo, ['remove', self.testreport]])

            while queue.unfinished_tasks:
                spinner()

            queue.join()

            for t in list(self.targets.values()):
                if t.lasterr():
                    logger.critical(
                        'failed to prepare host {!s}. stopping.\n# {!s}\n{!s}'.format(
                            t.hostname, t.lastin(), t.lasterr()))
                    return

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
        if 'A ZYpp transaction is already in progress.' in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr))
            raise UpdateError(target.hostname, 'update stack locked')
        if 'System management is locked' in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr))
            raise UpdateError('update stack locked', target.hostname)
        if '(c): c' in stdout:
            logger.critical(
                '{!s}: unresolved dependency problem. please resolve manually:\n{!s}'.format(
                    target.hostname, stdout))
            raise UpdateError('Dependency Error', target.hostname)

        return self.check(target, stdin, stdout, stderr, exitcode)

    def check(self, target, stdin, stdout, stderr, exitcode):
        """stub. needs to be overwritten by inherited classes"""
        return
