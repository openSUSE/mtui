"""A system for running pre- and post-execution hook scripts.

This module defines an abstract base class `Script` to define the
interface for these scripts, with concrete implementations for
`PreScript`, `PostScript`, and `CompareScript`.
"""

from abc import ABC, abstractmethod
from logging import getLogger
from pathlib import Path
import subprocess
from traceback import format_exc
from typing import Literal, TYPE_CHECKING

from . import messages

if TYPE_CHECKING:
    from .target import Target
    from .target.hostgroup import HostsGroup
    from .template.testreport import TestReport


log = getLogger("mtui.script")


class Script(ABC):
    """An abstract base class for hook scripts."""

    subdir: str = ""

    def __init__(self, tr: "TestReport", path: Path) -> None:
        """Initializes the script object.

        Args:
            tr: The test report object.
            path: The absolute path to the script.
        """
        self.path = path
        self.name = path.parent
        self.bname = path.stem
        self.testreport = tr

    def __repr__(self) -> str:
        return f"<{self.__module__}.{self.__class__.__name__} {self.path} for {repr(self.testreport)}>"

    def __str__(self) -> str:
        return f"{self.subdir} script {self.name}"

    def _result(self, cls, bname: str, t: "Target") -> Path:
        """Constructs the path to the result file for a script.

        Args:
            cls: The class of the script.
            bname: The base name of the script.
            t: The target host.

        Returns:
            The path to the result file.
        """
        return self.testreport.report_wd(
            *cls.result_parts(bname, t.hostname), filepath=True
        )

    @abstractmethod
    def _run(self, targets: "HostsGroup") -> None:
        """An abstract method for running the script.

        Args:
            targets: The group of target hosts.
        """
        ...

    @classmethod
    def result_parts(cls, *basename) -> tuple[Literal["output/scripts"], str]:
        """Returns the parts of the result path.

        Args:
            *basename: The base name of the result file.

        Returns:
            A tuple containing the parts of the result path.
        """
        return ("output/scripts", ".".join((cls.subdir,) + basename))

    def run(self, targets: "HostsGroup") -> None:
        """Runs the script.

        Args:
            targets: The group of target hosts.
        """
        try:
            log.info("running %s", self)
            self._run(targets)
        except KeyboardInterrupt:
            log.warning("skipping %s", self)


class PreScript(Script):
    """A hook script that runs before the main execution."""

    subdir = "pre"

    def _run(self, targets: "HostsGroup") -> None:
        """Runs the pre-execution script.

        Args:
            targets: The group of target hosts.
        """
        rname: Path = self.testreport.target_wd(
            "{!s}.{!s}".format(self.subdir, self.bname)
        )
        targets.sftp_put(self.path, rname)

        targets.sftp_put(
            self.testreport.report_wd("packages-list.txt", filepath=True),
            self.testreport.target_wd("package-list.txt"),
        )

        targets.run(
            "{exe} -r {repository} -p {pkg_list_file} {kind}".format(
                exe=rname,
                repository=self.testreport.repository,
                pkg_list_file=self.testreport.target_wd("package-list.txt"),
                kind=self.testreport.id,
            )
        )

        for t in targets.values():
            fname: Path = self._result(type(self), self.bname, t)
            try:
                with fname.open(mode="w") as f:
                    f.write(t.lastout())
                    f.write(t.lasterr())
            except IOError as e:
                log.error(messages.FailedToWriteScriptResult(fname, e))


class PostScript(PreScript):
    """A hook script that runs after the main execution."""

    subdir = "post"


class CompareScript(Script):
    """A script that compares the results of pre- and post-execution scripts."""

    subdir = "compare"

    def _run(self, targets: "HostsGroup") -> None:
        """Runs the compare script.

        Args:
            targets: The group of target hosts.
        """
        for t in targets.values():
            self._run_single_target(t)

    def _run_single_target(self, t: "Target") -> None:
        """Runs the compare script for a single target.

        Args:
            t: The target host.
        """
        bcheck = self.bname.replace("compare_", "check_")
        argv = [
            str(x)
            for x in (
                self.path,
                self._result(PreScript, bcheck, t),
                self._result(PostScript, bcheck, t),
            )
        ]

        log.debug("running %s", argv)
        try:
            ret = subprocess.run(
                argv,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                universal_newlines=True,
            )
        except EnvironmentError as e:
            t.out.append([" ".join(argv), "", "", 0x100, 0])
            log.critical(messages.StartingCompareScriptError(e, argv))
            log.debug(format_exc())
            return
        t.out.append([" ".join(argv), ret.stdout, ret.stderr, ret.returncode, 0])

        if ret.returncode == 0:
            return

        if ret.returncode == 2:
            logger, msg = log.critical, messages.CompareScriptCrashed
        else:
            logger, msg = log.warning, messages.CompareScriptFailed  # type: ignore

        logger(msg(argv, ret.stdout, ret.stderr, ret.returncode))
