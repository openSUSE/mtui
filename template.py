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

    def __init__(self, md5, location='default', directory=None):
        """open and parse maintenance template file

        Keyword arguments:
        md5       -- md5 checksum of patchinfo
        location  -- reference host location name
        directory -- QA maintenance update directory

        """

        self.md5 = md5

        if directory is not None:
            self.path = directory + '/' + md5 + '/'
        else:
            self.path = './' + md5 + '/'

        self.path = self.path + 'log'

        self.metadata = Metadata()
        self.metadata.md5 = md5
        self.metadata.path = self.path
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

            match = re.search('Bug #(\d+) \("(.*)"\):', line)  # deprecated
            if match:
                self.metadata.bugs[match.group(1)] = match.group(2)

            match = re.search('Testplatform: (.*)', line)
            if match:
                hosts = self.get_refhosts_from_testplatform(match.group(1))
                self.metadata.systems.update(hosts)

            match = re.search('(.*-.*) \(reference host: (\S+).*\)', line)
            if match:
                if '?' not in match.group(2):
                    self.metadata.systems[match.group(2)] = match.group(1)

            match = re.search('Bugs: (.*)', line)
            if match:
                for bug in match.group(1).split(','):
                    self.metadata.bugs[bug.strip(' ')] = 'Description not available'

    def get_refhosts_from_system(self, system):
        """get refhost from system name

        parses refhost mapping file to get testing machine

        Keyword arguments:
        system -- requested system name

        """

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

    def get_refhosts_from_testplatform(self, testplatform):
        """get refhost from testplatform string

        parses refhost mapping file to get testing machines

        Keyword arguments:
        testplatform -- requested testplatform string

        """

        hosts = {}
        refhost = Refhost(os.path.dirname(__file__) + '/' + 'refhosts.xml', self.metadata.location)

        try:
            try:
                refhost.set_attributes_from_testplatform(testplatform)
                hostnames = refhost.search()
            except (ValueError, KeyError):
                hostnames = []
                out.error('failed to parse Testplatform string')
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


