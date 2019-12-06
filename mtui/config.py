#
# mtui config file parser and default values
#

import configparser
import getpass
from collections.abc import Callable
from logging import getLogger
from os import getenv
from pathlib import Path
from traceback import format_exc

from mtui.messages import InvalidLocationError
from mtui.refhost import RefhostsFactory, RefhostsResolveFailed

logger = getLogger("mtui.config")


class InvalidOptionNameError(RuntimeError):
    pass


class Config:

    """Read and store the variables from mtui config files"""

    def __init__(self, path, refhosts=RefhostsFactory):
        self.refhosts = refhosts
        self._location = "default"

        # FIXME: gotta read config overide from env instead of argv
        # because this crap is used as a singleton all over the
        # place
        _pth = getenv("MTUI_CONF")
        if path:
            self.configfiles = [path]
        elif _pth:
            self.configfiles = [Path(_pth).expanduser()]
        else:
            self.configfiles = [Path("/etc/mtui.cfg"), Path("~/.mtuirc").expanduser()]
        self.read()

        self._define_config_options()
        self._parse_config()
        self._handle_testopia_cred()
        self._list_terms()

    def read(self):
        self.config = configparser.ConfigParser(inline_comment_prefixes=("#", ";"))
        try:
            self.config.read(self.configfiles)
        except configparser.Error as e:
            logger.error(e)

    @property
    def location(self):
        return self._location

    @location.setter
    def location(self, x):
        try:
            self.refhosts(self).check_location_sanity(x)
        except (InvalidLocationError) as e:
            logger.error(e)
            return
        except RefhostsResolveFailed:
            logger.error("Can't read `refhosts.yml` file, no valid refhosts database")
            return

        self._location = x

    def _parse_config(self):
        for datum in self.data:
            attr, inipath, default, fixup, getter = datum

            try:
                val = self._get_option(inipath, getter)
            except BaseException:
                if isinstance(default, Callable):
                    val = default()
                else:
                    val = default

            setattr(self, attr, fixup(val))
            logger.debug('config.{!s} set to "{!s}"'.format(attr, val))

    def _define_config_options(self):
        def normalizer(x):
            return x

        def expanduser(p):
            return Path(p).expanduser()

        data = [
            ("datadir", ("mtui", "datadir"), Path("/usr/share/mtui"), expanduser),
            (
                "template_dir",
                ("mtui", "template_dir"),
                lambda: Path(getenv("TEMPLATE_DIR", ".")),
                expanduser,
            ),
            (
                "local_tempdir",
                ("mtui", "tempdir"),
                lambda: Path(getenv("TMPDIR", "/tmp")),
                expanduser,
            ),
            ("session_user", ("mtui", "user"), getpass.getuser),
            ("install_logs", ("mtui", "install_logs"), Path("install_logs"), Path),
            # connection.timeout appears to be in units of seconds as
            # indicated by
            # http://www.lag.net/paramiko/docs/paramiko.Channel-class.html#gettimeout
            ("connection_timeout", ("mtui", "connection_timeout"), 300, int),
            ("svn_path", ("svn", "path"), "svn+ssh://svn@qam.suse.de/testreports"),
            ("bugzilla_url", ("url", "bugzilla"), "https://bugzilla.suse.com"),
            ("reports_url", ("url", "testreports"), "http://qam.suse.de/testreports"),
            (
                "smelt_api",
                ("smelt", "endpoint"),
                "https://smelt.suse.de/graphql/",
            ),
            ("target_tempdir", ("target", "tempdir"), Path("/tmp"), Path),
            (
                "target_testsuitedir",
                ("target", "testsuitedir"),
                Path("/usr/share/qa/tools"),
                Path,
            ),
            (
                "testopia_interface",
                ("testopia", "interface"),
                "https://apibugzilla.novell.com/xmlrpc.cgi",
            ),
            ("testopia_user", ("testopia", "user"), ""),
            ("testopia_pass", ("testopia", "pass"), ""),
            (
                "chdir_to_template_dir",
                ("mtui", "chdir_to_template_dir"),
                False,
                normalizer,
                self.config.getboolean,
            ),
            ("refhosts_resolvers", ("refhosts", "resolvers"), "https,path"),
            (
                "refhosts_https_uri",
                ("refhosts", "https_uri"),
                "https://qam.suse.de/metadata/refhosts.yml",
            ),
            (
                "refhosts_https_expiration",
                ("refhosts", "https_expiration"),
                3600 * 12,
                int,
                self.config.getint,
            ),
            (
                "refhosts_path",
                ("refhosts", "path"),
                Path("/usr/share/qam-metadata/refhosts.yml"),
                Path,
            ),
            (
                "use_keyring",
                ("mtui", "use_keyring"),
                False,
                bool,
                self.config.getboolean,
            ),
            (
                "report_bug_url",
                ("mtui", "report_bug_url"),
                "https://bugzilla.suse.com/enter_bug.cgi?classification=40&product=Testenvironment&submit=Use+This+Product&component=MTUI",
            ),
            # openQA connector
            ("openqa_instance", ("openqa", "openqa"), "https://openqa.suse.de"),
            (
                "openqa_instance_baremetal",
                ("openqa", "baremetal"),
                "http://openqa.qam.suse.cz",
            ),
            ("openqa_install_distri", ("openqa", "distri"), "sle"),
            (
                "openqa_install_logs",
                ("openqa", "install_logfile"),
                "update_install-zypper.log",
            ),
            (
                "openqa_kernel_install_logs",
                ("openqa", "kernel_install_logfile"),
                "update_kernel-zypper.log",
            ),
            # config for template export
            ("threshold", ("template", "smelt_threshold"), 10, int, self.config.getint),
            # process location last as that needs to access
            # RefhostsFactory which need access to parts of config.
            ("location", ("mtui", "location"), "default"),
        ]

        def add_normalizer(x):
            return x if len(x) > 3 else x + (normalizer,)

        data = (add_normalizer(x) for x in data)

        getter = self.config.get

        def add_getter(x):
            return x if len(x) > 4 else x + (getter,)

        data = [add_getter(x) for x in data]
        self.data = data

    def _has_option(self, opt):
        """
        :return True: if opt is valid option name
        """
        return opt in (x[0] for x in self.data)

    def set_option(self, opt, val):
        """
        :returns: None
        :raises: InvalidOptionNameError if opt is not valid option name

        Warning: this method is not type safe. You need to take care to
            pass proper type as the value.
            where by type safe is meant that the value is not passed
            through normalizer defined for the option.
        """
        # FIXME: ^ remove warning (add type safety)
        if not self._has_option(opt):
            raise InvalidOptionNameError()

        setattr(self, opt, val)

    def _handle_testopia_cred(self):
        if not self.use_keyring:
            logger.debug("keyring disabled by configuration")
            return

        try:
            import keyring
        except ImportError:
            logger.warning("keyring library not available")
            return

        logger.debug("querying keyring for Testopia password")
        if self.testopia_pass and self.testopia_user:
            try:
                keyring.set_password("Testopia", self.testopia_user, self.testopia_pass)
            except Exception:
                logger.warning("failed to add Testopia password to the keyring")
                logger.debug(format_exc())
        elif self.testopia_user:
            try:
                self.testopia_pass = keyring.get_password(
                    "Testopia", self.testopia_user
                )
            except Exception:
                logger.warning("failed to get Testopia password from the keyring")
                logger.debug(format_exc())

        logger.debug("config.testopia_pass = {0!r}".format(self.testopia_pass))

    def _list_terms(self):
        scripts = [x.name[5:-3] for x in self.datadir.glob("term.*.sh")]
        self.termnames = scripts

    def _get_option(self, secopt, getter):
        """
        :type secopt: 2-tuple
        :param secopt: (section, option)
        """
        try:
            return getter(*secopt)
        except (configparser.NoSectionError, configparser.NoOptionError):
            msg = "Config option {0}.{1} not found."
            logger.debug(msg.format(*secopt))
            raise
        except Exception:
            msg = "Config option {0}.{1} extraction from {2} " + "failed."
            logger.error(msg.format(secopt + (self.configfiles,)))
            raise

    def merge_args(self, args):
        """
        Merges argv config overrides into the config instance

        :param args: parsed argv:
        :type args: L{argparse.Namespace}
        """

        if args.location:
            self.location = args.location

        if args.template_dir:
            self.template_dir = args.template_dir

        if args.connection_timeout:
            self.connection_timeout = args.connection_timeout

        if args.smelt_api:
            self.smelt_api = args.smelt_api
