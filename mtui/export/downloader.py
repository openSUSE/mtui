from collections.abc import Callable, Hashable
import concurrent.futures
from logging import getLogger
import os.path
import urllib.error
from urllib.request import urlretrieve

from mtui.messages import ResultsMissingError

logger = getLogger("mtui.export.downloader")


class DownloaderDict(dict):
    def __getitem__(self, item: Hashable) -> Callable[[str, dict, str, str, str], None]:
        try:
            return super().__getitem__(item)
        except KeyError:
            return _emptylog


def _subdl(oqa_path: str, l_path: str, test: dict, errormode: str) -> None:
    try:
        logger.info("Downloading log %s", oqa_path)
        urlretrieve(oqa_path, l_path)
    except urllib.error.HTTPError:
        logger.error("Download from %s failed", oqa_path)
        if errormode == "full":
            raise ResultsMissingError(test["name"], test["arch"])


def _emptylog(host, test, *args, **kwds):
    logger.debug("No log to download for test: %s on %s", test["name"], host)
    pass


def _resultlog(host, test, resultsdir, _, errormode) -> None:
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
    oqa_path = os.path.join(
        host, "tests", str(test["test_id"]), "file", "update_kernel-zypper.log"
    )
    l_path = os.path.join(
        installlogsdir, f"{host.split('/')[-1]}-zypper-{test['arch']}.log"
    )
    logger.debug("Download from %s ", oqa_path)
    logger.debug("Store in %s", l_path)
    _subdl(oqa_path, l_path, test, errormode)


downloader = DownloaderDict({"install": _installlog, "ltp": _resultlog})


def download_logs(oqa, resultsdir, installogsdir, errormode: str) -> None:
    results_matrix: list[tuple[str, str, str, str]] = []
    for host in oqa:
        if host:
            results_matrix += [
                (host.host, x.name, x.test_id, x.arch) for x in host.results
            ]

    with concurrent.futures.ThreadPoolExecutor() as e:
        for host, name, test_id, arch in results_matrix:
            test = {"name": name, "test_id": test_id, "arch": arch}
            dl = downloader[name.split("_")[0]]
            e.submit(dl, host, test, resultsdir, installogsdir, errormode)
