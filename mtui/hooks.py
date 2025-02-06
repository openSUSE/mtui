import subprocess
from abc import ABCMeta, abstractmethod
from collections.abc import Callable
from logging import getLogger
from traceback import format_exc

from mtui import messages

log = getLogger("mtui.script")


class Script(metaclass=ABCMeta):
    """
    :type subdir: Path
    :param subdir: subdirectory in the L{TestReport.scripts_wd} where the
          scripts are located.

      Note: also used as a "type of the script" and can be shown to
      the user.

    FIXME: should be an abstract attribute
    """

    subdir: str = ""

    def __init__(self, tr, path) -> None:
        """
        :type path: str
        :param path: absolute path to the script
        """
        self.path = path
        self.name = path.parent
        self.bname = path.stem
        self.testreport = tr

    def __repr__(self) -> str:
        return "<{0}.{1} {2} for {3}>".format(
            self.__module__, self.__class__.__name__, self.path, repr(self.testreport)
        )

    def __str__(self) -> str:
        return "{0} script {1}".format(self.subdir, self.name)

    def _result(self, cls, bname, t):
        return self.testreport.report_wd(
            *cls.result_parts(bname, t.hostname), filepath=True
        )

    @abstractmethod
    def _run(self, targets) -> None: ...

    @classmethod
    def result_parts(cls, *basename):
        return ("output/scripts", ".".join((cls.subdir,) + basename))

    def run(self, targets) -> None:
        """
        :type targets: [{HostsGroup}]
        """
        try:
            log.info("running {0}".format(self))
            self._run(targets)
        except KeyboardInterrupt:
            log.warning("skipping {0}".format(self))
            return


class PreScript(Script):
    subdir = "pre"

    def _run(self, targets):
        rname = self.testreport.target_wd("{!s}.{!s}".format(self.subdir, self.bname))
        targets.put(self.path, rname)

        targets.put(
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
            fname = self._result(type(self), self.bname, t)
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

    def _run(self, targets):
        for t in targets.values():
            self._run_single_target(t)

    def _run_single_target(self, t):
        bcheck = self.bname.replace("compare_", "check_")
        argv = [
            (self.path),
            self._result(PreScript, bcheck, t),
            self._result(PostScript, bcheck, t),
        ]
        argv = [str(x) for x in argv]

        log.debug("running {0}".format(argv))

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
            logger, msg = log.warning, messages.CompareScriptFailed

        assert isinstance(logger, Callable), "{0!r} not callable".format(logger)

        logger(msg(argv, ret.stdout, ret.stderr, ret.returncode))
