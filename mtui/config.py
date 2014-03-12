# -*- coding: utf-8 -*-
#
# mtui config file parser and default values
#

import os
import getpass
import logging
import ConfigParser
from mtui import __version__

try:
    import keyring
except ImportError:
    # disable keyring support since python-keyring is missing
    keyring = None

out = logging.getLogger('mtui')


class Config(object):

    """Read and store the variables from mtui config files"""

    def __init__(self):
        try:
            # FIXME: gotta read config overide from env instead of argv
            # because this crap is used as a singleton all over the
            # place
            self.configfiles = [os.environ['MTUI_CONF']]
        except KeyError:
            self.configfiles = [
                os.path.join('/', 'etc', 'mtui.cfg'),
                os.path.expanduser('~/.mtuirc')
            ]

        self.config = ConfigParser.SafeConfigParser()
        try:
            self.config.read(self.configfiles)
        except ConfigParser.Error:
            pass


        normalizer = lambda x: x

        data = [
            ('datadir', ('mtui', 'datadir'),
             lambda: os.path.dirname(os.path.dirname(__file__)),
             os.path.expanduser),

            ('template_dir', ('mtui', 'templatedir'),
             lambda: os.path.expanduser(os.getenv('TEMPLATEDIR', '.')),
             os.path.expanduser),

            ('refhosts_xml', ('mtui', 'refhosts'),
             lambda: os.path.join(self.datadir, 'refhosts.xml'),
             lambda path: os.path.join(self.datadir, path)),

            ('local_tempdir', ('mtui', 'tempdir'),
             '/tmp'),

            ('session_user', ('mtui', 'user'),
             getpass.getuser),

            ('location', ('mtui', 'location'),
             'default'),

            ('interface_version', ('mtui', 'interface_version'),
             __version__),

            ('connection_timeout', ('connection', 'timeout'),
             300, int),

            ('svn_path', ('svn', 'path'),
             'svn+ssh://svn@qam.suse.de/testreports'),

            ('patchinfo_url', ('url', 'patchinfo'),
             'http://hilbert.nue.suse.com/abuildstat/patchinfo'),

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
             'https://apibugzilla.novell.com/tr_xmlrpc.cgi'),

            ('testopia_user', ('testopia', 'user'), ''),
            ('testopia_pass', ('testopia', 'pass'), ''),
            ('chdir_to_templatedir', ('mtui', 'chdir_to_templatedir'),
                False, normalizer, self.config.getboolean),
        ]

        add_normalizer = lambda x: x if len(x) > 3 \
            else x + (normalizer,)
        data = (add_normalizer(x) for x in data)

        getter = self.config.get
        add_getter = lambda x: x if len(x) > 4 else x + (getter,)
        data = (add_getter(x) for x in data)

        for datum in data:
            attr, inipath, default, fixup, getter = datum

            try:
                val = self._get_option(inipath, getter)
            except:
                if callable(default):
                    val = default()
                else:
                    val = default

            setattr(self, attr, fixup(val))
            out.debug('config.%s set to "%s"' % (attr, val))

        if keyring is not None:
            out.debug('querying keyring for Testopia password')
            if self.testopia_pass and self.testopia_user:
                try:
                    keyring.set_password('Testopia', self.testopia_user, self.testopia_pass)
                except Exception:
                    out.warning('failed to add Testopia password to the keyring')
            elif self.testopia_user:
                try:
                    self.testopia_pass = keyring.get_password('Testopia', self.testopia_user)
                except Exception:
                    out.warning('failed to get Testopia password from the keyring')

        out.debug('config.testopia_pass set to "%s"' % self.testopia_pass)

    def _get_option(self, secopt, getter):
        """
        :type secopt: 2-tuple
        :param secopt: (section, option)
        """
        try:
            return getter(*secopt)
        except (ConfigParser.NoSectionError, ConfigParser.NoOptionError):
            msg = 'Config option {0}.{1} not found.'
            out.debug(msg.format(*secopt))
            raise
        except Exception:
            msg = 'Config option {0}.{1} extraction from {2} ' + \
                'failed.'
            out.error(msg.format(secopt + (self.configfiles, )))
            raise

config = Config()
