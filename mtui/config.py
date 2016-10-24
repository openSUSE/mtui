# -*- coding: utf-8 -*-
#
# mtui config file parser and default values
#

import os
import getpass
try:
    import configparser
except ImportError:
    import ConfigParser as configparser

from traceback import format_exc
from mtui.refhost import RefhostsFactory
from mtui.messages import InvalidLocationError

try:
    import keyring
except ImportError:
    # disable keyring support since python-keyring is missing
    keyring = None

class InvalidOptionNameError(RuntimeError):
    pass

class Config(object):
    """Read and store the variables from mtui config files"""
    # FIXME: change str paths to L{filepath.FilePath}

    def __init__(self, logger, refhosts = RefhostsFactory, paths = None):
        self.log = logger
        self.refhosts = refhosts
        self._location = 'default'


        try:
            # FIXME: gotta read config overide from env instead of argv
            # because this crap is used as a singleton all over the
            # place
            self.configfiles = [os.environ['MTUI_CONF']]
        except KeyError:
            self.configfiles = paths or [
              '/etc/mtui.cfg'
            , os.path.expanduser('~/.mtuirc')
            ]
        self.read()

        self._define_config_options()
        self._parse_config()
        self._handle_testopia_cred()

    def read(self):
        self.config = configparser.SafeConfigParser()
        try:
            self.config.read(self.configfiles)
        except configparser.Error as e:
            self.log.error(e)

    @property
    def location(self):
        return self._location

    @location.setter
    def location(self, x):
        try:
            self.refhosts(self, self.log).check_location_sanity(x)
        except InvalidLocationError as e:
            self.log.error(e)
            return

        self._location = x

    def _parse_config(self):
        for datum in self.data:
            attr, inipath, default, fixup, getter = datum

            try:
                val = self._get_option(inipath, getter)
            except:
                if callable(default):
                    val = default()
                else:
                    val = default

            setattr(self, attr, fixup(val))
            self.log.debug('config.%s set to "%s"' % (attr, val))

    def _define_config_options(self):
        normalizer = lambda x: x

        data = [
            ('datadir', ('mtui', 'datadir'),
             lambda: os.path.dirname(os.path.dirname(__file__)),
             os.path.expanduser),

            ('template_dir', ('mtui', 'template_dir'),
             lambda: os.path.expanduser(os.getenv('TEMPLATE_DIR', '.')),
             os.path.expanduser),

            ('local_tempdir', ('mtui', 'tempdir'),
             lambda: os.path.expanduser(os.getenv('TMPDIR', '/tmp')),
             os.path.expanduser),

            ('session_user', ('mtui', 'user'),
             getpass.getuser),

            ('use_repose', ('mtui', 'use_repose'),
                True, bool, self.config.getboolean),

            # connection.timeout appears to be in units of seconds as
            # indicated by
            # http://www.lag.net/paramiko/docs/paramiko.Channel-class.html#gettimeout
            ('connection_timeout', ('mtui', 'connection_timeout'),
             300, int),

            ('svn_path', ('svn', 'path'),
             'svn+ssh://svn@qam.suse.de/testreports'),

            ('bugzilla_url', ('url', 'bugzilla'),
             'https://bugzilla.novell.com'),

            ('reports_url', ('url', 'testreports'),
             'http://qam.suse.de/testreports'),

            ('repclean_path', ('target', 'repclean'),
             '/mounts/qam/rep-clean/rep-clean.sh'),

            ('target_tempdir', ('target', 'tempdir'),
             '/tmp'),

            ('target_testsuitedir', ('target', 'testsuitedir'),
             '/usr/share/qa/tools'),

            ('testopia_interface', ('testopia', 'interface'),
             'https://apibugzilla.novell.com/xmlrpc.cgi'),

            ('testopia_user', ('testopia', 'user'), ''),
            ('testopia_pass', ('testopia', 'pass'), ''),
            ('chdir_to_template_dir', ('mtui', 'chdir_to_template_dir'),
                False, normalizer, self.config.getboolean),

            # {{{ refhosts
            ('refhosts_resolvers', ('refhosts', 'resolvers'), 'https'),

            ('refhosts_https_uri', ('refhosts', 'https_uri'),
                'https://qam.suse.de/metadata/refhosts.xml'),
            ('refhosts_https_expiration', ('refhosts',
                'https_expiration'), 3600*12, int, self.config.getint),

            ('refhosts_path', ('refhosts', 'path'),
                '/usr/share/suse-qam-metadata/refhosts.xml'),
            # }}}

            ('use_keyring', ('mtui', 'use_keyring'),
                False, bool, self.config.getboolean),

            ('report_bug_url', ('mtui', 'report_bug_url'),
                'https://bugzilla.suse.com/enter_bug.cgi?classification=40&product=Testenvironment&submit=Use+This+Product&component=MTUI'
            ),

            ('location', ('mtui', 'location'),
             'default'),
            # process location last as that needs to access
            # RefhostsFactory which need access to parts of config.
        ]

        add_normalizer = lambda x: x if len(x) > 3 \
            else x + (normalizer,)
        data = (add_normalizer(x) for x in data)

        getter = self.config.get
        add_getter = lambda x: x if len(x) > 4 else x + (getter,)
        data = [add_getter(x) for x in data]
        self.data = data

    def _has_option(self, opt):
        """
        :return True: if opt is valid option name
        """
        return opt in [x[0] for x in self.data]

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
            self.log.debug("keyring disabled by configuration")
            return

        if not keyring:
            self.log.warning("keyring library not available")
            return

        self.log.debug('querying keyring for Testopia password')
        if self.testopia_pass and self.testopia_user:
            try:
                keyring.set_password('Testopia', self.testopia_user,
                    self.testopia_pass)
            except Exception:
                self.log.warning('failed to add Testopia password to the keyring')
                self.log.debug(format_exc())
        elif self.testopia_user:
            try:
                self.testopia_pass = keyring.get_password('Testopia', self.testopia_user)
            except Exception:
                self.log.warning('failed to get Testopia password from the keyring')
                self.log.debug(format_exc())

        self.log.debug('config.testopia_pass = {0!r}'.format(
            self.testopia_pass))

    def _get_option(self, secopt, getter):
        """
        :type secopt: 2-tuple
        :param secopt: (section, option)
        """
        try:
            return getter(*secopt)
        except (configparser.NoSectionError, configparser.NoOptionError):
            msg = 'Config option {0}.{1} not found.'
            self.log.debug(msg.format(*secopt))
            raise
        except Exception:
            msg = 'Config option {0}.{1} extraction from {2} ' + \
                'failed.'
            self.log.error(msg.format(secopt + (self.configfiles, )))
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

