
import subprocess
from os.path import join
from qamlib.utils import ensure_dir_exists, chdir
from mtui.messages import SvnCheckoutInterruptedError


class _TemplateIOError(IOError):
    """
    New type to distinguish between IOErrors happening when reading the
    template file which are recoverable and IOErrors happening somewhere
    else in the process
    """
    pass


class TestReportAlreadyLoaded(RuntimeError):
    pass


def testreport_svn_checkout(config, log, path, id):
    ensure_dir_exists(
        config.template_dir,
        on_create=lambda path: log.debug(
            'created config.template_dir directory {0}'.format(path)))

    uri = join(path, id)
    with chdir(config.template_dir):
        try:
            subprocess.check_call(['svn', 'co', uri])
        except KeyboardInterrupt:
            raise SvnCheckoutInterruptedError(uri)
