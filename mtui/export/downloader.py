"""Functions for downloading logs from openQA."""

from collections.abc import Callable
import concurrent.futures
from logging import getLogger
import os.path
import urllib.error
from urllib.request import urlretrieve

from mtui.messages import ResultsMissingError

logger = getLogger("mtui.export.downloader")


def _subdl(oqa_path: str, l_path: str, test: dict, errormode: str) -> None:
    """A helper function for downloading a single log file.

    Args:
        oqa_path: The path to the log file on the openQA server.
        l_path: The local path to save the log file to.
        test: A dictionary containing information about the test.
        errormode: The error mode to use if the download fails.
    """
    try:
        logger.info("Downloading log %s", oqa_path)
        urlretrieve(oqa_path, l_path)
    except urllib.error.HTTPError:
        logger.error("Download from %s failed", oqa_path)
        if errormode == "full":
            raise ResultsMissingError(test["name"], test["arch"])


def _emptylog(host, test, *args, **kwds) -> None:
    """A downloader function for tests that have no log to download.

    Args:
        host: The host of the openQA instance.
        test: A dictionary containing information about the test.
        *args: Additional arguments (not used).
        **kwds: Additional keyword arguments (not used).
    """
    logger.debug("No log to download for test: %s on %s", test["name"], host)
    pass


def _resultlog(host, test, resultsdir, _, errormode) -> None:
    """A downloader function for result logs.

    Args:
        host: The host of the openQA instance.
        test: A dictionary containing information about the test.
        resultsdir: The directory to save the results to.
        _: An unused argument.
        errormode: The error mode to use if the download fails.
    """
    oqa_path = os.path.join(
        host, "tests", str(test["test_id"]), "file", "result_array.json"
    )
    l_path = os.path.join(
        resultsdir, f"{host.split('/')[-1]}-{test['arch']}-{test['name']}.json"
    )
    logger.debug("Download from %s ", oqa_path)
    logger.debug("Store in %s", l_path)
    _subdl(oqa_path, l_path, test, errormode)


def _installlog(host, test, _, installlogsdir, errormode) -> None:
    """A downloader function for install logs.

    Args:
        host: The host of the openQA instance.
        test: A dictionary containing information about the test.
        _: An unused argument.
        installlogsdir: The directory to save the install logs to.
        errormode: The error mode to use if the download fails.
    """
    oqa_path = os.path.join(
        host, "tests", str(test["test_id"]), "file", "update_kernel-zypper.log"
    )
    l_path = os.path.join(
        installlogsdir, f"{host.split('/')[-1]}-zypper-{test['arch']}.log"
    )
    logger.debug("Download from %s ", oqa_path)
    logger.debug("Store in %s", l_path)
    _subdl(oqa_path, l_path, test, errormode)


#: A dictionary that maps log types to downloader functions.
downloader: dict[str, Callable[[str, dict, str, str, str], None]] = {
    "install": _installlog,
    "ltp": _resultlog,
}


def download_logs(oqa, resultsdir, installogsdir, errormode: str) -> None:
    """Downloads logs from openQA.

    Args:
        oqa: A list of openQA connector instances.
        resultsdir: The directory to save the results to.
        installogsdir: The directory to save the install logs to.
        errormode: The error mode to use if a download fails.
    """
    results_matrix: list[tuple[str, str, str, str]] = []
    for host in oqa:
        if host:
            results_matrix += [
                (host.host, x.name, x.test_id, x.arch) for x in host.results
            ]

    with concurrent.futures.ThreadPoolExecutor() as e:
        for host, name, test_id, arch in results_matrix:
            test = {"name": name, "test_id": test_id, "arch": arch}
            dl = downloader.get(name.split("_")[0], _emptylog)
            e.submit(dl, host, test, resultsdir, installogsdir, errormode)
