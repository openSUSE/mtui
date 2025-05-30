from abc import ABC, abstractmethod
from collections.abc import Callable, Iterable
import concurrent.futures
from errno import EEXIST, ENOENT
import glob
from json import loads
from json.decoder import JSONDecodeError
from logging import getLogger
import os
from pathlib import Path
import re
import shutil
import stat
from traceback import format_exc
from typing import Any, Literal
from urllib.request import urlopen

from ..config import Config
from ..exceptions import UpdateError
from ..refhost import Attributes, RefhostsFactory, RefhostsResolveFailed
from ..target import Target
from ..target.hostgroup import HostsGroup
from ..template import TemplateIOError, TestReportAlreadyLoaded
from ..types import Product, TargetMeta
from ..utils import ensure_dir_exists

logger = getLogger("mtui.template.testreport")


re_ver = re.compile(r"(\S+)\s+(\S+)")


class TestReport(ABC):
    # FIXME: the code around read() (_open_and_parse, _parse and factory
    # _factory_md5) is weird a lot.
    # Firstly, it might clear some things up to change the open/read
    # things to file-like interface.

    refhostsFactory = RefhostsFactory

    @property
    @abstractmethod
    def _type(self) -> str:
        """
        :return: str Short human readable description of the TestReport
            type.
        """
        ...

    def __init__(self, config: Config, scripts_src_dir: Path | None = None) -> None:
        self.config: Config = config

        self._scripts_src_dir: Path = (
            scripts_src_dir if scripts_src_dir else config.datadir.joinpath("scripts")  # type: ignore
        )

        self.directory: Path = config.template_dir

        # Note: the default values here are unchanged from the previous
        # class Metadata for backward compaibility purposes, so we don't
        # have to modify every user of this class at the same time as
        # refactoring the internals.
        self.path: Path | None = None
        """
        :param path: path to the testreport file if loaded, otherwise None
        """
        self.systems: dict[str, str] = {}
        """
        :type systems: dict str -> str
        :param systems: hostname -> system
        """
        self.targets = HostsGroup([])
        """
        :type  targets: dict(hostname = L{Target})
            where hostname = str
        """
        self.update_repos: dict[Product, str] = {}
        """
        :type update_repos dict(Product = repository)
           where Product = namedtuple
                 repository = str
        """
        self.hostnames: set[str] = set()
        self.bugs: dict[str, str] = {}
        self.jira: dict[str, str] = {}
        self.testplatforms: list[str] = []
        self.products: list[str] = []
        self.category: str = ""
        self.packager: str = ""
        self.reviewer: str = ""
        self.repository: str = ""
        self.packages = {}

        self._attrs = [
            "products",
            "category",
            "packager",
            "reviewer",
            "packages",
            "bugs",
            "repository",
        ]
        """
        :type attrs: [str]
        :param attrs: attributes expected to exist on `self` after
            parsing the template
        """

        self.openqa: dict[str, Any] = {"auto": None, "kernel": []}

    @property
    @abstractmethod
    def id(self) -> str: ...

    def _open_and_parse(self, path: Path) -> None:
        metadata = path.parent / "metadata.json"
        try:
            tpl = path.read_text(errors="replace")
        except FileNotFoundError as e:
            args = list(e.args) + [e.filename]
            e_new = TemplateIOError(*args)
            e_new.__cause__ = e  # PEP 3134
            raise e_new

        data = None
        if metadata.exists() and metadata.is_file():
            data = metadata.read_text()
            try:
                data = loads(data)
            except JSONDecodeError:
                data = None

        if data:
            self._parse_json(data, tpl)
        else:
            self._parse(tpl)

    def read(self, path: Path) -> None:
        self._open_and_parse(path)
        self.path = path.resolve()
        self._update_repos_parse()
        if self.config.chdir_to_template_dir:  # type: ignore
            os.chdir(path.parent)

        self.copy_scripts()

    @abstractmethod
    def _parser(self) -> dict[str, Any]:
        """
        :returns: L{MetadataParser}
        """

    def _parse(self, tpl: str) -> None:
        """
        Parse qam testreport template into self attributes

        :type tpl_: file like object
        :param tpl_: opened template to read
        """

        if self.path:
            raise TestReportAlreadyLoaded(self.path)

        parser = self._parser()["full"]

        for line in tpl.splitlines():
            parser.parse(self, line)

        self._warn_missing_fields()

    def _parse_json(self, data, tpl: str) -> None:
        if self.path:
            raise TestReportAlreadyLoaded(self.path)

        parser_json = self._parser()["json"]
        parser_hosts = self._parser()["hosts"]

        for line in tpl.splitlines():
            parser_hosts.parse(self, line)

        parser_json.parse(self, data)

        self._warn_missing_fields()

    def _warn_missing_fields(self) -> None:
        missing = {x for x in self._attrs if not getattr(self, x)}

        if missing:
            msg = "TestReport: missing fields: {}".format(missing)
            logger.warning(msg)

    def get_package_list(self):
        ret = []
        for key in self.packages:
            for k in self.packages[key].keys():
                ret.append(k)
        # deduplicate list
        ret = list(set(ret))

        return ret

    @abstractmethod
    def list_update_commands(self, targets: HostsGroup, display) -> None: ...

    def perform_get(self, targets: HostsGroup, remote: Path):
        local = self.report_wd("downloads", remote.name, filepath=True)

        targets.sftp_get(remote, local)

    def perform_prepare(self, targets: HostsGroup, **kw) -> None:
        targets.perform_prepare(self.get_package_list(), self, **kw)

    def perform_update(self, targets: HostsGroup, params: list[str]) -> None:
        """
        :type  targets: dict(hostname = L{Target})
            where hostname = str
        """
        targets.add_history(["update", str(self.id), " ".join(self.get_package_list())])

        try:
            targets.perform_update(self, params)
        except UpdateError as e:
            logger.error("Update failed: %s" % e)
            logger.warning("Error while updating. Rolling back changes")
            self.perform_downgrade(targets)

    def perform_downgrade(self, targets):
        targets.add_history(
            ["downgrade", str(self.id), " ".join(self.get_package_list())]
        )
        targets.perform_downgrade(self.get_package_list(), self)

    def perform_install(self, targets: HostsGroup, packages) -> None:
        targets.add_history(["install", packages])

        targets.perform_install(packages)

    def perform_uninstall(self, targets: HostsGroup, packages) -> None:
        targets.add_history(["uninstall", packages])
        targets.perform_uninstall(packages)

    def copy_scripts(self) -> None:
        if not self.path:
            raise RuntimeError("Called while missing path")

        # copy check_* and compare_* scripts to the template directory
        # TODO: do not override
        src = self._scripts_src_dir
        dst = self.scripts_wd()

        ignore = shutil.ignore_patterns("*.svn")

        self._copy_scripts(src, dst, ignore)
        self._ensure_executable("{0}/*/compare_*".format(dst))

    def _copy_scripts(self, src: Path, dst: Path, ignore: Callable) -> None:
        try:
            logger.debug("Copying scripts: {0} -> {1}".format(src, dst))
            shutil.copytree(src, dst, ignore=ignore)
        except OSError as e:
            # this should not happen but was already noticed once or
            # twice.  probable due to nfs timeouts if mtui was checked
            # out to a nfs mount.
            msg = "Copy scripts {0} -> {1} failed. reason:"
            msg = msg.format(src, dst)
            if e.errno == ENOENT:
                logger.error(msg)
                logger.error(str(e))
                logger.error("copy scripts manually")
                logger.debug(format_exc())
            elif e.errno == EEXIST:
                logger.info("Scripts are in place")
            else:
                raise

    @staticmethod
    def _ensure_executable(pattern) -> None:
        for i in glob.glob(pattern):
            # make sure the compare scripts (which run localy) are
            # executable
            # TODO: add test that the scripts indeed are +x
            st = os.stat(i)
            os.chmod(i, st.st_mode | stat.S_IEXEC)

    def connect_target(
        self, host
    ) -> tuple[Target, str] | tuple[Literal[False], Literal[False]]:
        try:
            target = Target(
                self.config,
                host,
                self.packages,
                timeout=self.config.connection_timeout,
            )
            target.connect()
            new_system = str(target.system)
        except KeyboardInterrupt:
            logger.warning("Connection to %s canceled by user", host)
            return False, False
        except Exception:
            logger.debug(format_exc())
            msg = f"failed to add host {host} to target list"
            logger.warning(msg)
            return False, False
        else:
            return target, new_system

    def connect_targets(self) -> None:
        targets: dict[str, Target] = {}
        new_systems: dict[str, str] = {}
        executor = concurrent.futures.ThreadPoolExecutor()
        hosts: set[str] = {host for host in self.hostnames if host not in self.targets}

        if hosts:
            logger.info("Adding %s" % hosts)
        else:
            logger.info("No refhosts to add")

        connections = {}
        try:
            connections = {
                executor.submit(self.connect_target, host): host for host in hosts
            }
            done, _ = concurrent.futures.wait(connections)
            for future in done:
                host = connections[future]
                # TODO: how to type annatate this or how to change it to be compactible with type hints?
                targets[host], new_systems[host] = future.result()  # type: ignore
        except KeyboardInterrupt:
            for future in connections.keys():
                future.cancel()
            logger.debug("CTRL-C .. ...")

            # explicitly call del over Target instances
            for host in list(targets.keys()):
                del targets[host]
            targets = {}
            logger.warning("Connection to refhosts cancelled by user")
        finally:
            executor.shutdown(wait=False)
            del connections
            del executor

        # We need to be sure that only the system property only have the  connected hosts
        self.systems = {host: system for host, system in new_systems.items() if system}
        for t in self.targets.copy():
            if not self.targets[t].connection.is_active():
                del self.targets[t]

        self.targets.update(
            {host: target for host, target in targets.items() if target}
        )

    def add_target(self, hostname: str) -> None:
        if hostname in self.targets:
            logger.warning(
                "already connected to %s, skipping.", self.targets[hostname].hostname
            )
            return
        try:
            self.targets[hostname] = Target(self.config, hostname, self.packages)
            self.targets[hostname].connect()

            if self:
                self.systems[hostname] = str(self.targets[hostname].system)

        except Exception:
            if hostname in self.targets:
                del self.targets[hostname]
            if hostname in self.systems:
                del self.systems[hostname]
            logger.warning("failed to add host {0} to target list".format(hostname))
            logger.debug(format_exc())

    def refhosts_from_tp(self, testplatform) -> None:
        try:
            refhosts = self.refhostsFactory(self.config)
        except RefhostsResolveFailed:
            return

        try:
            hostnames = refhosts.search(Attributes.from_testplatform(testplatform))
        except (ValueError, KeyError):
            hostnames = []
            msg = "failed to parse testplatform {0!r}"
            logger.warning(msg.format(testplatform))
        else:
            if not hostnames and "openstack" not in testplatform:
                msg = "nothing found for testplatform {0!r}"
                logger.warning(msg.format(testplatform))
        self.hostnames.update(set(hostnames))

    def list_bugs(self, sink, arg):
        return sink(self.bugs, self.jira, arg)

    def _show_yourself_data(self) -> list[tuple[str, str]]:
        return (
            [
                ("Category", self.category),
                ("Hosts", " ".join(sorted(self.systems.keys()))),
                ("Reviewer", self.reviewer),
                ("Packager", self.packager),
                ("Bugs", ", ".join(sorted(self.bugs.keys()))),
                ("Jira", ", ".join(sorted(self.jira.keys()))),
                ("Packages", " ".join(sorted(self.get_package_list()))),
                ("Build checks", self._testreport_url()[:-3] + "build_checks"),
                ("Testreport", self._testreport_url()),
                ("Repository", self.repository),
            ]
            + [("Testplatform", x) for x in self.testplatforms]
            + [("Products", x) for x in self.products]
        )

    def show_yourself(self, writer) -> None:
        self._aligned_write(writer, self._show_yourself_data())

    @staticmethod
    def _aligned_write(writer, data: Iterable[tuple[str, str]]) -> None:
        """
        :type data:  [(str, str)]
        :param data: (key, value)
        """
        for x in sorted(data):
            name, value = x
            if value:
                writer.write(f"{name:15}: {value}\n")

    def _testreport_url(self) -> str:
        return "/".join([self.config.reports_url, str(self.id), "log"])  # type: ignore

    def _fancy_report_url(self) -> str:
        return "/".join([self.config.fancy_reports_url, str(self.id), "log"])  # type: ignore

    def local_wd(self, *paths) -> Path:
        """
        :return: str local working directory
        """
        return self._wd(self.config.local_tempdir, str(self.id), *paths)  # type: ignore

    def report_wd(self, *paths, **kw) -> Path:
        """
        :return: str local working directory relative to the testreport
            checkout.
        """
        assert self.path, "empty path"

        return self._wd(self.path.parent, *paths, **kw)

    @staticmethod
    def _wd(*paths, **kwargs) -> Path:
        return ensure_dir_exists(*paths, **kwargs)

    def target_wd(self, *paths) -> Path:
        """
        :return: str remote working directory on SUT
        """
        return self.config.target_tempdir.joinpath(str(self.id), *paths)  # type: ignore

    def scripts_wd(self, *paths):
        """
        :return: str path to the scripts dir joined with paths

        Note this method does not create the directories as needed
        because that's handled by L{TestReport.copy_scripts}
        """
        return self.report_wd().joinpath(*["scripts"] + list(paths))

    def __repr__(self):
        return "<{0}.{1} {2}>".format(self.__module__, self.__class__.__name__, self.id)

    def run_scripts(self, s, targets: HostsGroup) -> None:
        """
        :type s: L{Script} class
        """

        d = self.scripts_wd(s.subdir)

        # os.walk returns path as string and list of string with filenames
        for r, _, filelist in os.walk(d):
            if r == str(d):
                for f in filelist:
                    x = s(self, d / f)
                    x.run(targets)

    def download_file(self, from_, into) -> None:
        logger.info("Downloading %s", from_)
        from contextlib import closing

        with open(into, "wb") as dst, closing(urlopen(from_)) as src:
            dst.writelines(src)

    def list_versions(self, sink, targets: HostsGroup, packages):
        query = r"""
            for p in {!s}; do \
                zypper -n search -s --match-exact -t package $p; \
            done \
            | grep -e ^[iv] \
            | awk -F '|' '{{ print $2 $4 }}' \
            | sort -u
        """

        packages = packages or self.get_package_list()

        targets.run(query.format(" ".join(packages)))

        # this is a bit convoluted because the data is aggregated
        # on display (see the example in CommandPrompt#do_list_versions)
        # but acquired piecemeal in random order.
        #
        # input for a single target:
        #
        #   line = PKKGNAME +SP PKGVER
        #   input = *(line EOL)

        # by_host_pkg[hostname][package] = [version, ...]
        by_host_pkg = {}
        for hn, t in list(targets.items()):
            by_host_pkg[hn] = {}
            for line in t.lastout().split("\n"):
                if match := re.search(re_ver, line):
                    pkg, ver = match.group(1), match.group(2)
                    by_host_pkg[hn].setdefault(pkg, []).append(ver)
                else:
                    continue

        # by_pkg_vers[package][(version, ...)] = [hostname, ...]
        by_pkg_vers = {}
        for hn, pvs in list(by_host_pkg.items()):
            for pkg, vs in list(pvs.items()):
                by_pkg_vers.setdefault(pkg, {}).setdefault(tuple(vs), []).append(hn)

        # by_hosts_pkg[(hostname, ...)] = [(package, (version, ...)), ...]
        by_hosts_pkg = {}
        for pkg, vshs in list(by_pkg_vers.items()):
            for vs, hs in list(vshs.items()):
                by_hosts_pkg.setdefault(tuple(hs), []).append((pkg, vs))

        return sink(targets, by_hosts_pkg)

    def report_results(self, targetHosts=None) -> list[TargetMeta]:
        results = []

        if targetHosts is not None:
            targets = list(targetHosts)
        else:
            targets = self.targets.values()

        for t in targets:
            results.append(TargetMeta(t.hostname, str(t.system), t.packages, t.out))

        return results

    @abstractmethod
    def _update_repos_parser(self) -> dict[Product, str]:
        """Parse and store update repositories per product and arch"""
        pass

    def _update_repos_parse(self) -> None:
        self.update_repos = self._update_repos_parser()
