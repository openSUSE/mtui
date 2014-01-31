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
            ('testopia_pass', ('testopia', 'pass'), '')
        ]

        asis = lambda x: x

        for datum in data:
            try:
                attr, inipath, default, fixup = datum
            except ValueError:
                (attr, inipath, default), fixup = datum, asis

            try:
                setattr(self, attr, fixup(self._get_option(*inipath)))
            except Exception:
                try:
                    d = default()
                except TypeError:
                    d = default
                setattr(self, attr, d)
            out.debug('config.%s set to "%s"' % (attr, getattr(self, attr)))

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

    def _get_option(self, section, option):
        try:
            return self.config.get(section, option)
        except (ConfigParser.NoSectionError, ConfigParser.NoOptionError):
            out.debug('[%s]->%s not found. falling back to default.' % (section, option))
            raise
        except ConfigParser.Error:
            out.error('failed to parse config files %s. falling back to default.' % self.configfiles)
            raise

config = Config()

