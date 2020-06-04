from logging import getLogger

from mtui.target.actions import ThreadedMethod, UpdateError, queue

logger = getLogger("mtui.target.install")


class Install:
    def __init__(self, targets, packages=None):
        self.targets = targets
        self.packages = packages

    def run(self):
        skipped = False

        try:
            for t in self.targets.values():
                if t.is_locked() and not t._lock.is_mine():
                    skipped = True
                    logger.warning(
                        "host {!s} is locked since {!s} by {!s}. skipping.".format(
                            t.hostname, t._lock.time(), t._lock.locked_by()
                        )
                    )
                    if t._lock.comment():
                        logger.info(
                            "{!s}'s comment: {!s}".format(
                                t._lock.locked_by(), t._lock.comment()
                            )
                        )
                else:
                    t.lock()
                    thread = ThreadedMethod(queue)
                    thread.setDaemon(True)
                    thread.start()

            if skipped:
                for t in self.targets.values():
                    try:
                        t.unlock()
                    except AssertionError:
                        pass
                raise UpdateError("Hosts locked")

            for command in self.commands:
                self.targets.run(command)

                for t in self.targets.values():
                    self._check(t, t.lastin(), t.lastout(), t.lasterr(), t.lastexit())
        except BaseException:
            raise
        finally:
            for t in self.targets.values():
                if t.is_locked():
                    try:
                        t.unlock()
                    except AssertionError:
                        pass

    def _check(self, target, stdin, stdout, stderr, exitcode):
        if exitcode in [0, 100, 101, 102, 103, 106]:
            return self.check(target, stdin, stdout, stderr, exitcode)
        elif "zypper" in stdin and exitcode == 104:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("package not found", target.hostname)
        elif "A ZYpp transaction is already in progress." in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("update stack locked", target.hostname)
        elif "System management is locked" in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("update stack locked", target.hostname)
        elif "Error:" in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("RPM Error", target.hostname)
        elif "(c): c" in stdout:
            logger.critical(
                "{!s}: unresolved dependency problem. please resolve manually:\n{!s}".format(
                    target.hostname, stdout
                )
            )
            raise UpdateError("Dependency Error", target.hostname)
        else:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("Unknown Error", target.hostname)

    def check(self, target, stdin, stdout, stderr, exitcode):
        """stub. needs to be overwritten by inherited classes"""
        return
