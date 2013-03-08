# -*- coding: utf-8 -*-
#
# manage connection to Testopia
#

import xmlrpclib

from config import *

out = logging.getLogger('mtui')

class Testopia(object):
    """Connector to the Testopia XMLRPC interface

    Interface to the Testopia XMLRPC API documented at
    https://wiki.mozilla.org/Testopia:Documentation:XMLRPC

    """

    plans = { '9':'251', '10':'263,351', '11':'2672' }

    def __init__(self):
        """create xmlrpclib.ServerProxy object for communication

        creates a ServerProxy XMLRPC instance with Testopie credentials

        Keyword arguments:
        None

        """

        self.url = config.testopia_interface.replace('://', '://%s:%s@' % (config.testopia_user, config.testopia_pass))
        self.proxy = xmlrpclib.ServerProxy(self.url);

    def _query_interface(self, service, query):
        """generic XMLRPC interface query

        queries Testopia services and handles exceptions on the
        XMLRPC interface

        Keyword arguments:
        service  -- XMLRPC service to query object
        query    -- XMLRPC query

        """

        try:
            return service(query)
        except xmlrpclib.ProtocolError as error:
            if error.errcode == 401:
                out.critical('failed to authorize with Testopia')

        return []

    def get_testcase_list(self, packages, product):
        """queries package package testcases

        search for package testcases for the update product

        Keyword arguments:
        packages -- list of package names to search for
        product  -- SUSE product to query for (9, 10, 11)

        """

        try:
            assert(packages and product)
        except AssertionError:
            return []

        tags = ','.join([ 'packagename_%s' % i for i in packages ])

        response = self._query_interface(self.proxy.TestCase.list, {'tags':tags, 'tags_type':'anyexact', 'plan_id':self.plans[product]})

        # since we're lazy to copy testcases over to our latest products,
        # fall back to the old ones if none were found
        if not response:
            response = self._query_interface(self.proxy.TestCase.list, {'tags':tags, 'tags_type':'anyexact', 'plan_id':self.plans['10']})
            if response:
                out.warning('found testcases for product 10 while %s was empty' % product)
                out.warning('please consider migrating the testcases to product %s' % product)

        cases = [ {'case_id':case['case_id'], 'summary':case['summary']} for case in response]
        return cases


