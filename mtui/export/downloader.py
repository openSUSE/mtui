import concurrent.futures
import os.path
import urllib.error
from logging import getLogger
from urllib.request import urlretrieve

from mtui.messages import ResultsMissingError

logger = getLogger("mtui.export.downloader")


class DownloaderDict(dict):
    def __getitem__(self, item):
        try:
            return super().__getitem__(item)
        except KeyError:
            return emptylog


def subdl(oqa_path, l_path, test, errormode):
    try:
        logger.info(f"Downloading log {oqa_path}")
        urlretrieve(oqa_path, l_path)
    except urllib.error.HTTPError:
        logger.error("Download from {} failed".format(oqa_path))
        if errormode == "full":
            raise ResultsMissingError(test["name"], test["arch"])


def emptylog(host, test, *args, **kwds):
    logger.debug(f"No log to download for test: {test['name']} on {host}")
    pass


def resultlog(host, test, resultsdir, _, errormode):
    oqa_path = os.path.join(
        host, "tests", str(test["test_id"]), "file", "result_array.json"
    )
    l_path = os.path.join(
        resultsdir, f"{host.split('/')[-1]}-{test['arch']}-{test['name']}.json"
    )
    logger.debug("Download from {}".format(oqa_path))
    logger.debug("Store in {}".format(l_path))
    subdl(oqa_path, l_path, test, errormode)


def installlog(host, test, _, installlogsdir, errormode):
    oqa_path = os.path.join(
        host, "tests", str(test["test_id"]), "file", "update_kernel-zypper.log"
    )
    l_path = os.path.join(
        installlogsdir, f"{host.split('/')[-1]}-zypper-{test['arch']}.log"
    )
    logger.debug("Download from {}".format(oqa_path))
    logger.debug("Store in {}".format(l_path))
    subdl(oqa_path, l_path, test, errormode)


downloader = DownloaderDict({"install": installlog, "ltp": resultlog})


def download_logs(oqa, resultsdir, installogsdir, errormode):
    results_matrix = []
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
