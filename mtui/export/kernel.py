from datetime import datetime
from logging import getLogger
from pathlib import Path

from mtui.types import FileList
from mtui.utils import ensure_dir_exists

from .base import BaseExport
from .downloader import download_logs

logger = getLogger("mtui.export.kernel")


class KernelExport(BaseExport):
    """Exporter for kernel jobs"""

    def get_logs(self, *args, **kwds) -> list[Path]:
        in_path = self.config.template_dir / str(self.rrid) / self.config.install_logs
        res_path = self.config.template_dir / str(self.rrid) / "results"
        ensure_dir_exists(res_path)
        oqa = (result for result in self.openqa["kernel"])
        # TODO: configurable errormode
        download_logs(oqa, res_path, in_path, "tolerant")

        return [fn.name for fn in in_path.glob("*.log")]

    def kernel_results(self) -> None:
        line = self.template.index("regression tests:\n")
        try:
            line = self.template.index("(put your details here)\n", line)
            del self.template[line]
        except ValueError:
            try:
                line = (
                    self.template.index(
                        "    * https://pes.suse.de/QA_Maintenance/kernel-default/\n"
                    )
                    + 1
                )
            except ValueError:
                line = line = self.template.index("regression tests:\n") + 1

            e_line = self.template.index("build log review:\n")
            del self.template[line:e_line]

        self.template.insert(line, f"Results added on {datetime.now()}\n")
        self.template.insert(line + 1, "\n")
        self.template.insert(line + 2, "Results from openQA:\n")
        self.template.insert(line + 3, "\n")
        line += 4

        for results in self.openqa["kernel"]:
            if results:
                for r in results.pp:
                    self.template.insert(line, r)
                    line += 1
                line += 1

        line = self.template.index("build log review:\n")
        self.template.insert(line, "\n")

    def run(self, *args, **kwds) -> FileList | list[str]:
        self.install_results()
        self.inject_openqa()
        self.kernel_results()
        filenames = self.get_logs()
        self.installlogs_lines(filenames)
        self.add_sysinfo()
        self.dedup_lines()
        return self.template
