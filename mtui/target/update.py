from logging import getLogger

from mtui.target.actions import UpdateError
from mtui.target.actions import ThreadedMethod

from mtui.target.actions import queue
from mtui.target.actions import spinner

from mtui.target.locks import LockedTargets

from mtui.hooks import PreScript
from mtui.hooks import PostScript
from mtui.hooks import CompareScript

from qamlib.types.rpmver import RPMVersion
from mtui.utils import yellow

logger = getLogger("mtui.target.update")


class Update(object):
    def __init__(self, targets, packages, testreport):
        self.targets = targets
        self.packages = packages
        self.testreport = testreport
        self.commands = []

    def run(self, params):
        with LockedTargets(list(self.targets.values())):
            if hasattr(self, "type") and self.type == "transactional":
                self._run_transactional(params)
            else:
                self._run(params)

    def _run_transactional(self, params):
        if any(param for param in params):
            logger.warning(
                "The options --noprepare, --newpackage and --noscript are not valid for transactional updates"
            )

        self.lock_and_run()
        logger.warning(
            "Please reboot the host to activate the changes and avoid data loss"
        )

    def _run(self, params):
        if "noprepare" not in params:
            self.testreport.perform_prepare(self.targets)

        for hn, t in list(self.targets.items()):
            not_installed = []

            t.query_versions()

            for pkgname, pkg in list(t.packages.items()):
                required = self.testreport.packages[pkgname]
                before = pkg.current

                pkg.set_versions(before=before, required=required)

                if not before:
                    not_installed.append(pkgname)
                else:
                    if RPMVersion(before) >= RPMVersion(required):
                        logger.warning(
                            "{!s}: package is too recent: {!s} ({!s}, target version is {!s})".format(
                                hn, pkgname, before, required
                            )
                        )

            if not_installed:
                logger.warning(
                    "{!s}: these packages are missing: {!s}".format(hn, not_installed)
                )

        if "noscript" not in params and not self.testreport.config.auto:
            self.testreport.run_scripts(PreScript, self.targets)

        self.lock_and_run()
        if "newpackage" in params:
            # TODO: testing=True for newpackage ? oh
            self.testreport.perform_prepare(self.targets, testing=True)

        for hn, t in list(self.targets.items()):
            t.query_versions()

            for pkgname, pkg in list(t.packages.items()):
                before = pkg.before
                required = pkg.required
                after = pkg.current

                pkg.set_versions(after=after)

                if after and before:
                    if RPMVersion(before) == RPMVersion(after):
                        logger.warning(
                            "{!s}: package was not updated: {!s} ({!s})".format(
                                hn, pkgname, after
                            )
                        )
                if after:
                    if RPMVersion(after) < RPMVersion(required):
                        logger.warning(
                            "{!s}: package does not match required version: {!s} ({!s}, required {!s})".format(
                                hn, pkgname, after, required
                            )
                        )

        if "noscript" not in params and not self.testreport.config.auto:
            self.testreport.run_scripts(PostScript, self.targets)
            self.testreport.run_scripts(CompareScript, self.targets)

    def _check(self, target, stdin, stdout, stderr, exitcode):
        if "zypper" in stdin and exitcode == 104:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("update stack locked", target.hostname)
        if "zypper" in stdin and exitcode == 106:
            logger.warning(
                "{!s}: zypper returns exitcode 106:\n{!s}".format(
                    target.hostname, stderr
                )
            )
        if "Additional rpm output" in stdout:
            logger.warning(
                "There was additional rpm output on {!s}:".format(target.hostname)
            )
            marker = "Additional rpm output:"
            start = stdout.find(marker) + len(marker)
            end = stdout.find("Retrieving", start)
            print(stdout[start:end].replace("warning", yellow("warning")))
        if "A ZYpp transaction is already in progress." in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("update stack locked", target.hostname)
        if "System management is locked" in stderr:
            logger.critical(
                '{!s}: command "{!s}" failed:\nstdin:\n{!s}\nstderr:\n{!s}'.format(
                    target.hostname, stdin, stdout, stderr
                )
            )
            raise UpdateError("update stack locked", target.hostname)
        if "(c): c" in stdout:
            logger.critical(
                "{!s}: unresolved dependency problem. please resolve manually:\n{!s}".format(
                    target.hostname, stdout
                )
            )
            raise UpdateError("Dependency Error", target.hostname)

        return self.check(target, stdin, stdout, stderr, exitcode)

    def check(self, target, stdin, stdout, stderr, exitcode):
        """stub. needs to be overwritten by inherited classes"""
        return

    def lock_and_run(self):
        """
        Locks the targets and run the commands
        """
        skipped = False

        try:
            for t in list(self.targets.values()):
                lock = t.locked()
                if lock.locked and not lock.own():
                    skipped = True
                    logger.warning(
                        "host {!s} is locked since {!s} by {!s}. skipping.".format(
                            t.hostname, lock.time(), lock.user
                        )
                    )
                    if lock.comment:
                        logger.info(
                            "{!s}'s comment: {!s}".format(lock.user, lock.comment)
                        )
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
                raise UpdateError("Hosts locked")

            for t in list(self.targets.values()):
                if (
                    hasattr(self, "type") and self.type != "transactional"
                ) or not hasattr(self, "type"):
                    queue.put([t.set_repo, ["add", self.testreport]])

            while queue.unfinished_tasks:
                spinner()

            queue.join()

            for command in self.commands:
                self.targets.run(command)

                for t in list(self.targets.values()):
                    self._check(t, t.lastin(), t.lastout(), t.lasterr(), t.lastexit())
        except BaseException:
            raise
        finally:
            for t in list(self.targets.values()):
                lock = t.locked()
                if not lock.locked:  # wasn't locked earlier by set_host_lock
                    try:
                        t.remove_lock()
                    except AssertionError:
                        pass
