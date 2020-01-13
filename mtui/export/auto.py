from http.client import RemoteDisconnected
from itertools import zip_longest
from logging import getLogger
from urllib.error import HTTPError, URLError
from urllib.request import urlopen

from .base import BaseExport

logger = getLogger("mtui.export.auto")


class AutoExport(BaseExport):
    """ Export class for automatic worflow"""

    def get_logs(self, *args, **kwds):
        filepath = self.config.template_dir / str(self.rrid) / self.config.install_logs
        ilogs = zip_longest(
            self.openqa["auto"].results,
            map(self._openqa_installog_to_template, self.openqa["auto"].results),
        )
        filenames = []
        for i, y in ilogs:
            fn = "{}_{}_{}.log".format(i.distri.lower(), i.version, i.arch)
            if y:
                self._writer(filepath.joinpath(fn), y)
                filenames.append(fn)

        return filenames

    @staticmethod
    def _openqa_installog_to_template(url):
        # input is URLs instance
        try:
            with urlopen(url.url) as log:
                t = log.readlines()
            return [x.decode() for x in t]
        except (RemoteDisconnected, HTTPError, URLError) as e:
            logger.error(f"log {url.url} failed to download - {e}")
            return []

    def run(self, *args, **kwds):
        self.install_results()
        self.inject_openqa()
        self.inject_smelt()
        filenames = self.get_logs()
        self.installlogs_lines(filenames)
        self.cut_smelt_data()
        self.add_sysinfo()
        self.dedup_lines()
        return self.template
