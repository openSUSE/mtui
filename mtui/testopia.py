# -*- coding: utf-8 -*-
#
# manage connection to Testopia
#

import re
import logging
import HTMLParser

from mtui.config import *
from mtui.connector.bugzilla import *

out = logging.getLogger('mtui')


class Testopia(object):
    """Managing Testopia testcases

    Interface to the Testopia XMLRPC API documented at
    https://wiki.mozilla.org/Testopia:Documentation:XMLRPC

    """

    # product to testplan maps
    plans = { '9':'251', '10':'263,351', '11':'2672' }

    def __init__(self, product=None, packages=None):
        """create xmlrpclib.ServerProxy object for communication

        creates a ServerProxy XMLRPC instance with Testopie credentials

        Keyword arguments:
        None

        """

        self.testcases = {}
        self.product = product
        self.packages = packages

        interface = config.testopia_interface
        username = config.testopia_user
        password = config.testopia_pass

        out.debug('creating Testopia Interface at %s' % interface)
        self.bugzilla = Bugzilla(interface, username, password)

        # cache testcases since Testopia is slow
        self.update_testcase_list()

    def _replace_html(self, text):
        parser = HTMLParser.HTMLParser()

        text = parser.unescape(text)
        text = re.sub('<br>', '\n', text)
        text = re.sub('</span>', '\n', text)
        text = re.sub('</div>', '\n', text)
        text = re.sub('&nbsp;', ' ', text)
        text = re.sub('<[^>]*>', '', text)

        return text

    def update_testcase_list(self):
        out.debug('updating Testopia testcase list')
        self.testcases = self.get_testcase_list(self.product, self.packages)

    def get_testcase_list(self, product, packages):
        """queries package testcases

        search for package testcases for the update product

        Keyword arguments:
        packages -- list of package names to search for
        product  -- SUSE product to query for (9, 10, 11)

        """

        cases = {}

        try:
            assert(product and packages)
        except AssertionError:
            return {}

        out.debug('getting testcase list for packages %s' % packages)
        tags = ','.join([ 'packagename_%s' % i for i in packages ])

        try:
            response = self.bugzilla.query_interface('TestCase.list', {'tags':tags, 'tags_type':'anyexact', 'plan_id':self.plans[product]})
        except Exception:
            out.debug('failed to get a XMLRPC response')
            return {}

        # since we're too lazy to copy testcases over to our latest products,
        # fall back to the old ones if none were found
        if not response:
            response = self.bugzilla.query_interface('TestCase.list', {'tags':tags, 'tags_type':'anyexact', 'plan_id':self.plans['10']})
            if response:
                out.warning('found testcases for product 10 while %s was empty' % product)
                out.warning('please consider migrating the testcases to product %s' % product)

        for case in response:
            cases[case['case_id']] = case['summary']

        return cases

    def get_testcase(self, case_id):
        """queries testopia testcase actions

        show testcase actions

        Keyword arguments:
        case_id  -- Testopia testcase ID

        """

        try:
            assert(case_id)
        except AssertionError:
            return []

        try:
            response = self.bugzilla.query_interface('TestCase.get', case_id)
        except Exception:
            out.debug('failed to get a XMLRPC response')
            return []

        try:
            testcase = {'requirement':response['requirement'],
                        'action':self._replace_html(response['text']['action']),
                        'breakdown':self._replace_html(response['text']['breakdown']),
                        'setup':self._replace_html(response['text']['setup']),
                        'summary':self._replace_html(response['summary'])}
        except KeyError:
            out.error('testcase %s not found' % case_id)
            return []

        return testcase

