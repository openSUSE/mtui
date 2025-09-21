"""Helper classes and functions for working with test report templates."""

from logging import getLogger
from os.path import join
import subprocess

from ..messages import SvnCheckoutFailed, SvnCheckoutInterruptedError
from ..types import RequestReviewID
from ..utils import chdir, ensure_dir_exists

logger = getLogger("mtui.template")


class TemplateIOError(IOError):
    """Exception raised for recoverable I/O errors when reading a template."""

    pass


class TestReportAlreadyLoaded(RuntimeError):
    """Exception raised when a test report is already loaded."""

    pass


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
            subprocess.check_call(["svn", "co", uri])
        except KeyboardInterrupt:
            raise SvnCheckoutInterruptedError(uri)
        except subprocess.CalledProcessError:
            f_url = join(config.fancy_reports_url, str(rrid))
            raise SvnCheckoutFailed(uri, f_url)
