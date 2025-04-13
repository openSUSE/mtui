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
    """
    :type subdir: Path
    :param subdir: subdirectory in the L{TestReport.scripts_wd} where the
          scripts are located.

      Note: also used as a "type of the script" and can be shown to
      the user.

    FIXME: should be an abstract attribute
    """

    subdir: str = ""

    def __init__(self, tr: "TestReport", path: Path) -> None:
        """
        :type path: Path
        :param path: absolute path to the script
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
        return self.testreport.report_wd(
            *cls.result_parts(bname, t.hostname), filepath=True
        )

    @abstractmethod
    def _run(self, targets: "HostsGroup") -> None: ...

    @classmethod
    def result_parts(cls, *basename) -> tuple[Literal["output/scripts"], str]:
        return ("output/scripts", ".".join((cls.subdir,) + basename))

    def run(self, targets: "HostsGroup") -> None:
        """
        :type targets: [{HostsGroup}]
        """
        try:
            log.info("running %s", self)
            self._run(targets)
        except KeyboardInterrupt:
            log.warning("skipping %s", self)


class PreScript(Script):
    subdir = "pre"

    def _run(self, targets: "HostsGroup") -> None:
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
    subdir = "post"


class CompareScript(Script):
    subdir = "compare"

    def _run(self, targets: "HostsGroup") -> None:
        for t in targets.values():
            self._run_single_target(t)

    def _run_single_target(self, t: "Target") -> None:
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
