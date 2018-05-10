# -*- coding: utf-8 -*-
#
# manage connection to Bugzilla
#
import xmlrpc.client
from logging import getLogger

logger = getLogger("mtui.connector.bugzilla")


class Bugzilla(object):
    """Connector to the Bugzilla XMLRPC interface

    Interface to the Bugzilla XMLRPC API documented at
    http://www.bugzilla.org/docs/4.0/en/html/api/Bugzilla/WebService.html

    Interface to the Testopia XMLRPC API documented at
    https://wiki.mozilla.org/Testopia:Documentation:XMLRPC

    """

    def __init__(self, interface, username="", password=""):
        """create xmlrpclib.ServerProxy object for communication

        creates a ServerProxy XMLRPC instance with Bugzilla credentials

        Keyword arguments:
        None

        """

        # just basic auth for the start
        self.url = interface.replace(
            '://', '://{!s}:{!s}@'.format(username, password))
        self.proxy = xmlrpc.client.ServerProxy(self.url)

    def query_interface(self, service, *query):
        """generic XMLRPC interface query

        queries Bugzilla services and handles exceptions on the
        XMLRPC interface

        Keyword arguments:
        service  -- XMLRPC service to query object
        query    -- XMLRPC query

        """

        try:
            method = getattr(self.proxy, service)
            return method(*query)
        except AttributeError:
            logger.critical('service "{!s}" does not exist.'.format(service))
            raise
        except xmlrpc.client.ProtocolError as error:
            if error.errcode == 401:
                logger.critical('failed to authorize with Bugzilla')
            raise
        except xmlrpc.client.Fault as error:
            if error.faultCode == 32000:
                logger.critical('testcase does not exist')
            raise
