from logging import getLogger
from os.path import join
import subprocess

from ..messages import SvnCheckoutFailed, SvnCheckoutInterruptedError
from ..types import RequestReviewID
from ..utils import chdir, ensure_dir_exists

logger = getLogger("mtui.template")


class TemplateIOError(IOError):
    """
    New type to distinguish between IOErrors happening when reading the
    template file which are recoverable and IOErrors happening somewhere
    else in the process
    """

    pass


class TestReportAlreadyLoaded(RuntimeError):
    pass


def testreport_svn_checkout(config, path: str, rrid: RequestReviewID) -> None:
    """
    param: path type: str - svn base path - not handled by pathlib
    param: config type: instance of Config singleton
    param: id type: str - RequestReviewID
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
