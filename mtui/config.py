"""Handles the configuration for mtui.

This module reads configuration files, sets default values, and allows for
overriding configuration options with command-line arguments.
"""

import configparser
import getpass
from argparse import Namespace
from collections.abc import Callable
from dataclasses import dataclass, field
from logging import getLogger
from os import getenv
from pathlib import Path
from typing import Any

from .datafiles import terms_path
from .messages import InvalidLocationError
from .refhost import RefhostsFactory, RefhostsResolveFailedError

logger = getLogger("mtui.config")


def _identity(x: Any) -> Any:
    """Default fixup: return the value unchanged."""
    return x


@dataclass(frozen=True, slots=True)
class ConfigOption:
    """Declarative description of a single configuration option.

    Replaces the historical 5-tuple shape used in ``Config._define_config_options``
    with a named-field record. Behaviour is unchanged from the tuple form:

    - ``getter`` is invoked as ``getter(*ini_path)`` to read the raw value
      from the INI file (typically ``config.get``, ``config.getint`` or
      ``config.getboolean``).
    - When the read fails (option absent or malformed), the option falls
      back to ``default`` (called if callable, otherwise used verbatim).
    - ``fixup`` is applied to the successfully-read value to coerce it to
      the final attribute type (e.g. ``Path``, ``int``).
    """

    attr: str
    ini_path: tuple[str, str]
    default: Any
    fixup: Callable[[Any], Any] = field(default=_identity)
    # ``getter`` cannot have a meaningful default at class-definition time
    # because it is a bound method of the per-instance ConfigParser; the
    # caller fills it in (defaulting to ``config.get``) when building the list.
    getter: Callable[..., Any] = field(default=_identity)


class Config:
    """Read and store the variables from mtui config files."""

    # -- Attributes set dynamically by _parse_config() via setattr() --
    template_dir: Path
    local_tempdir: Path
    session_user: str
    install_logs: Path
    connection_timeout: int
    svn_path: str
    bugzilla_url: str
    reports_url: str
    fancy_reports_url: str
    qem_dashboard_api: str
    target_tempdir: Path
    chdir_to_template_dir: bool
    refhosts_resolvers: str
    refhosts_https_uri: str
    refhosts_https_expiration: int
    refhosts_path: Path
    use_keyring: bool
    report_bug_url: str
    openqa_instance: str
    openqa_instance_baremetal: str
    openqa_install_distri: str
    openqa_install_logs: str
    openqa_kernel_install_logs: str
    threshold: int
    gitea_token: str
    ssh_strict_host_key_checking: str

    # -- Attributes set externally in main.py --
    kernel: bool
    auto: bool
    distro: str
    distro_ver: str
    distro_kernel: str

    def __init__(self, path: Path | None, refhosts=RefhostsFactory) -> None:
        """Initializes the configuration object.

        This method reads config files, and sets up options.

        Args:
            path: An optional path to a specific config file.
            refhosts: The factory to use for creating refhosts.

        """
        self.refhosts = refhosts
        self.__location = "default"

        if path:
            self.configfiles = [path]
        elif _pth := getenv("MTUI_CONF"):
            self.configfiles = [Path(_pth).expanduser()]
        else:
            self.configfiles = [Path("/etc/mtui.cfg"), Path("~/.mtuirc").expanduser()]
        self.read()

        self._define_config_options()
        self._parse_config()
        self._list_terms()

    def read(self) -> None:
        """Reads the configuration files."""
        self.config = configparser.ConfigParser(inline_comment_prefixes=("#", ";"))
        try:
            self.config.read(self.configfiles)
        except configparser.Error as e:
            logger.error(e)

    @property
    def location(self) -> str:
        """The location property."""
        return self.__location

    @location.setter
    def location(self, x: str) -> None:
        """Sets the location property.

        Args:
            x: The new location.

        """
        try:
            self.refhosts(self).check_location_sanity(x)
        except InvalidLocationError as e:
            logger.error(e)
            return
        except RefhostsResolveFailedError:
            logger.error("Can't read `refhosts.yml` file, no valid refhosts database")
            return

        self.__location = x

    def _parse_config(self) -> None:
        """Parses the configuration options from the config files."""
        for opt in self.data:
            try:
                val = self._get_option(opt.ini_path, opt.getter)
            except Exception:
                logger.debug(
                    "config option %s not in INI; using default",
                    opt.attr,
                    exc_info=True,
                )
                val = opt.default() if callable(opt.default) else opt.default

            setattr(self, opt.attr, opt.fixup(val))
            logger.debug('config.%s set to "%s"', opt.attr, val)

    def _define_config_options(self) -> None:
        """Defines all available configuration options."""

        def expanduser(p: Path | str) -> Path:
            return Path(p).expanduser()

        get = self.config.get
        getint = self.config.getint
        getboolean = self.config.getboolean

        self.data: list[ConfigOption] = [
            ConfigOption(
                "template_dir",
                ("mtui", "template_dir"),
                lambda: Path(getenv("TEMPLATE_DIR", ".")),
                expanduser,
                get,
            ),
            ConfigOption(
                "local_tempdir",
                ("mtui", "tempdir"),
                lambda: Path(getenv("TMPDIR", "/tmp")),
                expanduser,
                get,
            ),
            ConfigOption(
                "session_user",
                ("mtui", "user"),
                getpass.getuser,
                getter=get,
            ),
            ConfigOption(
                "install_logs",
                ("mtui", "install_logs"),
                Path("install_logs"),
                Path,
                get,
            ),
            # connection.timeout appears to be in units of seconds as
            # indicated by
            # http://www.lag.net/paramiko/docs/paramiko.Channel-class.html#gettimeout
            ConfigOption(
                "connection_timeout",
                ("mtui", "connection_timeout"),
                300,
                int,
                get,
            ),
            ConfigOption(
                "svn_path",
                ("svn", "path"),
                "svn+ssh://svn@qam.suse.de/testreports",
                getter=get,
            ),
            ConfigOption(
                "bugzilla_url",
                ("url", "bugzilla"),
                "https://bugzilla.suse.com",
                getter=get,
            ),
            ConfigOption(
                "reports_url",
                ("url", "testreports"),
                "https://qam.suse.de/testreports",
                getter=get,
            ),
            ConfigOption(
                "fancy_reports_url",
                ("url", "fancy_reports"),
                "https://qam.suse.de/reports",
                getter=get,
            ),
            ConfigOption(
                "qem_dashboard_api",
                ("qem_dashboard", "api"),
                "http://dashboard.qam.suse.de/api",
                getter=get,
            ),
            ConfigOption(
                "target_tempdir",
                ("target", "tempdir"),
                Path("/tmp"),
                Path,
                get,
            ),
            ConfigOption(
                "chdir_to_template_dir",
                ("mtui", "chdir_to_template_dir"),
                False,
                getter=getboolean,
            ),
            ConfigOption(
                "refhosts_resolvers",
                ("refhosts", "resolvers"),
                "https,path",
                getter=get,
            ),
            ConfigOption(
                "refhosts_https_uri",
                ("refhosts", "https_uri"),
                "https://qam.suse.de/refhosts/refhosts.yml",
                getter=get,
            ),
            ConfigOption(
                "refhosts_https_expiration",
                ("refhosts", "https_expiration"),
                3600 * 12,
                int,
                getint,
            ),
            ConfigOption(
                "refhosts_path",
                ("refhosts", "path"),
                Path("/usr/share/qam-metadata/refhosts.yml"),
                Path,
                get,
            ),
            ConfigOption(
                "use_keyring",
                ("mtui", "use_keyring"),
                False,
                bool,
                getboolean,
            ),
            ConfigOption(
                "report_bug_url",
                ("mtui", "report_bug_url"),
                "https://bugzilla.suse.com/enter_bug.cgi?classification=40&product=Testenvironment&submit=Use+This+Product&component=MTUI",
                getter=get,
            ),
            # openQA connector
            ConfigOption(
                "openqa_instance",
                ("openqa", "openqa"),
                "https://openqa.suse.de",
                getter=get,
            ),
            ConfigOption(
                "openqa_instance_baremetal",
                ("openqa", "baremetal"),
                "http://openqa.qam.suse.cz",
                getter=get,
            ),
            ConfigOption(
                "openqa_install_distri",
                ("openqa", "distri"),
                "sle",
                getter=get,
            ),
            ConfigOption(
                "openqa_install_logs",
                ("openqa", "install_logfile"),
                "update_install-zypper.log",
                getter=get,
            ),
            ConfigOption(
                "openqa_kernel_install_logs",
                ("openqa", "kernel_install_logfile"),
                "update_kernel-zypper.log",
                getter=get,
            ),
            # config for template export
            ConfigOption(
                "threshold",
                ("template", "smelt_threshold"),
                10,
                int,
                getint,
            ),
            # process location last as that needs to access
            # RefhostsFactory which need access to parts of config.
            ConfigOption(
                "location",
                ("mtui", "location"),
                "default",
                getter=get,
            ),
            ConfigOption(
                "gitea_token",
                ("gitea", "token"),
                getenv("GITEA_TOKEN", ""),
                getter=get,
            ),
            ConfigOption(
                "ssh_strict_host_key_checking",
                ("connection", "ssh_strict_host_key_checking"),
                "auto_add",
                str,
                get,
            ),
        ]

    def _list_terms(self) -> None:
        """Finds available terminal scripts."""
        scripts: list[str] = [x.name[5:-3] for x in terms_path().glob("term.*.sh")]
        self.termnames = scripts

    def _get_option(self, secopt, getter):
        """Gets an option from the configuration.

        Args:
            secopt: A tuple containing the section and option name.
            getter: The function to use to get the option.

        Returns:
            The value of the option.

        """
        try:
            return getter(*secopt)
        except (configparser.NoSectionError, configparser.NoOptionError):
            msg = "Config option {}.{} not found.".format(*secopt)
            logger.debug(msg)
            raise
        except Exception:
            msg = "Config option {0}.{1} extraction from {2} " + "failed."
            logger.error(msg.format((*secopt, self.configfiles)))
            raise

    def merge_args(self, args: Namespace) -> None:
        """Merges command-line arguments into the configuration.

        Args:
            args: The parsed command-line arguments.

        """
        if args.location:
            self.location = args.location

        if args.template_dir:
            self.template_dir = args.template_dir

        if args.connection_timeout:
            self.connection_timeout = args.connection_timeout

        if args.gitea_token:
            self.gitea_token = args.gitea_token
