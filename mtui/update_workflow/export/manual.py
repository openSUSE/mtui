"""An exporter for the manual workflow."""

import contextlib
import re
from itertools import zip_longest
from logging import getLogger
from pathlib import Path
from typing import Any

from ...types import FileList
from .base import BaseExport

logger = getLogger("mtui.export.manual")


class ManualExport(BaseExport):
    """An exporter for the manual workflow."""

    # Injected via BaseExport.__init__(**kwargs)
    results: Any

    def get_logs(self, hosts, *args, **kwds) -> list[Path]:
        """Gets the logs from the target hosts.

        Args:
            hosts: A list of hosts to get logs from.
            *args: Additional arguments (not used).
            **kwds: Additional keyword arguments (not used).

        Returns:
            A list of paths to the log files.

        """
        filepath = self.config.template_dir / str(self.rrid) / self.config.install_logs
        ilogs = zip_longest(hosts, map(self._host_installog_to_template, hosts))
        filenames = []
        # first export files
        for i, y in ilogs:
            fn = filepath / (i + ".log")
            filenames.append(i + ".log")
            self._writer(fn, y)

        return filenames

    def _fillup_hosts_to_template(self) -> None:
        """Fills up the template with host information."""
        # for each host/system of the mtui session, search for the correct location
        # in the template. disabled hosts are not excluded.
        # if the location was found, add the hostname.
        # if the location isn't found, it's considered that it doesn't exist.
        # in this case, a whole new host section including the systemname is added.
        # --- self.results --- is injected from BaseExport.__init__ keyword argument
        for host in self.results:
            hostname = host.hostname
            systemtype = host.system
            # systemname/reference host string in the maintenance template
            # in case the hostname is already set
            line = f"{systemtype} (reference host: {hostname})\n"
            try:
                # position of the system line
                index = self.template.index(line)
            except ValueError:
                # system line not found
                logger.debug("host section %s not found, searching system", hostname)
                # systemname/reference host string in the maintenance template
                # in case the hostname is not yet set
                line = f"{systemtype} (reference host: ?)\n"
                try:
                    # trying again with a not yet set hostname
                    index = self.template.index(line)
                    self.template[index] = (
                        f"{systemtype} (reference host: {hostname})\n"
                    )
                except ValueError:
                    # system line still not found (not with already set hostname, nor
                    # with not yet set hostname). create new one
                    logger.debug(
                        "system section %s not found, creating new one", systemtype
                    )
                    # starting point, just above the hosts section
                    line = "Test results by product-arch:\n"

                    try:
                        index = self.template.index(line) + 2
                    except ValueError:
                        # starting point not found, try again with the deprecated one
                        # from older templates
                        try:
                            line = "Test results by test platform:\n"
                            index = self.template.index(line) + 2
                        except ValueError:
                            # no hostsection found and no starting point for insertion,
                            # bail out and try the next host
                            logger.error("update results section not found")
                            break

                    # insert new package version log at position i.
                    # example:
                    # sles11sp1-i386 (reference host: leo.suse.de)
                    # --------------
                    # before:
                    # after:
                    #
                    # => PASSED/FAILED
                    #
                    # comment: (none)  # noqa: ERA001
                    #
                    self.template.insert(index, "\n")
                    index += 1
                    self.template.insert(
                        index, f"{systemtype} (reference host: {hostname})\n"
                    )
                    index += 1
                    self.template.insert(index, "--------------\n")
                    index += 1
                    self.template.insert(index, "before:\n")
                    index += 1
                    self.template.insert(index, "after:\n")
                    index += 1
                    self.template.insert(index, "\n")
                    index += 1
                    self.template.insert(index, "=> PASSED/FAILED\n")
                    index += 1
                    self.template.insert(index, "\n")
                    index += 1
                    self.template.insert(index, "comment: (none)\n")
                    index += 1
                    self.template.insert(index, "\n")

        # add package version log for each host to the template
        for host in self.results:
            versions = {}
            hostname = host.hostname
            systemtype = host.system

            # search for system position which is already existing in the template
            # or was created in the previous step.
            line = f"{systemtype} (reference host: {hostname})\n"
            try:
                index = self.template.index(line)
            except ValueError:
                # host section not found (this should really not happen)
                # proceed with the next one.
                logger.warning("host section %s not found", hostname)
                continue
            for state in ["before", "after"]:
                versions[state] = {}

                # Bound the header search to this host's own block, so it can
                # never overshoot into a later host's section (nor, for that
                # matter, undershoot into an earlier one). The block ends at
                # the next "reference host:" line, or the end of the
                # template. Recomputed on every iteration (rather than once
                # per host) because inserting the "before" version lines
                # shifts the position of everything after them, including
                # the next host's header.
                block_end = len(self.template)
                for j in range(index + 1, len(self.template)):
                    if "reference host:" in self.template[j]:
                        block_end = j
                        break

                indented_index = None
                unindented_index = None
                with contextlib.suppress(ValueError):
                    indented_index = self.template.index(
                        f"      {state}:\n", index, block_end
                    )
                with contextlib.suppress(ValueError):
                    unindented_index = self.template.index(
                        f"{state}:\n", index, block_end
                    )

                if indented_index is None and unindented_index is None:
                    logger.error("%s packages section not found", state)
                    continue
                # prefer whichever header form is nearest, rather than
                # always favouring the indented one
                if indented_index is None:
                    index = unindented_index + 1
                elif unindented_index is None:
                    index = indented_index + 1
                else:
                    index = min(indented_index, unindented_index) + 1

                for package in host.packages.values():
                    name = package.name
                    version = getattr(package, state)
                    versions[state].update({name: version})
                    try:
                        # if the package version was already exported, overwrite it with
                        # the new version. if the package version was not yet exported,
                        # add a new line
                        if name in self.template[index]:
                            # if package version is 0, package isn't installed
                            if version is not None:
                                self.template[index] = f"\t{name}-{version}\n"
                            else:
                                self.template[index] = (
                                    f"\tpackage {name} is not installed\n"
                                )
                        elif version is not None:
                            self.template.insert(index, f"\t{name}-{version}\n")
                        else:
                            self.template.insert(
                                index, f"\tpackage {name} is not installed\n"
                            )
                        index += 1
                    except IndexError:
                        # the state header was the last template line, so there
                        # is no line at ``index`` to inspect/overwrite. The
                        # template is malformed; skip the line but say so
                        # instead of silently dropping it from the report.
                        logger.warning(
                            "malformed template: cannot write %s version of "
                            "package %s for host %s",
                            state,
                            name,
                            hostname,
                        )
            # if the package versions were not updated, set the result to
            # FAILED, otherwise to PASSED
            failed = False
            for package in versions["before"]:
                # check if the packages have a higher version after the update.
                # ``after`` may be missing this package entirely if its section
                # could not be located (e.g. a malformed template) -- skip the
                # comparison for it rather than raising a KeyError and aborting
                # the whole export.
                after_version = versions["after"].get(package)
                if (
                    after_version is not None
                    and versions["before"][package] is not None
                    and not versions["before"][package] < after_version
                ):
                    failed = True
            if failed:
                logger.warning(
                    "installation test result on %s set to FAILED as some packages were not updated. please override manually.",
                    hostname,
                )

            # flip the verdict placeholder for this host's section. Bounded by the
            # host's trailing ``comment:`` line — or the start of the next host
            # block — so an already-set verdict (from a previous export) is left
            # untouched rather than the next host's grabbed, even if a block is
            # ever malformed (missing its comment line).
            for j in range(index, len(self.template)):
                if "PASSED/FAILED" in self.template[j]:
                    self.template[j] = "=> FAILED\n" if failed else "=> PASSED\n"
                    break
                if (
                    self.template[j].startswith("comment:")
                    or "reference host:" in (self.template[j])
                ):
                    break

    def _host_installog_to_template(self, target) -> list[str]:
        """Converts a host's install log to a template.

        Args:
            target: The target host.

        Returns:
            A list of strings representing the log content.

        """
        t = []
        try:
            host_log = [host for host in self.results if host.hostname == target][0]
        except IndexError:
            return []

        # add hostname to indicate from which host the log was exported
        t.append(f"log from {host_log.hostname}:\n")
        for cmd_log in host_log.hostlog:
            cmd = cmd_log.command
            if "zypper " in cmd or "transactional-update" in cmd:
                t.append(f"# {cmd!s}\n{cmd_log.stdout!s}\n")
        return t

    def install_results(self) -> None:
        """Adds installation results to the template."""
        hosts = [h.hostname for h in self.results]
        c_host = None
        tmp_template = []
        for line in self.template:
            # Track which host section we are in so only the *current
            # session's* hosts get their stale result lines refreshed. The
            # old pattern required two spaces after the colon (the template
            # emits one) and read group(0) (the whole match, never a bare
            # hostname), so no stale line was ever removed. The host line
            # itself is kept -- it is the section header.
            match = re.search(r"reference host:\s+([^)\s]+)", line)
            if match:
                c_host = match.group(1)
                tmp_template.append(line)
                continue

            if c_host is not None and line.startswith("comment:"):
                # End of this host's block (same boundary convention as the
                # verdict loop above). Without the reset the deletion window
                # bled past the last host section and ate tester-authored
                # lines like 'reproducer : FAILED before update' from the
                # regression-tests notes.
                tmp_template.append(line)
                c_host = None
                continue

            if (
                not re.search(
                    r"\s:\s(SUCCEEDED|(?<!PASSED/)FAILED|INTERNAL ERROR)", line
                )
                or c_host not in hosts
            ):
                tmp_template.append(line)
        self.template.clear()
        self.template.extend(tmp_template)

        self._fillup_hosts_to_template()

    def run(self, hosts, *args, **kwds) -> list[str] | FileList:
        """Runs the exporter.

        Args:
            hosts: A list of hosts to export logs from.
            *args: Additional arguments (not used).
            **kwds: Additional keyword arguments (not used).

        Returns:
            The exported template.

        """
        self.install_results()
        self.inject_openqa()
        self.inject_overview()
        filenames = self.get_logs(hosts)
        self.installlogs_lines(filenames)
        self.add_sysinfo()
        self.dedup_lines()
        return self.template
