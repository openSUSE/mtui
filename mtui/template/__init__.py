"""Helper classes and functions for working with test report templates."""

import subprocess
from logging import getLogger
from os.path import join
from pathlib import Path

from ..support.fileops import chdir, ensure_dir_exists
from ..support.messages import SvnCheckoutFailed, SvnCheckoutInterruptedError
from ..types import RequestReviewID

logger = getLogger("mtui.template")


class TemplateIOError(IOError):
    """Exception raised for recoverable I/O errors when reading a template."""


class TestReportAlreadyLoadedError(RuntimeError):
    """Exception raised when a test report is already loaded."""


class TemplateFormatError(RuntimeError):
    """Exception raised when a template does not match the expected format."""


def svn_commit_testreport(
    checkout: Path, install_logs: Path, msg: list[str] | None = None
) -> None:
    """Adds the testreport artifacts to SVN and commits the working copy.

    This is the reusable core of the ``commit`` command, shared so other
    commands (e.g. ``approve -r``) can commit the testreport too. Unlike the
    ``commit`` command wrapper, this function lets ``subprocess`` exceptions
    propagate so callers can decide whether to abort.

    Args:
        checkout: The testreport working directory (``report_wd()``).
        install_logs: Path of the install logs to ``svn add``.
        msg: Extra arguments for ``svn ci`` (e.g. ``["-m", "..."]``).

    Raises:
        subprocess.CalledProcessError: If any required ``svn`` call fails.

    """
    msg = msg or []
    subprocess.check_call(
        f"svn add --force {install_logs!s}".split(),
        cwd=checkout,
    )
    if checkout.joinpath("results").exists():
        subprocess.call(
            "svn add --force {}".format("results").split(),
            cwd=checkout,
        )
    if checkout.joinpath("checkers.log").exists():
        subprocess.check_call(
            "svn add --force {}".format("checkers.log").split(),
            cwd=checkout,
        )
    subprocess.check_call(["svn", "up"], cwd=checkout)
    subprocess.check_call(["svn", "ci", *msg], cwd=checkout)


def testreport_svn_checkout(config, path: str, rrid: RequestReviewID) -> None:
    """Checks out a test report template from SVN.

    Args:
        config: The application configuration.
        path: The base path of the SVN repository.
        rrid: The RequestReviewID of the test report.

    """
    ensure_dir_exists(
        config.template_dir,
        on_create=lambda path: logger.debug(
            "created config.template_dir directory %s", path
        ),
    )
    uri = join(path, str(rrid))

    with chdir(config.template_dir):
        try:
            # Capture stderr so svn's cryptic "E170000: URL ... doesn't
            # exist" line does not reach the user; it is surfaced at debug
            # while the caller logs a clear, actionable message instead.
            result = subprocess.run(
                ["svn", "co", uri],
                stderr=subprocess.PIPE,
                text=True,
                check=False,
            )
        except KeyboardInterrupt:
            raise SvnCheckoutInterruptedError(uri) from None

        if result.returncode != 0:
            if result.stderr:
                logger.debug("svn co %s failed: %s", uri, result.stderr.strip())
            report_url = f"{config.fancy_reports_url.rstrip('/')}/{rrid}/log"
            raise SvnCheckoutFailed(str(rrid), report_url) from None
