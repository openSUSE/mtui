# -*- coding: utf-8 -*-
#
# common Metadata parser + OBS extension

import re
from qamlib.types.obs import RequestReviewID


class MetadataParser(object):

    def parse_line(self, results, line):
        """
        :returns: bool True if line was parsed, otherwise False
        """
        match = re.search('Category: (.+)', line)
        if match:
            results.category = match.group(1)
            return True

        match = re.search('Packager: (.+)', line)
        if match:
            results.packager = match.group(1)
            return True

        match = re.search('Packages: (.+)', line)
        if match:
            results.packages = dict(
                [(pack.split()[0], pack.split()[2]) for pack in match.group(1).split(',')])
            return True

        match = re.search('Test Plan Reviewer(?:s)?: (.+)', line)
        if match:
            results.reviewer = match.group(1)
            return True

        match = re.search('Bug #(\d+) \("(.*)"\):', line)  # deprecated
        if match:
            results.bugs[match.group(1)] = match.group(2)
            return True

        match = re.search('Testplatform: (.*)', line)
        if match:
            results.testplatforms.append(match.group(1))
            return True

        match = re.search('(.*-.*) \(reference host: (\S+).*\)', line)
        if match:
            if '?' not in match.group(2):
                results.systems[match.group(2)] = match.group(1)
            return True

        match = re.search('Bugs: (.*)', line)
        if match:
            for bug in match.group(1).split(','):
                results.bugs[bug.strip(' ')] = 'Description not available'
            return True

        m = re.match('Repository: (.+)', line)
        if m:
            results.repository = m.group(1)
            return True

        return False


class OBSMetadataParser(MetadataParser):

    def parse_line(self, results, line):
        if super(OBSMetadataParser, self).parse_line(results, line):
            return True

        m = re.match('Rating: (.+)', line)
        if m:
            results.rating = m.group(1)
            return True

        m = re.match('ReviewRequestID: (.+)', line)
        if m:
            results.rrid = RequestReviewID(m.group(1))
            return True
