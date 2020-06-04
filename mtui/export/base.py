from abc import ABC, abstractmethod
from logging import getLogger

from mtui.systemcheck import system_info
from mtui.utils import prompt_user, timestamp

logger = getLogger("mtui.export.base")


class BaseExport(ABC):
    """Base Export class, it modify 'template' in place (ugly sideeffect)
    and downloads/exports all helper logs"""

    __slots__ = [
        "config",
        "xmllog",
        "openqa",
        "smelt",
        "template",
        "force",
        "rrid",
        "interactive",
    ]

    def __init__(
        self, config, openqa, smelt, template, force, rrid, interactive, **kwargs ):
        """ param: config = Config()
            param: xmllog = xml.minidom
            param: openqa = testreport.openqa
            param: smelt = testreport.smelt
            param: template = FileList()
            param: force = Bool()
            param: rrid = testreport.id
            param: interactive
        """

        self.config = config
        self.openqa = openqa
        self.smelt = smelt
        self.template = template[:]
        self.force = force
        self.rrid = rrid
        self.interactive = interactive
        for key in kwargs:
            self.__setattr__(key, kwargs[key])

    def _writer(self, fn, data):
        if fn.exists() and not self.force:
            logger.warning(f"file {fn} exists.")
            if not prompt_user(
                f"Should I overwrite {fn} (y/N) ",
                ["y", "Y", "yes", "Yes", "YES"],
                self.interactive,
            ):
                fn = fn.with_suffix("." + timestamp())

        logger.info(f"exporting log to {fn}")

        try:
            with fn.open(mode="w", encoding="utf-8") as f:
                f.write("\n".join(line.rstrip() for line in data))
        except IOError as e:
            logger.error("Failed to write {}: {}".format(fn, e.strerror))
            return

    def installlogs_lines(self, filenames):
        o = 0
        for l in self.template:
            if "HAS_UNTRACKED" in l:
                break
        o += 1

        index = len(self.template)
        if "## export MTUI:" in self.template[-1]:
            index -= 1
        self.template.insert(index, "\n")
        self.template.insert(index + 1, "Links for update logs:\n")
        self.template.insert(index + 2, "\n")
        index += 2

        add_empty_line = False
        for fn in filenames:
            install_log = "{!s}/{!s}/{!s}/{!s}\n".format(
                self.config.reports_url, self.rrid, self.config.install_logs, fn
            )
            if install_log not in self.template[o:]:
                index += 1
                self.template.insert(index, install_log)
                add_empty_line = True

        if add_empty_line:
            self.template.insert(index + 1, "\n")

    def dedup_lines(self):
        """simple deduplication, start it as last"""
        pr_line = None
        lines = []
        for c_line in self.template:
            if pr_line == c_line and c_line != "\n":
                pass
            else:
                lines.append(c_line)
            pr_line = c_line

        self.template = lines

    def add_sysinfo(self):
        system_information = system_info(
            self.config.distro,
            self.config.distro_ver,
            self.config.distro_kernel,
            self.config.session_user,
        )
        if system_information != self.template[-1].rstrip():
            self.template.append(system_information)

    @abstractmethod
    def get_logs(self, *args, **kwds):
        pass

    @abstractmethod
    def run(self, *args, **kwds):
        pass

    def cut_smelt_data(self):
        """ trip melt chechers to defined lenght and rest sends to own list,
        must be called after **inject_smelt** """

        try:
            start = self.template.index("SMELT Checkers:\n")
        except ValueError:
            logger.debug("No SMELT data in template")
            return

        end = self.template.index("REGRESSION TEST SUMMARY:\n", start)

        if end - start < self.config.threshold:
            return
        else:
            smelt = self.template[start:end]
            del self.template[start + self.config.threshold : end]
            self.template.insert(start + self.config.threshold, "\n")
            self.template.insert(
                start + self.config.threshold,
                "Rest of SMELT checkers results were moved to checkers.log file, please check it\n",
            )
            self.template.insert(start + self.config.threshold, "\n")
            logger.info("Checkers results were stripped and moved to checkers.log file")

        # Write checkers.log
        fn = self.config.template_dir / str(self.rrid) / "checkers.log"
        self._writer(fn, smelt)
        logger.info(f"Wrote checkers results to {fn}")

    def inject_smelt(self):
        if not self.smelt:
            return
        smelt_output = self.smelt.pretty_output()
        if smelt_output:
            smelt_output = ["SMELT Checkers:\n", "===============\n"] + smelt_output
        else:
            return

        i = self.template.index("REGRESSION TEST SUMMARY:\n", 0)
        if "SMELT Checkers:\n" not in self.template:
            self.template.insert(i, "\n")
            for line in reversed(smelt_output):
                self.template.insert(i, line)

    def inject_openqa(self):
        if not self.openqa["auto"]:
            return

        openqa = self.openqa["auto"].pp
        if not openqa:
            return

        # remove previous results
        # TODO: simplify in future +- 5m after release of MTUI12

        if "Results from incidents openQA jobs:\n" in self.template:
            r_start = self.template.index("Results from incidents openQA jobs:\n")
            try:
                r_end = self.template.index("End of openQA Incidents results\n") + 1
            except ValueError:
                r_end = self.template.index("source code change review:\n", 0) - 1

            del self.template[r_start:r_end]
        # new title
        elif "Results from openQA incidents jobs:\n" in self.template:
            r_start = self.template.index("Results from openQA incidents jobs:\n")
            try:
                r_end = self.template.index("End of openQA Incidents results\n") + 1
            except ValueError:
                r_end = self.template.index("source code change review:\n", 0) - 1

            del self.template[r_start:r_end]

        index = self.template.index("source code change review:\n", 0) - 1
        for line in reversed(openqa):
            self.template.insert(index, line)

        index = self.template.index("source code change review:\n", 0) - 1
        self.template.insert(index, "\n")
        self.template.insert(index + 1, "\End of openQA Incidents results\n")
        self.template.insert(index + 2, "\n")

    def install_results(self):
        index = self.template.index("Test results by product-arch:\n", 0)
        self.template.insert(
            index + 3,
            "All installation tests done in openQA please see installlogs section\n",
        )
        self.template.insert(index + 4, "\n")
