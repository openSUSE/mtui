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

from ..hosts.refhost import RefhostsFactory, RefhostsResolveFailedError
from .messages import InvalidLocationError
from .paths import terms_path

logger = getLogger("mtui.config")


def _identity(x: Any) -> Any:
    """Default fixup: return the value unchanged."""
    return x


_TRUE_STRINGS = frozenset({"1", "yes", "true", "on"})
_FALSE_STRINGS = frozenset({"0", "no", "false", "off"})


def _parse_ssl_verify(raw: str) -> bool | str:
    """Coerce the ``[mtui] ssl_verify`` value into a ``requests`` ``verify``.

    Accepts the usual boolean spellings (``true``/``false``/``yes``/...)
    and otherwise treats the value as a path to a CA bundle file, which
    :mod:`requests` accepts directly as ``verify``.
    """
    token = raw.strip()
    lowered = token.lower()
    if lowered in _TRUE_STRINGS:
        return True
    if lowered in _FALSE_STRINGS:
        return False
    return token


@dataclass(frozen=True, slots=True)
class ConfigOption:
    """Declarative description of a single configuration option.

    Replaces the historical 5-tuple shape used in ``Config._define_config_options``
    with a named-field record. Behaviour is unchanged from the tuple form:

    - ``getter`` is invoked as ``getter(*ini_path)`` to read the raw value
      from the INI file (typically ``config.get``, ``config.getint`` or
      ``config.getboolean``).
    - On any failure during read OR ``fixup``, the option falls back to
      ``default`` (called if callable, otherwise used verbatim) and the
      failure is logged at ERROR level.
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
    smelt_url: str
    target_tempdir: Path
    chdir_to_template_dir: bool
    refhosts_resolvers: str
    refhosts_https_uri: str
    refhosts_https_expiration: int
    refhosts_path: Path
    use_keyring: bool
    openqa_instance: str
    openqa_instance_baremetal: str
    openqa_install_distri: str
    openqa_install_logs: str
    openqa_kernel_install_logs: str
    gitea_token: str
    ssl_verify: bool | str | None
    ssh_strict_host_key_checking: str
    lock_reap_stale: bool
    lock_stale_age: int
    lock_pi_autolock: bool

    # -- mtui-mcp server (http transport) per-client session registry --
    mcp_session_cap: int
    mcp_session_idle_timeout: int

    # -- Attributes set externally in main.py --
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
        """Parses the configuration options from the config files.

        For each declared :class:`ConfigOption`, reads the raw value via the
        option's ``getter``, applies its ``fixup``, and assigns the result
        to ``self``. On any failure during read OR fixup, logs an ERROR
        naming the option, the offending value (when known), and the
        default being applied, then assigns the default.
        """
        for opt in self.data:
            raw: Any = None
            try:
                raw = self._get_option(opt.ini_path, opt.getter)
                val = opt.fixup(raw)
            except (configparser.NoSectionError, configparser.NoOptionError):
                # Option absent from the INI: not an error, just use the default.
                val = opt.default() if callable(opt.default) else opt.default
            except Exception:
                default_val = opt.default() if callable(opt.default) else opt.default
                logger.exception(
                    "Config option %s (%s.%s) failed to parse value %r; "
                    "falling back to default %r",
                    opt.attr,
                    opt.ini_path[0],
                    opt.ini_path[1],
                    raw,
                    default_val,
                )
                val = default_val

            setattr(self, opt.attr, val)
            logger.debug('config.%s set to "%s"', opt.attr, val)

    def _define_config_options(self) -> None:
        """Defines all available configuration options."""

        def expanduser(p: Path | str) -> Path:
            return Path(p).expanduser()

        get = self.config.get
        getint = self.config.getint
        getboolean = self.config.getboolean

        def get_connection_timeout(section: str, option: str) -> str:
            # Read from the [connection] section, falling back to the legacy
            # [mtui] location so existing configs keep working.
            try:
                return self.config.get(section, option)
            except (configparser.NoSectionError, configparser.NoOptionError):
                return self.config.get("mtui", option)

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
                expanduser,
                get,
            ),
            # Seconds. Bounds both establishing the SSH connection (TCP
            # connect / banner / auth) and remote command execution. Read
            # from [connection], falling back to the legacy [mtui] section.
            ConfigOption(
                "connection_timeout",
                ("connection", "connection_timeout"),
                300,
                int,
                get_connection_timeout,
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
            # SMELT base URL. No default on purpose: it is not part of mtui's
            # public configuration surface, so it is read from ``[smelt] url`` in
            # the user's config and the SMELT features are skipped when unset.
            ConfigOption(
                "smelt_url",
                ("smelt", "url"),
                "",
                getter=get,
            ),
            ConfigOption(
                "target_tempdir",
                ("target", "tempdir"),
                Path("/tmp"),
                expanduser,
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
                expanduser,
                get,
            ),
            ConfigOption(
                "use_keyring",
                ("mtui", "use_keyring"),
                False,
                bool,
                getboolean,
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
            ConfigOption(
                "gitea_token",
                ("gitea", "token"),
                getenv("GITEA_TOKEN", ""),
                getter=get,
            ),
            # Global policy for TLS certificate verification on every
            # outbound HTTP call (see mtui.support.http). Defaults to
            # ``True`` so mtui verifies certificates everywhere out of the
            # box; this requires the SUSE CA in the system trust store to
            # reach internal hosts that present an internal-CA certificate.
            # Set ``ssl_verify = false`` to skip verification everywhere, or
            # point at a CA bundle file with ``ssl_verify = /path/to/ca.pem``.
            ConfigOption(
                "ssl_verify",
                ("mtui", "ssl_verify"),
                True,
                _parse_ssl_verify,
                get,
            ),
            ConfigOption(
                "ssh_strict_host_key_checking",
                ("connection", "ssh_strict_host_key_checking"),
                "auto_add",
                str,
                get,
            ),
            # On connect, force-remove a pre-existing remote lock older
            # than ``lock_stale_age`` seconds regardless of owner. Set
            # ``reap_stale = false`` (or ``stale_age = 0``) to disable.
            ConfigOption(
                "lock_reap_stale",
                ("lock", "reap_stale"),
                True,
                bool,
                getboolean,
            ),
            ConfigOption(
                "lock_stale_age",
                ("lock", "stale_age"),
                86400,
                int,
                getint,
            ),
            # When testing a Product Increment (PI), automatically lock all
            # reference hosts on ``assign`` and unlock them at end of testing
            # (``unassign`` / ``approve`` / ``reject``). Set to false to
            # disable.
            ConfigOption(
                "lock_pi_autolock",
                ("lock", "pi_autolock"),
                True,
                bool,
                getboolean,
            ),
            # ``mtui-mcp`` http transport isolates state per client in a
            # session registry (see mtui.mcp.registry). ``session_cap``
            # bounds how many concurrent client sessions may exist at
            # once (DoS guard against unbounded targets/threads);
            # ``session_idle_timeout`` is the seconds of inactivity
            # after which an idle session is swept and its hosts
            # disconnected. Both are ignored under the stdio transport.
            ConfigOption(
                "mcp_session_cap",
                ("mcp", "session_cap"),
                32,
                int,
                getint,
            ),
            ConfigOption(
                "mcp_session_idle_timeout",
                ("mcp", "session_idle_timeout"),
                1800,
                int,
                getint,
            ),
            # ``location`` MUST be parsed last. Assigning it goes through the
            # ``location`` property setter, which resolves refhosts to validate
            # the location -- and the resolve reads ``ssl_verify`` and the
            # ``refhosts_*`` options. Those must already be set on ``self``, so
            # this option stays at the end of the list. Reordering it earlier
            # reintroduces ``AttributeError: 'Config' object has no attribute
            # 'ssl_verify'`` during config parsing.
            ConfigOption(
                "location",
                ("mtui", "location"),
                "default",
                getter=get,
            ),
        ]

    def _list_terms(self) -> None:
        """Finds available terminal scripts."""
        scripts: list[str] = [x.name[5:-3] for x in terms_path().glob("term.*.sh")]
        self.termnames = scripts

    def _get_option(self, secopt: tuple[str, str], getter: Callable[..., Any]) -> Any:
        """Gets an option from the configuration.

        Args:
            secopt: A tuple containing the section and option name.
            getter: The function to use to get the option.

        Returns:
            The value of the option.

        Raises:
            configparser.NoSectionError / NoOptionError: option absent.
            Exception: any failure raised by ``getter`` (e.g. ``ValueError``
                from ``getint`` / ``getboolean`` on a malformed value); the
                caller (:meth:`_parse_config`) is responsible for logging
                and falling back to the default.

        """
        try:
            return getter(*secopt)
        except (configparser.NoSectionError, configparser.NoOptionError):
            logger.debug("Config option %s.%s not found.", *secopt)
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
