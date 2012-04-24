#!/usr/bin/python
# -*- coding: utf-8 -*-

import os
import logging
import re

from target import *
from refhost import *

out = logging.getLogger('mtui')


class Template(object):

    """input handling of QA Maintenance template file"""

    def __init__(self, md5, team=None, location='default', directory=None):
        """open and parse maintenance template file

        Keyword arguments:
        md5       -- md5 checksum of patchinfo
        team      -- team suffix (emea or asia) of template file
        location  -- reference host location name
        directory -- QA maintenance update directory

        """

        self.md5 = md5

        if directory is not None:
            self.path = directory + '/' + md5 + '/'
        else:
            self.path = './' + md5 + '/'

        if team is None:
            team = 'emea'

        self.refhosts = 'refhosts.' + team
        self.path = self.path + 'log'
        if not os.path.isfile(self.path):
            self.path = self.path + '.' + team

        self.metadata = Metadata()
        self.metadata.md5 = md5
        self.metadata.path = self.path
        if team == 'asia':
            self.metadata.location = 'beijing'
        else:
            self.metadata.location = location

        try:
            with open(self.path, 'r') as template:
                self.parse_template(template)
        except IOError, error:
            out.error('failed to open template: %s' % error.strerror)
            raise

    def parse_template(self, template):
        """parse maintenance template file

        parses metadata from QA maintenance template file

        Keyword arguments:
        template -- template file contents

        """

        platforms = {}

        for line in template.readlines():
            match = re.search('Category: (.+)', line)
            if match:
                self.metadata.category = match.group(1)

            match = re.search('YOU Patch No: (\d+)', line)
            if match:
                self.metadata.patches['you'] = match.group(1)

            match = re.search('ZYPP Patch No: (\d+)', line)
            if match:
                self.metadata.patches['zypp'] = match.group(1)

            match = re.search('SAT Patch No: (\d+)', line)
            if match:
                self.metadata.patches['sat'] = match.group(1)

            match = re.search('SUBSWAMPID: (\d+)', line)
            if match:
                self.metadata.swampid = match.group(1)

            match = re.search('Packager: (.+)', line)
            if match:
                self.metadata.packager = match.group(1)

            match = re.search('Packages: (.+)', line)
            if match:
                self.metadata.packages = dict([(pack.split()[0], pack.split()[2]) for pack in match.group(1).split(',')])

            match = re.search('Suggested Test Plan Reviewers: (.+)', line)
            if match:
                self.metadata.reviewer = match.group(1)

            match = re.search('Testplatform: (.*)', line)
            if match:
                hosts = self.get_refhosts_from_testplatform(match.group(1))
                #self.metadata.systems.update(hosts)
                platforms.update(hosts)

            match = re.search('(.*-.*) \(reference host: (.+)\)', line)
            if match:
                if '?' in match.group(2):
                    hostname = self.get_refhosts_from_system(match.group(1))
                else:
                    hostname = match.group(2)

                if hostname and hostname != '???':
                    self.metadata.systems[hostname] = match.group(1)
                else:
                    out.error('no hostname found for system %s' % match.group(1))

            match = re.search('Bug #(\d+) \("(.*)"\):', line)  # deprecated
            if match:
                self.metadata.bugs[match.group(1)] = match.group(2)

            match = re.search('Bugs: (.*)', line)
            if match:
                for bug in match.group(1).split(','):
                    self.metadata.bugs[bug.strip(' ')] = 'Description not available'

        if platforms and platforms != self.metadata.systems:
            out.error("Testplatform tags expanded to wrong hostnames. Please send this dump to ckornacker@suse.de and continue testing:")
            print "template md5: %s" % self.metadata.md5
            print "platforms:\n%s" % platforms
            print "systems:\n%s" % self.metadata.systems

    def get_refhosts_from_system(self, system):
        """get refhost from system name

        parses refhost mapping file to get testing machine

        Keyword arguments:
        system -- requested system name

        """

        """ new engine """
        try:
            refhost = Refhost(os.path.dirname(__file__) + '/' + 'refhosts.xml', self.metadata.location)

            try:
                refhost.set_attributes_from_system(system)
                host = refhost.search()[0]
            except Exception:
                out.warning("system %s not found in refhosts.xml. please report to ckornacker." % system)

        except Exception:
            import traceback
            out.critical('nonfatal error. please report to ckornacker and proceed with testing')
            traceback.print_exc()
            host = ""

        """ old engine """

        refhostfile = os.path.dirname(__file__) + '/' + self.refhosts

        try:
            with open(refhostfile, 'r') as reffile:
                for line in reffile.readlines():
                    match = re.search('%s="(.*)"' % system, line)
                    if match:
                        try:
                            assert(host != match.group(1))
                            out.critical("%s != %s for system %s. please report to ckornacker" % (host, match.group(1), system))
                        except:
                            pass
                        return match.group(1)
        except OSError, error:
            if error.errno == errno.ENOENT:
                out.warning('refhost mapping file %s not found' % self.refhosts)
            else:
                pass

    def get_refhosts_from_testplatform(self, testplatform):
        """get refhost from testplatform string

        parses refhost mapping file to get testing machines

        Keyword arguments:
        testplatform -- requested testplatform string

        """

        hosts = {}
        refhost = Refhost(os.path.dirname(__file__) + '/' + 'refhosts.xml', self.metadata.location)

        try:
            refhost.set_attributes_from_testplatform(testplatform)
            hostnames = refhost.search()
            if not hostnames:
                out.warning('nothing found for testplatform %s' % testplatform)

            for hostname in hostnames:
                system = refhost.get_host_systemname(hostname)
                hosts[hostname] = system
            return hosts
        except Exception:
            import traceback
            traceback.print_exc()
            out.warning("failed to resolve testplatform %s. please report to ckornacker." % testplatform)


