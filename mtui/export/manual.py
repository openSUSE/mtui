import os.path
import re
from itertools import zip_longest
from logging import getLogger

from mtui.types.rpmver import RPMVersion

from .base import BaseExport

logger = getLogger("mtui.export.manual")


class ManualExport(BaseExport):
    """Manual workflow export"""

    def get_logs(self, hosts, *args, **kwds):
        filepath = self.config.template_dir / str(self.rrid) / self.config.install_logs
        ilogs = zip_longest(hosts, map(self._host_installog_to_template, hosts))
        filenames = []
        # first export files
        for i, y in ilogs:
            fn = filepath / (i + ".log")
            filenames.append(i + ".log")
            self._writer(fn, y)

        return filenames

    def _fillup_hosts_to_template(self):
        # for each host/system of the mtui session, search for the correct location
        # in the template. disabled hosts are not excluded.
        # if the location was found, add the hostname.
        # if the location isn't found, it's considered that it doesn't exist.
        # in this case, a whole new host section including the systemname is added.
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
                logger.debug(f"host section {hostname} not found, searching system")
                # systemname/reference host string in the maintenance template
                # in case the hostname is not yet set
                line = "{systemtype} (reference host: ?)\n"
                try:
                    # trying again with a not yet set hostname
                    index = self.template.index(line)
                    self.template[index] = "{systemtype} (reference host: {hostname})\n"
                except ValueError:
                    # system line still not found (not with already set hostname, nor
                    # with not yet set hostname). create new one
                    logger.debug(
                        f"system section {systemtype} not found, creating new one"
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
                    # scripts:
                    #
                    # => PASSED/FAILED
                    #
                    # comment: (none)
                    #
                    self.template.insert(index, "\n")
                    index += 1
                    self.template.insert(
                        index, f"{systemtype} (reference host: {hostname})\n"
                    )
                    index += 1
                    self.template.insert(index, "--------------\n")
                    index += 1
                    if systemtype.startswith("caasp"):
                        self.template.insert(
                            index,
                            f"Please check the install logs for the transactional update on host {hostname}\n\n",
                        )
                        continue
                    self.template.insert(index, "before:\n")
                    index += 1
                    self.template.insert(index, "after:\n")
                    index += 1
                    self.template.insert(index, "scripts:\n")
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

        # add package version log and script results for each host to the template
        for host in self.results:
            versions = {}
            hostname = host.hostname
            systemtype = host.system

            # Skip the caasp hosts
            if systemtype.startswith("caasp"):
                continue

            # search for system position which is already existing in the template
            # or was created in the previous step.
            line = f"{systemtype} (reference host: {hostname})\n"
            try:
                index = self.template.index(line)
            except ValueError:
                # host section not found (this should really not happen)
                # proceed with the next one.
                logger.warning(f"host section {hostname} not found")
                continue
            for state in ["before", "after"]:
                versions[state] = {}
                try:
                    index = self.template.index("      {state}:\n", index) + 1
                except ValueError:
                    try:
                        index = self.template.index(f"{state}:\n", index) + 1
                    except ValueError:
                        logger.error(f"{state} packages section not found")
                        continue

                for package in host.packages.values():
                    name = package.name
                    version = package.__getattribute__(state)
                    versions[state].update({name: version})
                    try:
                        # if the package version was already exported, overwrite it with
                        # the new version. if the package version was not yet exported,
                        # add a new line
                        if name in self.template[index]:
                            # if package version is 0, package isn't installed
                            if version != "None":
                                self.template[index] = f"\t{name}-{version}\n"
                            else:
                                self.templatet[
                                    index
                                ] = f"\tpackage {name} is not installed\n"
                        else:
                            if version != "None":
                                self.template.insert(index, f"\t{name}-{version}\n")
                            else:
                                self.template.insert(
                                    index, f"\tpackage {name} is not installed\n"
                                )
                        index += 1
                    except Exception:
                        pass
            try:
                # search for scripts starting point
                index = self.template.index("scripts:\n", index - 1) + 1
            except ValueError:
                # if no scripts section is found, add a new one
                logger.debug("scripts section not found, adding one")
                self.template.insert(index, "      scripts:\n")
                index += 1

            # if the package versions were not updated or one of the testscripts
            # failed, set the result to FAILED, otherwise to PASSED
            failed = False
            for package in versions["before"].keys():
                # check if the packages have a higher version after the update
                if (
                    versions["after"][package] != "None"
                    and versions["before"][package] != "None"
                ):
                    if not RPMVersion(versions["before"][package]) < RPMVersion(
                        versions["after"][package]
                    ):
                        failed = True
            if failed:
                logger.warning(
                    f"installation test result on {hostname} set to FAILED as some packages were not updated. please override manually."
                )

            # temporary variable to avoid repeating the same script. We only want the
            # last result, so we store the previous position
            template_log = host.hostlog
            scripts = {}
            for cmdlog in template_log:
                # search for check scripts in the xml and inspect return code
                # return code values:   0 SUCCEEDED
                #                       1 FAILED
                #                       2 INTERNAL ERROR
                #                       3 NOT RUN
                try:
                    # name == command, exitcode == exitcode
                    name = cmdlog.command
                    exitcode = cmdlog.exitcode

                except Exception:
                    continue

                # check if command is a compare_* script
                if "scripts/compare/compare_" in name:
                    scriptname = os.path.basename(name.split(" ")[0])
                    scriptname = scriptname.replace("compare_", "")
                    scriptname = scriptname.replace(".pl", "")
                    scriptname = scriptname.replace(".sh", "")

                    # move on if the script wasn't run
                    if exitcode == 3:
                        continue

                    if exitcode == 0:
                        result = "SUCCEEDED"
                    elif exitcode == 1:
                        failed = True
                        result = "FAILED"
                    else:
                        failed = True
                        result = "INTERNAL ERROR"

                    scriptline = "\t{0:25}: {1}\n".format(scriptname, result)

                    if scriptname in scripts:
                        self.template[scripts[scriptname]] = scriptline
                    else:
                        scripts[scriptname] = index
                        if scriptname in self.template[index]:
                            self.template[index] = scriptline
                        else:
                            self.template.insert(index, scriptline)

                        index += 1

            if "PASSED/FAILED" in self.template[index + 1]:
                if failed:
                    self.template[index + 1] = "=> FAILED\n"
                else:
                    self.template[index + 1] = "=> PASSED\n"

    def _host_installog_to_template(self, target):

        t = []
        try:
            host_log = [host for host in self.results if host.hostname == target][0]
        except IndexError:
            return []

        # add hostname to indicate from which host the log was exported
        t.append(f"log from {host_log.hostname}:\n")
        for cmd_log in host_log.hostlog:
            cmd = cmd_log.command
            if cmd.startswith("zypper ") or cmd.startswith("transactional-update"):
                t.append("# {!s}\n{!s}\n".format(cmd, cmd_log.stdout))
        return t

    def install_results(self):
        hosts = [h.hostname for h in self.results]
        c_host = None
        tmp_template = []
        for line in self.template:
            match = re.search(r"reference host:\s (.*)$", line)
            if match:
                c_host = match.group(0)
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

    def run(self, hosts, *args, **kwds):
        self.install_results()
        self.inject_openqa()
        self.inject_smelt()
        filenames = self.get_logs(hosts)
        self.installlogs_lines(filenames)
        self.cut_smelt_data()
        self.add_sysinfo()
        self.dedup_lines()
        return self.template
