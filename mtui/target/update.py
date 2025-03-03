from logging import getLogger

from ..hooks import CompareScript, PostScript, PreScript
from ..types.rpmver import RPMVersion
from .actions import queue, spinner
from .basedoer import Doer
from .hostgroup import HostsGroup
from .locks import LockedTargets

logger = getLogger("mtui.target.update")


class Update(Doer):
    def __init__(self, targets: HostsGroup, testreport) -> None:
        super().__init__(targets, testreport)
        self.commands: list[str] = []

    def run(self, params) -> None:
        with LockedTargets(self.targets.values()):
            if hasattr(self, "type") and self.type == "transactional":
                self._run_transactional(params)
            else:
                self._run_standard(params)

    def _run_transactional(self, params):
        if any(param for param in params):
            logger.warning(
                "The options --noprepare, --newpackage and --noscript are not valid for transactional updates"
            )

        self.lock_hosts()
        self._run()
        self.unlock_hosts()
        logger.warning(
            "Please reboot the host to activate the changes and avoid data loss"
        )

    def _run_standard(self, params) -> None:
        if "noprepare" not in params:
            self.testreport.perform_prepare(self.targets)

        for hn, t in self.targets.items():
            not_installed = []

            t.query_versions()

            for pkg in t.packages.keys():
                required = t.packages[pkg].required
                before = t.packages[pkg].current

                t.packages[pkg].before = before

                if not before:
                    not_installed.append(pkg)
                else:
                    if RPMVersion(before) >= RPMVersion(required):
                        logger.warning(
                            "%s: package is too recent: %s (%s, target version is %s)",
                            hn,
                            pkg,
                            before,
                            required,
                        )

            if not_installed:
                logger.warning("%s: these packages are missing: %s", hn, not_installed)

        if "noscript" not in params and not self.testreport.config.auto:
            self.testreport.run_scripts(PreScript, self.targets)

        self.lock_hosts()
        self._run()
        self.unlock_hosts()

        if "newpackage" in params:
            # TODO: testing=True for newpackage ? oh
            self.testreport.perform_prepare(self.targets, testing=True)

        for hn, t in list(self.targets.items()):
            t.query_versions()

            for pkg in t.packages.keys():
                before = t.packages[pkg].before
                required = t.packages[pkg].required
                after = t.packages[pkg].current

                t.packages[pkg].after = after

                if after and before:
                    if RPMVersion(before) == RPMVersion(after):
                        logger.warning(
                            "%s: package was not updated: %s (%s)", hn, pkg, after
                        )
                if after:
                    if RPMVersion(after) < RPMVersion(required):
                        logger.warning(
                            "%s: package does not match required version: %s (%s, required %s)",
                            hn,
                            pkg,
                            after,
                            required,
                        )

        if "noscript" not in params and not self.testreport.config.auto:
            self.testreport.run_scripts(PostScript, self.targets)
            self.testreport.run_scripts(CompareScript, self.targets)

    def _run(self) -> None:
        for t in self.targets.values():
            if (hasattr(self, "type") and self.type != "transactional") or not hasattr(
                self, "type"
            ):
                queue.put((t.set_repo, ["add", self.testreport]))

        while queue.unfinished_tasks:
            spinner()

        queue.join()

        for command in self.commands:
            self.targets.run(command)

            for t in self.targets.values():
                self._check(t, t.lastin(), t.lastout(), t.lasterr(), t.lastexit())
